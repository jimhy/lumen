//! Lumen 云服务端入口（M5.1）：账户、设备、设置/历史同步。
//!
//! 纯跨平台依赖：Windows（本地测试）与 Linux（生产发布）均可 `cargo build`。
//! 配置全走环境变量（见 [`config::Config`]），默认对接本地 docker Postgres。

#![forbid(unsafe_code)]

mod auth;
mod config;
mod db;
mod error;
mod handlers;
mod hub;
mod ssh_sync;
mod state;
// M6 P2P：极简 STUN 反射端（独立 UDP，客户端探公网映射端点做 QUIC 打洞）。
mod stun;
// 片 11：登录 / 注册节流（进程内滑动窗口）。
mod throttle;
mod ws;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, patch, post};
use axum::Router;
use lumen_protocol::routes as r;

use crate::config::Config;
use crate::hub::Hub;
use crate::state::AppState;
use crate::throttle::Throttle;

/// 后台清理周期：扫一遍未决配对，移除过期项（与配对码有效期对齐）。
const HUB_GC_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let config = Config::from_env();
    // 片 11：默认 JWT 密钥从「只 warn」升级为**拒绝启动**。
    //
    // 它是源码里公开的字符串，而本服务的 JWT 无 jti、无版本号、改密码不失效、TTL 7 天
    // ——用默认密钥等于任何人都能给任意账户签一张 7 天有效的通行证。
    // 理由与迁移步骤见 `Config::insecure_secret_refusal` 与 server/deploy/README.md。
    if let Some(reason) = config.insecure_secret_refusal() {
        // 用 error! 之后**再往 stderr 打一遍**：这条信息的读者是正在敲部署命令的人，
        // 而 LUMEN_LOG 可能把 error 也过滤掉，那样他只会看到进程一声不吭地退出。
        tracing::error!("{reason}");
        eprintln!("{reason}");
        anyhow::bail!("默认 JWT 密钥");
    }
    if config.uses_default_jwt_secret() {
        // 走到这里 = 显式设了逃生口。仍然每次启动都喊一嗓子。
        tracing::warn!(
            "⚠ 已显式放行默认 JWT 密钥（{}=1），任何人都能伪造本服务的凭据；监听 {}。切勿用于公网。",
            crate::config::ALLOW_INSECURE_SECRET_ENV,
            config.bind_addr
        );
    }
    tracing::info!("连接数据库 …");
    let pool = db::create_pool(&config.database_url)?;
    db::init_schema(&pool).await?;
    tracing::info!("数据库就绪，建表完成");

    let bind_addr = config.bind_addr.clone();
    let stun_bind = config.stun_bind_addr.clone();
    let hub = Arc::new(Hub::new());
    let throttle = Arc::new(Throttle::new());
    let state = AppState {
        pool,
        config: Arc::new(config),
        hub: hub.clone(),
        throttle: throttle.clone(),
    };
    // 后台 GC：周期清理过期未决配对（防内存泄漏 + 释放被占目标），
    // 并顺带清掉节流表里不活跃的键——那张表的键来自**未鉴权可达**的登录接口，
    // 不清就是一条随请求量无界增长的内存路径。
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(HUB_GC_INTERVAL);
        loop {
            ticker.tick().await;
            hub.gc();
            throttle.gc(auth::now_secs());
            // 把节流表的规模变成可观测的：它是一条随**未鉴权**请求量增长的内存路径，
            // 真被刷爆时这行日志是唯一的现场（GC 之后仍然很大 = 正在被持续攻击）。
            let tracked = throttle.tracked_keys();
            if tracked > 0 {
                tracing::debug!("节流表 GC 后仍跟踪 {tracked} 个键");
            }
        }
    });
    // M6 P2P STUN 反射端（独立 UDP，与中继 WS 解耦）：客户端探公网映射端点做 QUIC 打洞。
    // 绑定失败仅告警、不拖垮主服务（中继仍可用，P2P 退化为不可加速）。
    tokio::spawn(async move {
        if let Err(e) = stun::serve(&stun_bind).await {
            tracing::warn!("STUN 反射端退出（P2P 打洞将不可用，中继不受影响）: {e}");
        }
    });
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("lumen-server 已就绪 → http://{bind_addr}");
    // ★ `into_make_service_with_connect_info` 是片 11 的节流拿到 socket 对端地址的
    // 唯一途径（`ConnectInfo<SocketAddr>` 提取器）。换回裸 `app` 会让 IP 维度**静默
    // 失效**：`Option<ConnectInfo<..>>` 恒为 None ⇒ 每次都走「拿不到 IP」分支。
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// 组装路由。
fn build_router(state: AppState) -> Router {
    Router::new()
        .route(r::HEALTH, get(handlers::health))
        .route(r::REGISTER, post(handlers::register))
        .route(r::LOGIN, post(handlers::login))
        .route(r::REFRESH, post(handlers::refresh))
        .route(r::DEVICES, get(handlers::list_devices))
        .route(
            "/api/v1/devices/{id}",
            patch(handlers::rename_device).delete(handlers::delete_device),
        )
        // 片 11：配对信任的列举与撤销。
        // ⚠ `{peer}` 通配段要能匹配 `*`（清空全部）——axum 的 `{name}` 捕获单段，
        // `*` 是普通字符、不需要特殊处理。
        .route(r::PAIRS, get(handlers::list_pairs))
        .route("/api/v1/pairs/{peer}", delete(handlers::revoke_pair))
        .route(
            r::SYNC_SETTINGS,
            get(handlers::get_settings).put(handlers::put_settings),
        )
        .route(
            r::SYNC_HISTORY,
            get(handlers::pull_history).post(handlers::push_history),
        )
        .route(r::SYNC_SSH, post(ssh_sync::sync_ssh))
        .route(r::HEARTBEAT, post(handlers::heartbeat))
        // M5.3 远程控制 WebSocket 中继（升级请求无 body，下方 DefaultBodyLimit 不影响它；
        // WS 帧大小另由 ws_handler 的 max_frame_size/max_message_size 收口）。
        .route(r::WS, get(ws::ws_handler))
        .with_state(state)
        // 全局请求体上限 1 MiB，防超大 payload（DoS 面收口）。仅作用于 REST 请求体。
        .layer(DefaultBodyLimit::max(1_048_576))
}

/// 初始化日志（`LUMEN_LOG` 控制级别，默认 info）。
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("LUMEN_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}
