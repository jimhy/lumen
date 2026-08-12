//! M5.3 终端远程：WebSocket 传输层（axum 升级 + 单连接 socket 循环）。
//!
//! 职责边界——本模块只管**传输**：JWT 鉴权（复用 [`AuthUser`] 提取器，走
//! `Authorization` 头而非 query，避免反代日志泄漏 token）、DB 握手（取设备名、
//! 刷 `last_seen`）、WebSocket 升级、单连接 `tokio::select!` 读写循环、JSON
//! 帧编解码；所有**状态机逻辑**（配对 / 独占 / 会话）下沉 [`crate::hub::Hub`]。
//!
//! 每条连接一个 task：`select!` 在「socket 收到客户端消息」与「Hub 经 mpsc 投递
//! 出站消息」两路间多路复用，单 task 独占 socket，无需 split，临界区零 `.await`。

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use lumen_protocol::remote::{RemoteC2S, RemoteS2C};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::channel;
use tokio::sync::Notify;

use crate::auth::{self, AuthUser};
use crate::hub::ToClient;
use crate::state::AppState;

/// 单条 WS 消息上限（4 MiB）：part1 控制消息极小，留余量给 part2/3 的状态快照。
const MAX_WS_MESSAGE: usize = 4 * 1024 * 1024;
/// 单帧上限（4 MiB）：限制单个 WebSocket 帧，收口内存 DoS。
const MAX_WS_FRAME: usize = 4 * 1024 * 1024;
/// WS 连接内 `last_seen` 刷新节流（秒）：避免每个 Ping 都打库。
const LAST_SEEN_THROTTLE_SECS: i64 = 25;

/// 多久没有任何入站就发一次 WS Ping（片 11）。
///
/// # 为什么必须有（在此之前一条计时分支都没有）
///
/// 手机休眠 / 拔网线 / NAT 静默丢表项时，TCP 连接可以**在服务端这侧一直是「已连接」**
/// ——没有 FIN、没有 RST，`socket.recv()` 就那么挂着。于是 `hub.disconnect` 永远不跑，
/// 该设备在 `peers` 里成为**僵尸**：控制端看到它在线、发起控制、然后等一个永远不来的回音。
///
/// 这个值要**小于**客户端自己的保活间隔（移动端 25s Ping / 桌面端同量级），
/// 这样正常连接永远不会走到发 Ping 这一步——只有真的没动静了才发。
const IDLE_PING_AFTER: Duration = Duration::from_secs(40);

/// 发了 Ping 之后再等多久还没有任何入站，就判定死连接并断开。
///
/// **判据是「任何入站」而不是「收到 Pong」**：`Message::Pong` 由 axum/tungstenite 自动
/// 回复，但某些中间盒会把 Pong 吃掉而仍然转发数据。只要对端还在说话（Ping 帧、业务
/// 消息、甚至一个 Pong），这条连接就是活的。
const IDLE_KILL_AFTER: Duration = Duration::from_secs(20);

/// 计时分支的轮询粒度。
///
/// 用固定 tick 轮询而不是给每条连接起两个 `sleep_until`：`select!` 里每轮重建
/// timer future 会把上面两个超时**在每次收到消息时重置**，那正是我们要的语义，
/// 而固定 tick 的实现更短、也不会因为 timer 泄漏堆积。5 秒的抖动对 40/20 秒的
/// 判据无影响。
const IDLE_TICK: Duration = Duration::from_secs(5);

/// idle 检查这一拍该做什么。
///
/// 抽成枚举 + 纯函数（[`idle_action`]）而不是把判断写在 `select!` 里，是为了**能断言**：
/// 埋在异步循环里的两个阈值只能靠真的等 60 秒来验，那种测试没人会写、也没人愿意跑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleAction {
    /// 还在说话，什么都不做。
    Wait,
    /// 静默够久了，探一下。
    Ping,
    /// 探过了还是没动静，判死。
    Kill,
}

/// 按「静默多久」与「探过没有」决定这一拍的动作。
const fn idle_action(quiet: Duration, ping_sent: bool) -> IdleAction {
    if ping_sent {
        // ★ 判据是**从最后一次入站算起**的总静默时长，不是「发 Ping 之后又过了多久」。
        // 后者要额外记一个时间戳，而两者只差一个常量——少一个状态就少一处能写错的地方。
        if quiet.as_secs() >= IDLE_PING_AFTER.as_secs() + IDLE_KILL_AFTER.as_secs() {
            IdleAction::Kill
        } else {
            IdleAction::Wait
        }
    } else if quiet.as_secs() >= IDLE_PING_AFTER.as_secs() {
        IdleAction::Ping
    } else {
        IdleAction::Wait
    }
}

/// 每条连接的出站队列容量（片 11：背压）。
///
/// # 从 `unbounded` 换成有界，是因为无界的那一头连着**网络**
///
/// 出站消息由 Hub 产生（中继对端的终端输出、镜像帧…），消费速度取决于**这条 socket
/// 写得多快**。客户端网络一慢、或者干脆卡死不读，`unbounded_channel` 就会一直涨——
/// 涨到 OOM 为止，而且是**一个慢客户端拖垮整个服务端**。
///
/// 256 条的量级：M5.3 终端镜像一帧一条，256 条约等于对端卡住 8 秒（33ms/帧）。
/// 正常网络抖动远达不到，真到了就说明这条连接已经不可用了。
const OUTBOUND_QUEUE: usize = 256;

/// `GET /api/v1/ws`：远程控制 WebSocket 升级入口。
///
/// [`AuthUser`] 提取器先行完成 JWT 鉴权（失败即 401，不升级）；通过后升级并交
/// [`handle_socket`] 跑连接循环。
pub async fn ws_handler(
    State(state): State<AppState>,
    user: AuthUser,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_FRAME)
        .on_upgrade(move |socket| handle_socket(socket, state, user.user_id, user.device_id))
}

/// 单连接生命周期：DB 握手 → 登记 Hub → 发 `Welcome` → 读写循环 → 断开清理。
async fn handle_socket(mut socket: WebSocket, state: AppState, user_id: String, device_id: String) {
    // —— DB 握手：取设备名 + 刷 last_seen ——
    let name = match lookup_device_name(&state, &device_id, &user_id).await {
        Some(n) => n,
        None => {
            // token 指向的设备已不存在（被删等）：不建立 presence，关闭连接。
            tracing::warn!("WS 拒绝：设备 {device_id} 不存在或不属于该账户");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    touch_last_seen(&state, &device_id, &user_id).await;

    // —— 登记 Hub（同 device 旧连接会被驱逐）——
    //
    // 片 11：出站通道**有界**（见 OUTBOUND_QUEUE）。队列满时 Hub 塞不进关闭信号
    // （通道就是满的那个），所以另开一条 `kill` 旁路——见下面 select! 的对应分支。
    let (tx, mut rx) = channel::<ToClient>(OUTBOUND_QUEUE);
    let kill = Arc::new(Notify::new());
    let conn_id = state
        .hub
        .register(&device_id, user_id.clone(), name, tx.clone(), kill.clone());
    // 立即下发 Welcome（经 mpsc 由本循环写出）。
    let _ = tx.try_send(ToClient::Msg(Box::new(crate::hub::Hub::welcome(
        &device_id,
    ))));

    let mut last_seen_at = auth::now_secs();
    // 片 11：最后一次**入站**时刻（任何帧都算，含 Ping/Pong）。
    let mut last_inbound = Instant::now();
    // 已经发过 Ping、正在等对端出声。
    let mut ping_sent = false;
    let mut idle_ticker = tokio::time::interval(IDLE_TICK);

    // —— 读写循环 ——
    loop {
        tokio::select! {
            inbound = socket.recv() => {
                // 片 11：任何入站都证明这条连接是活的（含底层 Ping/Pong 控制帧）。
                last_inbound = Instant::now();
                ping_sent = false;
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<RemoteC2S>(text.as_str()) {
                            Ok(RemoteC2S::Ping) => {
                                // Ping 同时承担保活：节流刷 last_seen，回 Pong。
                                let now = auth::now_secs();
                                if now - last_seen_at >= LAST_SEEN_THROTTLE_SECS {
                                    last_seen_at = now;
                                    touch_last_seen(&state, &device_id, &user_id).await;
                                }
                                let _ = tx.try_send(ToClient::Msg(Box::new(RemoteS2C::Pong)));
                            }
                            // RequestControl / SubmitPairing 走专用方法（需 DB 配对信任）。
                            Ok(RemoteC2S::RequestControl { target }) => {
                                // 已配对（首次配对后）→ 跳过配对码直连（海风哥拍板）。
                                let paired = is_paired(&state, &user_id, &device_id, &target).await;
                                state.hub.request_control(&device_id, conn_id, &target, paired);
                            }
                            // M7 片 4b：手机端发起隐藏会话。**恒传 paired = false**，
                            // 即隐藏会话每次都要念配对码，绝不复用 device_pairs 里那行
                            // 为**镜像**会话建立的信任。
                            //
                            // 理由与另一半（submit_pairing 在 Hidden 分支恒返回 None、
                            // 故隐藏配对也不写 device_pairs）写在 hub.rs 的 submit_pairing 里。
                            // 一句话：device_pairs 没有会话种类列，共用一行信任会让
                            // 「手机为跟 LLM 说话念的那次码」顺带授出**镜像**权限，
                            // 反向则让老的镜像信任静默开出一条**无横幅、无指示器**的隐藏通道。
                            // 代价只是多念一次码，换掉的是一条静默提权路径。
                            //
                            // 完整方案（device_pairs 加 kind 列）落地前，**这两处必须同时成立**。
                            Ok(RemoteC2S::OpenHidden { target }) => {
                                state.hub.open_hidden(&device_id, conn_id, &target, false);
                            }
                            Ok(RemoteC2S::SubmitPairing { target, code }) => {
                                // 配对成功 → 持久化这对设备信任，之后免重输（直到一端被删）。
                                if let Some((a, b)) =
                                    state.hub.submit_pairing(&device_id, conn_id, &target, &code)
                                {
                                    persist_pair(&state, &user_id, &a, &b).await;
                                }
                            }
                            // 其余（含片 4b 的 ClientHello / RelayTo / EndHidden，它们
                            // 不需要 DB）走这条兜底臂，无需新增分支。
                            Ok(msg) => state.hub.handle(&device_id, conn_id, msg),
                            Err(e) => {
                                // 非法 JSON：不外泄消息体内容，仅记错误类型后继续读。
                                tracing::debug!("WS 消息解析失败（device={device_id}）: {e}");
                            }
                        }
                    }
                    // 二进制 / 控制帧（含底层 Ping/Pong）：part1 不使用，忽略。
                    Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        tracing::debug!("WS 读出错（device={device_id}）: {e}");
                        break;
                    }
                }
            }
            outbound = rx.recv() => {
                match outbound {
                    Some(ToClient::Msg(msg)) => {
                        let Ok(text) = serde_json::to_string(&*msg) else {
                            tracing::error!("WS 出站消息序列化失败");
                            continue;
                        };
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break; // 对端已断。
                        }
                    }
                    // 被新连接驱逐：礼貌关闭后退出。
                    Some(ToClient::Close) | None => {
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
            // 片 11：背压旁路。出站队列打满时 Hub 无法再塞任何东西进 `rx`
            // （`ToClient::Close` 也塞不进去——通道就是满的那个），只能靠这条独立信号。
            () = kill.notified() => {
                tracing::warn!("WS 出站积压超过 {OUTBOUND_QUEUE} 条，断开（device={device_id}）");
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            // 片 11：idle 判定。没有这一路，一条静默死掉的 TCP（手机休眠 / 拔网线 /
            // NAT 丢表项）会让该设备在 Hub 里成为**永久僵尸**：控制端看到它在线、
            // 发起控制、然后等一个永远不来的回音。
            _ = idle_ticker.tick() => {
                let quiet = last_inbound.elapsed();
                match idle_action(quiet, ping_sent) {
                    IdleAction::Wait => {}
                    IdleAction::Ping => {
                        // 发的是 **WebSocket 协议层** Ping，不是 `RemoteC2S::Ping`：
                        // 后者要求对端实现业务协议，而这里要探的恰恰是「对端还在不在」，
                        // 连老客户端也该能自动回。
                        if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                            break;
                        }
                        ping_sent = true;
                    }
                    IdleAction::Kill => {
                        tracing::info!(
                            "WS 静默 {quiet:?} 且 Ping 无回应，判定死连接并断开（device={device_id}）"
                        );
                        break;
                    }
                }
            }
        }
    }

    // —— 断开清理（conn_id 守卫：被驱逐的旧连接不会误删新连接状态）——
    state.hub.disconnect(&device_id, conn_id);
}

/// 取设备显示名（限定本账户）；不存在返回 `None`。
async fn lookup_device_name(state: &AppState, device_id: &str, user_id: &str) -> Option<String> {
    let client = state.pool.get().await.ok()?;
    let row = client
        .query_opt(
            "SELECT name FROM devices WHERE id = $1 AND user_id = $2",
            &[&device_id, &user_id],
        )
        .await
        .ok()??;
    Some(row.get(0))
}

/// 把两个设备 id 排序成无序对（`dev_lo <= dev_hi`），用作 `device_pairs` 主键。
fn ordered_pair<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// 查这对设备是否已持久化配对信任（首次配对后免重输）。失败/无记录均返回 false。
async fn is_paired(state: &AppState, user_id: &str, a: &str, b: &str) -> bool {
    let (lo, hi) = ordered_pair(a, b);
    let Ok(client) = state.pool.get().await else {
        return false;
    };
    client
        .query_opt(
            "SELECT 1 FROM device_pairs WHERE user_id=$1 AND dev_lo=$2 AND dev_hi=$3",
            &[&user_id, &lo, &hi],
        )
        .await
        .ok()
        .flatten()
        .is_some()
}

/// 持久化这对设备的配对信任（幂等）。失败仅记日志——下次连接退化为重输配对码。
async fn persist_pair(state: &AppState, user_id: &str, a: &str, b: &str) {
    let (lo, hi) = ordered_pair(a, b);
    let now = auth::now_secs();
    let Ok(client) = state.pool.get().await else {
        return;
    };
    if let Err(e) = client
        .execute(
            "INSERT INTO device_pairs (user_id, dev_lo, dev_hi, created_at) VALUES ($1,$2,$3,$4) \
             ON CONFLICT DO NOTHING",
            &[&user_id, &lo, &hi, &now],
        )
        .await
    {
        tracing::warn!("持久化配对信任失败: {e}");
    }
}

/// 刷新本设备 `last_seen`（与 M5.2 REST 心跳同一字段，保持 online 判定一致）。
/// 失败仅记日志、不致命——presence 仍由 Hub 内存态兜底。
async fn touch_last_seen(state: &AppState, device_id: &str, user_id: &str) {
    let now = auth::now_secs();
    let Ok(client) = state.pool.get().await else {
        return;
    };
    if let Err(e) = client
        .execute(
            "UPDATE devices SET last_seen=$1 WHERE id=$2 AND user_id=$3",
            &[&now, &device_id, &user_id],
        )
        .await
    {
        tracing::debug!("WS 刷新 last_seen 失败（device={device_id}）: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 秒 → Duration。
    const fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn 片11_正常说话的连接永远不被打扰() {
        // 这个阈值必须**大于**客户端自己的保活间隔（移动端 25s Ping），
        // 否则正常连接每分钟都会被服务端探一次，白白多一轮往返。
        assert_eq!(idle_action(s(0), false), IdleAction::Wait);
        assert_eq!(idle_action(s(25), false), IdleAction::Wait);
        assert_eq!(idle_action(IDLE_PING_AFTER - s(1), false), IdleAction::Wait);
    }

    #[test]
    fn 片11_静默够久就探一下() {
        assert_eq!(idle_action(IDLE_PING_AFTER, false), IdleAction::Ping);
        assert_eq!(idle_action(s(3600), false), IdleAction::Ping);
    }

    #[test]
    fn 片11_探过之后先等再判死() {
        // 探过了但还没到判死时刻 ⇒ 什么都不做（**不要重复发 Ping**：
        // 每 5 秒一个 tick，重复发会在死连接上刷出一串没用的帧）。
        assert_eq!(idle_action(IDLE_PING_AFTER, true), IdleAction::Wait);
        assert_eq!(
            idle_action(IDLE_PING_AFTER + IDLE_KILL_AFTER - s(1), true),
            IdleAction::Wait
        );
        assert_eq!(
            idle_action(IDLE_PING_AFTER + IDLE_KILL_AFTER, true),
            IdleAction::Kill
        );
    }

    #[test]
    fn 片11_判死总时长是两个阈值之和() {
        // 手机休眠 / 拔网线 / NAT 丢表项时，TCP 在服务端这侧可以一直是「已连接」。
        // 没有这条判定，该设备会在 Hub 里成为**永久僵尸**：控制端看到它在线、
        // 发起控制、然后等一个永远不来的回音。
        //
        // 总时长要够长，别把「地铁里钻隧道」这种正常抖动判死。
        let total = IDLE_PING_AFTER + IDLE_KILL_AFTER;
        assert!(total >= s(60), "判死太快会误杀弱网用户，实际 {total:?}");
        assert!(
            total <= s(180),
            "判死太慢，僵尸会长期占着目标，实际 {total:?}"
        );
    }
}
