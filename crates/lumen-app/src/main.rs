//! Lumen 主程序：winit 事件循环，组装 PTY → 终端状态机 → 渲染器 → egui 外壳。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod action;
mod app_lock;
/// 日志落盘（stderr + `<数据目录>/lumen.log`）：GUI 构建无控制台，不落盘就没有任何现场证据。
mod applog;
mod background;
// M5 远程控制：客户端与 lumen-server 的 REST 通道 + 设备 id 持久化。
// cloud.rs 提供 M5.1–M5.4 的完整 REST API；设备列表/重命名/删除、设置/历史
// 同步等方法在 M5.2+ 才接线，故暂允许 dead_code（脚手架，非真死代码）。
#[allow(dead_code)]
mod cloud;
// 系统文件剪贴板（CF_HDROP）：文件树复制/粘贴与资源管理器互通。
mod clipboard_files;
mod virtual_files;
// M5.2 远程设备状态（心跳 + 设备列表后台线程）。
mod remote;
mod remote_mirror;
// M7 片 6：被控端把配对码同时渲染成二维码，手机扫码即完成配对。
mod remote_pairing_qr;
mod remote_ws;
// M6 P2P 直连（QUIC 打洞 + 中继回退）：tokio 隔离后台线程 + STUN 端点发现 + QUIC/证书就位。
/// 文件路径补全逻辑引擎（M4.4 批1）：token 提取 + 路径枚举，纯逻辑无 egui 依赖。
#[cfg(feature = "input-editor")]
mod completion;
/// 命令补全 sidecar 进程管理（M4.4 批2）：持久 pwsh 进程 + JSON 协议 + 异步响应唤醒。
#[cfg(feature = "input-editor")]
mod completion_sidecar;
/// footer 输入区视图组装（M4.1 批C，feature = "input-editor"）——设计稿 §7.1。
#[cfg(feature = "input-editor")]
mod composer;
/// footer 鼠标事件处理：像素→Position、click-count 状态机、词/行选区（第十一轮）。
#[cfg(feature = "input-editor")]
mod footer_mouse;
/// 命令历史库（M4.1 批D2，feature = "input-editor"）——设计稿 §8。
#[cfg(feature = "input-editor")]
mod history;
mod i18n;
mod input;
mod keymap;
/// F10 终端可点击链接：URL/文件路径识别 + 系统默认程序打开。
mod links;
#[cfg(feature = "input-editor")]
mod llm_attachments;
mod llm_cli;
mod llm_hud;
// M7 片 2：PC 端 headless LLM runner（进程 + 行解析 + 白名单 + 适配接口）。
// 片 2/片 3 只落地底座与 Claude 适配器，与远程帧的分发接线在片 4，故此刻大量公开项
// 尚无调用点——与 `mod cloud` 同样的脚手架处置（非真死代码）。
//
// **`cfg_attr(not(test), …)` 是刻意的**：allow 只在**非测试**构建里生效。
// 于是 `cargo clippy -p lumen-app --all-targets`（带 `cfg(test)`）里，
// 「既没被生产代码调用、也没被任何测试碰过」的项**仍然会报死代码**——那正是片 3/片 4
// 开发期唯一能发现「写错了、没接上」的信号。整块无条件 allow 会把这个信号一起关掉
// （3000 行的 claude.rs 全在它的覆盖范围内）。
// **片 4 接完线后请把这一行整个去掉。**
#[cfg_attr(not(test), allow(dead_code))]
mod llm_runner;
mod mode;
/// unix：从系统读 shell 进程实时 cwd（bash/zsh 无 OSC 9;9 cwd 上报时文件树的兜底）。
#[cfg(unix)]
mod os_cwd;
mod p2p;
/// 应用数据目录解析（单一真源）：按构建类型隔离 debug/release 的持久化数据。
mod paths;
/// 安装完成后的首启门闩：等安装器完全退出后再恢复终端，避免安装事务
/// 尚未收尾时并发启动 PowerShell/Scoop shim。
#[cfg(windows)]
mod post_install;
/// F7②：侧栏会话图标 = 会话内前台运行程序的 exe 图标（查不到回退字形）。
mod proc_icon;
mod profile;
mod session;
mod sessions_store;
mod settings;
mod shortcuts;
// P1 SSH：领域库存先独立落地，UI/连接/同步分批接线。
mod shell;
mod single_instance;
#[allow(dead_code)]
mod ssh;
mod ssh_runtime;
// M3.8 批2 Snap Layouts 子类化（仅 Windows）。
#[cfg(target_os = "windows")]
mod snap_layouts;
/// F3 热更（自动更新）：查 GitHub latest Release + 下载 Inno Setup 安装包。
mod update;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{error, info};
use lumen_pty::PtyEvent;
use lumen_renderer::{wgpu, Renderer};
use lumen_term::{
    encode_mouse, MouseButton as MouseReportBtn, MouseEncoding, MouseEvent, MouseEventKind,
    MouseMods, MouseProtocol, SelPoint, Selection,
};
use session::{Session, SessionId, Tab, TabId, MAX_PANES};
use shell::layout::{DividerKind, PaneLayout};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Icon, Window, WindowId};
// M3.8 自绘标题栏：Windows 平台扩展（无边框阴影 / 圆角）。
#[cfg(target_os = "windows")]
use winit::platform::windows::WindowAttributesExtWindows;

/// scrollback 容量（行）。
const SCROLLBACK: usize = 10_000;

/// 渲染静默窗口（trailing debounce）：每批 PTY 数据都把渲染往后
/// 推这么久，只有数据流静默后才上屏。TUI 程序一帧重绘往往分多次
/// write 到达，且帧尾光标常停在临时位置、之后才补「移回输入框」
/// 的序列——必须等整组数据到齐再画，否则光标/半成品行会闪烁。
/// 对打字回显是无感的延迟。
const REDRAW_DEBOUNCE: Duration = Duration::from_millis(5);
/// 最低刷新保障：数据持续不断（大量输出）时静默窗口会被一直推后，
/// 自首批未渲染数据起最多等这么久就强制渲染一次（约 30fps）。
const REDRAW_HARD_CAP: Duration = Duration::from_millis(33);
/// 绝对兜底：强制渲染时刻若恰处于 DEC 2026 同步区间会小步顺延等
/// 帧完成，但等待不超过该时长（防应用卡死在 BSU 画面冻结）。
const REDRAW_ABS_CAP: Duration = Duration::from_millis(100);
/// 光标「帧尾未归位」冻结的超时：ESU 后应用迟迟不发「显示光标」
/// 归位序列时，超过该时长就信任当前位置。
/// 打字光标走同行近距直通不受此值影响，它只兜「跨行大跳」
/// （动画残留位）——经实战验证 50ms 能盖住 codex 归位批的延迟，
/// 调小到 10ms 时 ESU 直渲下残留位会在超时后漏画（闪烁回归）。
const CURSOR_FREEZE_CAP: Duration = Duration::from_millis(50);
/// 拖选边缘 auto-scroll 的 tick 间隔：拖选时鼠标停在内容区上/下边缘外，每隔此
/// 时长滚动一行并把选区端点续到边缘（~50 行/秒）。
const AUTOSCROLL_DRAG_TICK: Duration = Duration::from_millis(20);

/// 后台会话单次 wake 的消化字节上限：`advance()` 在主线程跑，后台
/// `yes` 级输出不限量会抢占主线程拖慢前台打字。超限的事件留在**本
/// 会话自己的通道**里（靠 bounded 容量反压该会话的读线程，不连坐
/// 其他会话），并补发一个 wake 下轮继续消化。
const BG_DRAIN_CAP: usize = 256 * 1024;

/// 自定义事件：PTY 有新输出待处理（去重信号，数据在 channel 里）。
///
/// 不直接携带数据：主循环收到信号后一次 drain 全部积压字节再渲染，
/// 避免把 TUI 重绘的中间状态（光标游走、半成品行）画到屏幕上。
#[derive(Debug)]
struct PtyWake;

/// 与一个登录账号、一个服务端地址绑定的 SSH 同步会话。
///
/// token 与远程心跳/控制共享同一 `Arc`。切换账号时必须先把它原地清空，
/// 再 drop worker，令仍在途的旧账号任务即使晚一步读 token 也拿不到凭据。
struct ActiveSshSync {
    account_id: String,
    server_url: String,
    token: Arc<RwLock<String>>,
    worker: ssh::SshSyncWorker,
    /// 相同错误在退避重试期间只提示一次；成功回包后清空。
    last_failure: Option<String>,
}

fn profile_server_origin(
    profile: Option<&profile::Profile>,
    current_server_url: &str,
) -> Option<String> {
    let profile = profile?;
    let current = cloud::canonical_server_origin(current_server_url).ok()?;
    (profile.auth_origin.as_deref() == Some(current.as_str())).then_some(current)
}

fn profile_origin_requires_reauth(profile: &profile::Profile, current_server_url: &str) -> bool {
    if profile
        .token
        .as_deref()
        .is_none_or(|token| token.trim().is_empty())
    {
        return false;
    }
    let Some(saved_origin) = profile.auth_origin.as_deref() else {
        // 旧版缺字段由迁移入口处理；配置暂不可用时也必须保留档案。
        return false;
    };
    let Ok(current_origin) = cloud::canonical_server_origin(current_server_url) else {
        return false;
    };
    saved_origin != current_origin
}

fn canonical_ssh_account_id(
    profile: Option<&profile::Profile>,
    current_server_url: &str,
) -> Option<String> {
    let profile = profile?;
    let server_origin = profile_server_origin(Some(profile), current_server_url)?;
    let raw = profile.user_id.as_deref()?;
    ssh::StorageScope::account(&server_origin, raw)
        .ok()?
        .canonical_account_id()
        .ok()
        .flatten()
}

fn ssh_sync_identity(
    profile: Option<&profile::Profile>,
    server_url: &str,
) -> Option<(String, String)> {
    let profile = profile?;
    profile
        .token
        .as_deref()
        .filter(|token| !token.trim().is_empty())?;
    let server_origin = profile_server_origin(Some(profile), server_url)?;
    let account_id = canonical_ssh_account_id(Some(profile), &server_origin)?;
    Some((account_id, server_origin))
}

fn profile_auth_token(
    profile: Option<&profile::Profile>,
    current_server_url: &str,
) -> Option<Arc<RwLock<String>>> {
    let profile = profile?;
    profile_server_origin(Some(profile), current_server_url)?;
    profile
        .token
        .as_ref()
        .filter(|token| !token.trim().is_empty())
        .map(|token| Arc::new(RwLock::new(token.clone())))
}

fn clear_shared_token(token: &Arc<RwLock<String>>) {
    use zeroize::Zeroize as _;

    match token.write() {
        Ok(mut value) => value.zeroize(),
        Err(poisoned) => poisoned.into_inner().zeroize(),
    }
}

fn load_ssh_store_for_profile(
    data_root: &std::path::Path,
    profile: Option<&profile::Profile>,
    current_server_url: &str,
) -> Result<ssh::SshStore, ssh::StoreError> {
    let Some(profile) = profile else {
        return ssh::SshStore::load(data_root, ssh::StorageScope::Local);
    };
    let Some(server_origin) = profile_server_origin(Some(profile), current_server_url) else {
        return ssh::SshStore::load(data_root, ssh::StorageScope::Local);
    };
    let Some(account_id) = canonical_ssh_account_id(Some(profile), &server_origin) else {
        return ssh::SshStore::load(data_root, ssh::StorageScope::Local);
    };
    ssh::SshStore::load_account_claiming_unclaimed(data_root, &server_origin, &account_id)
}

fn should_trigger_ssh_sync_after_local_change(
    worker_account: Option<&str>,
    store_account: Option<&str>,
) -> bool {
    matches!(
        (worker_account, store_account),
        (Some(worker), Some(store)) if worker == store
    )
}

fn should_apply_ssh_sync_event(
    worker_account: Option<&str>,
    store_account: Option<&str>,
    event_account: &str,
) -> bool {
    should_trigger_ssh_sync_after_local_change(worker_account, store_account)
        && worker_account == Some(event_account)
}

fn should_continue_ssh_sync(has_more: bool, pending_mutations: usize) -> bool {
    has_more || pending_mutations > 0
}

/// M5.3 part4 本地输入优先仲裁窗口：被控端本地用户在此窗口内有过输入，则丢弃控制端
/// 转发来的远程输入（本地输入优先，海风哥拍板）。
const REMOTE_INPUT_ARBITRATION: std::time::Duration = std::time::Duration::from_millis(800);

/// M5.3 part3b 控制端镜像的离屏纹理保留 id（避开自增的会话 id，取 `u64::MAX`）。
/// 镜像 `Terminal` 复用窗格同款 wgpu 渲染器画进此 id 的离屏纹理（上色/属性/光标）。
const MIRROR_OFFSCREEN_ID: session::SessionId = u64::MAX;

/// SSH active terminal's dedicated renderer target. Keep it below the full
/// mirror reservation (`MAX`, then `MAX-1..MAX-MAX_PANES`) so no terminal
/// domain can alias another domain's texture or row cache.
const SSH_OFFSCREEN_ID: session::SessionId = MIRROR_OFFSCREEN_ID - 2 - MAX_PANES as u64;

/// part3d 被控端镜像源签名：订阅会话的 `(tab_id, 各窗格(id,行,列), 焦点下标, 最大化下标)`。
/// 变化即重发全部窗格整屏快照 + 布局（`SubscriptionStarted`）。
type MirrorSig = (
    session::TabId,
    Vec<(session::SessionId, u16, u16)>,
    u32,
    Option<u32>,
);

/// part3d 被控端「控制端接管的窗格网格」：`(订阅 tab_id, 各窗格 session_id → (行, 列))`。
/// 订阅期间该会话的网格按**控制电脑**算（`SubViewport`），本机窗口矩形不再改它。
type ControllerGrids = (session::TabId, HashMap<session::SessionId, (usize, usize)>);

/// 远程文件树根的 cwd 解析：优先 OSC 9;9 上报（Windows shell 集成注入）；
/// unix 本地 shell 无注入（`session::shell_integration_args` 仅 Windows 返回
/// 非空），退回读 shell 进程实时 cwd（与本地文件树输入同源，见 RedrawRequested
/// 的 active_cwd 兜底）。缺了它，Linux 被控端的 `term.cwd()` 恒为 None，
/// `RootChanged` 一帧都发不出，控制端远程树永远停在「等待 shell 上报路径」。
fn remote_root_cwd(tab: &session::Tab) -> Option<String> {
    let osc = tab.cwd_path();
    #[cfg(unix)]
    {
        osc.or_else(|| {
            tab.focused_pane()
                .pty
                .shell_pid()
                .and_then(os_cwd::shell_cwd)
                .map(|path| path.display().to_string())
        })
    }
    #[cfg(not(unix))]
    {
        osc
    }
}

/// part3d Phase 3c 多窗格镜像第 `i` 个窗格的离屏纹理保留 id：自 `MIRROR_OFFSCREEN_ID-1` 递减，
/// 避开自增会话 id（小）与单 mirror 的 `MIRROR_OFFSCREEN_ID`（`u64::MAX`）。`i` 上限 = `MAX_PANES`。
const fn mirror_pane_offscreen_id(i: usize) -> session::SessionId {
    MIRROR_OFFSCREEN_ID - 1 - i as u64
}

/// 从 PNG 字节流解码并构造 winit 窗口图标。
///
/// 解码失败（格式损坏、尺寸越界）时返回 `None` 并打印 warn，
/// 不 panic——图标是视觉增强，缺失不影响功能。
///
/// # Examples
///
/// ```no_run
/// let icon = load_icon(include_bytes!("../../../icons/lumen-icon-32.png"));
/// // icon 可能为 None（损坏）；正常情况下为 Some(Icon)
/// ```
fn load_icon(bytes: &[u8]) -> Option<Icon> {
    let img = match image::load_from_memory(bytes) {
        Ok(i) => i.into_rgba8(),
        Err(e) => {
            log::warn!("窗口图标解码失败，跳过设置：{e}");
            return None;
        }
    };
    let (width, height) = img.dimensions();
    match Icon::from_rgba(img.into_raw(), width, height) {
        Ok(icon) => Some(icon),
        Err(e) => {
            log::warn!("构造窗口 Icon 失败，跳过设置：{e}");
            None
        }
    }
}

fn main() -> Result<()> {
    #[cfg(windows)]
    post_install::wait_for_installer_if_requested();

    // stderr + 文件双写：release 是 windows_subsystem="windows"（无控制台），不落盘等于没日志。
    let log_path = applog::init();
    match &log_path {
        Some(p) => log::info!("日志落盘 → {}", p.display()),
        None => log::warn!("日志无法落盘（数据目录不可用），本次运行仅输出到 stderr"),
    }
    // [BUILD-MARKER] composer-IME 修复专用构建标记（坐实后移除）：日志开头
    // 出现此行 = 你跑的就是带「Ime::Enabled 即定位候选框」修复的最新版；
    // 若日志里没有这行，就是拷了旧 exe，本次测试无效。
    log::info!(
        "[BUILD-MARKER] composer-ime-fix-r4 ime-enabled-cursor-area+pos-log+title 2026-06-16"
    );
    // F8 单实例限制（事件循环创建前检测）：release 默认单开——已有
    // 实例在跑时通知其前台化、本实例静默退出；debug 构建与
    // --multi-instance / LUMEN_MULTI_INSTANCE=1 放行多开。
    // `instance` 持有命名互斥量，必须存活到 main 结束（单实例锁覆盖
    // 整个运行期）。
    let instance = single_instance::acquire();
    if matches!(instance, single_instance::InstanceCheck::AlreadyRunning) {
        info!("已有 Lumen 实例在运行，已通知其前台化，本实例退出");
        return Ok(());
    }
    let event_loop = EventLoop::<PtyWake>::with_user_event()
        .build()
        .context("创建事件循环失败")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    // 第一实例：起前台化监听线程（第二实例 SetEvent → 置标志 + 借
    // PtyWake 唤醒主循环，见 single_instance 模块文档）。
    if let single_instance::InstanceCheck::Primary(guard) = &instance {
        single_instance::spawn_foreground_listener(guard, proxy.clone());
    }
    let mut app = App { proxy, state: None };
    event_loop.run_app(&mut app).context("事件循环异常退出")?;
    Ok(())
}

struct App {
    proxy: EventLoopProxy<PtyWake>,
    state: Option<AppState>,
}

/// PTY 原始字节的人类可读转义表示（LUMEN_DUMP_PTY 取证设施，B3）。
///
/// 格式规则：
/// - 可打印 ASCII（0x20..=0x7e）原样输出；
/// - CR(`\r`)→`<CR>`、LF(`\n`)→`<LF>\n`（保留换行让文本文件可读）；
/// - ESC（0x1b）后跟 `[`：完整 CSI 序列以 `<ESC[...终止符>` 表示；
/// - ESC 后跟 `]`：完整 OSC 序列以 `<OSC...ST>` 表示（含 BEL/ST 终止）；
/// - 其余控制字符以 `<XX>` 十六进制表示。
fn dump_pty_readable(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            // 可打印 ASCII 原样输出
            0x20..=0x7e => {
                out.push(b as char);
                i += 1;
            }
            // CR
            b'\r' => {
                out.push_str("<CR>");
                i += 1;
            }
            // LF：保留一个真换行让 .txt 文件可读
            b'\n' => {
                out.push_str("<LF>\n");
                i += 1;
            }
            // BEL
            0x07 => {
                out.push_str("<BEL>");
                i += 1;
            }
            // BS
            0x08 => {
                out.push_str("<BS>");
                i += 1;
            }
            // ESC 序列
            0x1b => {
                let next = bytes.get(i + 1).copied();
                match next {
                    // CSI：ESC [ ... 终止符（0x40..=0x7e）
                    Some(b'[') => {
                        let start = i;
                        i += 2; // 跳过 ESC [
                        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                            i += 1;
                        }
                        if i < bytes.len() {
                            i += 1; // 包含终止符
                        }
                        out.push_str("<ESC");
                        for &c in &bytes[start + 1..i] {
                            if (0x20..=0x7e).contains(&c) {
                                out.push(c as char);
                            } else {
                                out.push_str(&format!("\\x{c:02x}"));
                            }
                        }
                        out.push('>');
                    }
                    // OSC：ESC ] ... BEL(0x07) 或 ST(ESC \)
                    Some(b']') => {
                        let start = i;
                        i += 2; // 跳过 ESC ]
                        loop {
                            if i >= bytes.len() {
                                break;
                            }
                            if bytes[i] == 0x07 {
                                i += 1; // BEL 终止
                                break;
                            }
                            // ST = ESC \
                            if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                        out.push_str("<OSC");
                        for &c in &bytes[start + 2..i] {
                            if c == 0x07 || (c == b'\\' && i > 0) {
                                break;
                            }
                            if (0x20..=0x7e).contains(&c) {
                                out.push(c as char);
                            } else {
                                out.push_str(&format!("\\x{c:02x}"));
                            }
                        }
                        out.push_str("...ST>");
                    }
                    // 其它 ESC 序列（ESC x）
                    Some(c) => {
                        out.push_str(&format!("<ESC{}>", c as char));
                        i += 2;
                    }
                    None => {
                        out.push_str("<ESC>");
                        i += 1;
                    }
                }
            }
            // 其余控制字符
            c => {
                out.push_str(&format!("<{c:02X}>"));
                i += 1;
            }
        }
    }
    out
}

/// 窗格 drain 的轮询顺序：激活 tab 的焦点窗格最先（回显延迟对排队
/// 最敏感），其余激活 tab 窗格次之（可见、正在渲染），最后是后台
/// tab 的窗格。`pane_counts` 为各 tab 的窗格数，`active_tab` /
/// `focused` 为激活 tab 与其焦点窗格下标；抽成纯函数便于单测，
/// 下标越界（防御）时退化为纯下标序。
fn drain_order(pane_counts: &[usize], active_tab: usize, focused: usize) -> Vec<(usize, usize)> {
    let mut order = Vec::with_capacity(pane_counts.iter().sum::<usize>());
    if let Some(&n) = pane_counts.get(active_tab) {
        if focused < n {
            order.push((active_tab, focused));
        }
        order.extend((0..n).filter(|&p| p != focused).map(|p| (active_tab, p)));
    }
    for (t, &n) in pane_counts.iter().enumerate() {
        if t == active_tab {
            continue;
        }
        order.extend((0..n).map(|p| (t, p)));
    }
    order
}

/// 面板宽度写盘判定（P10；B1 抽成纯函数加单测）：指针松开后的实际
/// 宽度 `actual` 在合法范围 ±1 容差内、且与已存值 `stored` 差 ≥1 逻辑
/// 像素才值得写盘——窗口过窄被临时压缩到范围外的瞬态宽度不写（重启
/// 还原用户最后一次主动调整的值），亚像素抖动不写（避免每帧白写）。
/// NaN/Inf（防御）一律不写。
fn width_worth_persisting(actual: f32, stored: f32, min: f32, max: f32) -> bool {
    (min - 1.0..=max + 1.0).contains(&actual) && (actual - stored).abs() >= 1.0
}

/// 最大化时窗口四边超出工作区的物理像素越界量（纯函数，单测友好）。
///
/// # 机理（M3.8 / 第十轮问题1）
///
/// Windows 无边框窗口（`WS_THICKFRAME` + `WM_NCCALCSIZE` 铺满客户区）
/// 最大化时，系统把窗口 outer rect 向四周各扩约 8px，使粗边框恰好
/// 隐藏在屏幕外——这是 VSCode/Chromium 等无边框应用的标准行为，
/// 俗称「隐形边框」。egui 按完整 `inner_size` 布局，四边各 ~8px 画在
/// 屏幕外，右/下贴边内容被裁。
///
/// 本函数比较窗口矩形与显示器工作区矩形，计算各边超出量（物理像素）：
/// - `left`  = `work.left  - win.left`（win 比 work 更偏左时为正）
/// - `top`   = `work.top   - win.top`
/// - `right` = `win.right  - work.right`（win 右端超出 work 时为正）
/// - `bottom`= `win.bottom - work.bottom`
///
/// 非最大化时窗口在工作区内，各边差值 ≤ 0，函数返回全零（0,0,0,0）。
/// 跨显示器负坐标（副显示器在主屏左侧）由 i32 算术自然处理。
///
/// # 参数
/// - `win`  : `(left, top, right, bottom)` 窗口 outer rect 物理像素（屏幕坐标）。
/// - `work` : `(left, top, right, bottom)` 显示器工作区物理像素（屏幕坐标）。
///
/// # 返回
/// `(left, top, right, bottom)` 各边越界量（物理像素，最小 0）。
fn maximized_overflow(
    win: (i32, i32, i32, i32),
    work: (i32, i32, i32, i32),
) -> (i32, i32, i32, i32) {
    let left = (work.0 - win.0).max(0);
    let top = (work.1 - win.1).max(0);
    let right = (win.2 - work.2).max(0);
    let bottom = (win.3 - work.3).max(0);
    (left, top, right, bottom)
}

/// 查询当前窗口相对所在显示器工作区的四边越界量（物理像素）。
///
/// 仅在 Windows + 最大化时实际调用；非最大化时直接返回 `(0,0,0,0)`
/// 以避免不必要的 Win32 调用。失败时静默返回 `(0,0,0,0)`（退化安全）。
///
/// # 实现说明
/// - `GetWindowRect` 取窗口 outer rect（含不可见 THICKFRAME 部分）。
/// - `MonitorFromWindow(MONITOR_DEFAULTTONEAREST)` 取所在（或最近）显示器。
/// - `GetMonitorInfoW` 取该显示器的工作区（rcWork，不含任务栏）。
/// - 二者差值由 [`maximized_overflow`] 纯函数计算（便于单测）。
// ALLOW: 此函数第十轮引入，第十一轮已确认无需在运行路径中调用（见上方注释），
// 但保留供将来如需平台相关调试用。单测覆盖靠 maximized_overflow 纯函数。
#[cfg(target_os = "windows")]
#[allow(dead_code)]
fn query_maximized_overflow(hwnd: windows_sys::Win32::Foundation::HWND) -> (i32, i32, i32, i32) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;

    // SAFETY: hwnd 由 winit 创建并由调用方保证在消息循环期间有效；
    // 两个 RECT 均以 zeroed 初始化，API 成功时完整填写，失败时
    // 我们检查返回值并返回 (0,0,0,0)，不读取未初始化内存。
    unsafe {
        let mut win_rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut win_rect) == 0 {
            return (0, 0, 0, 0);
        }

        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if hmon.is_null() {
            return (0, 0, 0, 0);
        }

        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: std::mem::zeroed(),
            rcWork: std::mem::zeroed(),
            dwFlags: 0,
        };
        if GetMonitorInfoW(hmon, &mut mi) == 0 {
            return (0, 0, 0, 0);
        }

        let win = (win_rect.left, win_rect.top, win_rect.right, win_rect.bottom);
        let work = (
            mi.rcWork.left,
            mi.rcWork.top,
            mi.rcWork.right,
            mi.rcWork.bottom,
        );
        maximized_overflow(win, work)
    }
}

/// 恢复路径各窗格的初始内容区估算（B2 修复，抽成纯函数加单测）。
///
/// spawn 发生在首帧 egui 布局之前，旧实现给所有窗格统一按**整个
/// 终端区**估算行列——多窗格布局下首帧要做腰斩级缩行 resize，恰
/// 与 shell 打印首个提示符的时间窗重叠：ConPTY/PSReadLine 跨
/// resize 的差量重绘按陈旧坐标落格，是 B2 症状②「提示符丢字 +
/// 回显错位混叠」的温床；缩行擦除则联动症状①。这里用与 shell
/// 首帧完全相同的布局引擎按还原权重预切窗格矩形、再扣窗格标题栏
/// 占高，估算与首帧实际值的偏差只剩面板像素级出入（行列 ±1 级），
/// 首帧 resize 从「腰斩」降为「微调或无」。
///
/// `area` 为终端工作区估算（逻辑点）；`maximized` 窗格按独占整区
/// 计算，其余窗格仍按布局矩形——还原最大化时回到布局矩形，届时
/// resize 近似无损。返回各窗格内容区物理像素 (宽, 高)，顺序与窗格
/// 一致；布局与 n 不符（防御）时按均分计算。
fn estimate_restored_pane_px(
    area: egui::Rect,
    layout: &PaneLayout,
    n: usize,
    maximized: Option<usize>,
    scale: f32,
) -> Vec<(u32, u32)> {
    let rects = if layout.pane_count() == n {
        layout.pane_rects(area)
    } else {
        PaneLayout::uniform(n).pane_rects(area)
    };
    rects
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let r = match maximized {
                Some(m) if m == i => area,
                _ => r,
            };
            // 与 shell/mod.rs 的窗格标题栏占高同源（极矮窗格防御：
            // 最多占一半高）。
            let title_h = shell::PANE_TITLE_HEIGHT.min(r.height() / 2.0);
            let w = (r.width() * scale).round().max(1.0) as u32;
            let h = ((r.height() - title_h) * scale).round().max(1.0) as u32;
            (w, h)
        })
        .collect()
}

/// F3：更新提示弹窗的按钮动作（egui 帧内捕获，帧后施加）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateAction {
    /// 立即下载并安装。
    Install,
    /// 前往下载页（非 Windows：自动安装是 Windows 专属，引导手动更新）。
    OpenDownload,
    /// 稍后（本次运行不再弹）。
    Later,
    /// 跳过此版本（持久化 skip_version）。
    Skip,
}

/// F10：鼠标悬停命中的可点击链接（区段 + 打开目标）。
#[derive(Debug, Clone)]
struct HoverLink {
    /// 命中所在窗格的会话 id（渲染时只给该窗格传 hover 区段）。
    pane_id: SessionId,
    /// 链接所在绝对行号（与 grid 显示坐标系一致）。
    line: u64,
    /// 链接在该行内的起始列（含）。
    start_col: usize,
    /// 链接在该行内的结束列（不含）。
    end_col: usize,
    /// 解析后的打开目标。
    target: links::LinkTarget,
}

/// F7② 会话图标纹理缓存条目：纹理 + 首抽时刻 + 是否已延迟重抽（自愈用）。
struct SessionIcon {
    /// 抽取到的纹理；`None` = 抽取失败（回退自绘字形）。
    tex: Option<egui::TextureHandle>,
    /// 首次抽取时刻（延迟重抽计时基准）。
    born: Instant,
    /// 是否已做过「进程稳定后重抽一次」（做过即定型、不再重抽）。
    refreshed: bool,
}

/// F7②-remote 图标位图内容 hash（控制端远程图标纹理缓存键；同图标多 tab
/// 共享一张纹理）。`DefaultHasher` 固定 key、跨调用确定，作缓存键足够。
fn remote_icon_hash(bm: &lumen_protocol::remote::IconBitmap) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bm.w.hash(&mut h);
    bm.h.hash(&mut h);
    bm.rgba.hash(&mut h);
    h.finish()
}

/// 被控端：从「控制端接管表」（`App::remote_pane_viewports`）里查某窗格的目标网格。
///
/// 订阅期间会话的绝对网格恒由控制端拥有（渲染内容大小按控制电脑算），故此处命中即
/// **覆盖**本机窗口矩形算出的尺寸。`None` = 未被接管（未被控 / 非订阅会话 / 该窗格还
/// 没出现在控制端上报的清单里），调用方回退按本机矩形算。
fn controller_owned_pane_grid(
    owned: Option<&ControllerGrids>,
    tab_id: session::TabId,
    session_id: session::SessionId,
) -> Option<(usize, usize)> {
    let (owned_tab, sizes) = owned?;
    if *owned_tab != tab_id {
        return None;
    }
    sizes.get(&session_id).copied()
}

/// 三种工作模式的全局快捷键。只接受精确的 Ctrl+Shift 组合，避免
/// Alt/Super 等额外修饰键参与时误切模式。
fn view_mode_shortcut(
    modifiers: ModifiersState,
    physical_key: PhysicalKey,
) -> Option<settings::ViewMode> {
    if modifiers != (ModifiersState::CONTROL | ModifiersState::SHIFT) {
        return None;
    }
    match physical_key {
        PhysicalKey::Code(KeyCode::Digit1) => Some(settings::ViewMode::Local),
        PhysicalKey::Code(KeyCode::Digit2) => Some(settings::ViewMode::Remote),
        PhysicalKey::Code(KeyCode::Digit3) => Some(settings::ViewMode::Ssh),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileTreeClipboardShortcut {
    Copy,
    Paste,
}

/// 文件树复制/粘贴必须先于 SSH 终端输入路由裁决，否则 SSH 分支会把
/// Ctrl+C / Ctrl+V 编成控制字符后提前返回，文件树快捷键永远不可达。
///
/// 只接收精确的 Ctrl+C / Ctrl+V：Ctrl+Shift+C/V 等终端组合仍交给
/// 当前终端；搜索框、重命名框或其他模态打开时由调用方把 `available`
/// 置为 false，让文本控件保留自己的复制/粘贴语义。
fn filetree_clipboard_shortcut(
    state: ElementState,
    repeat: bool,
    modifiers: ModifiersState,
    physical_key: PhysicalKey,
    available: bool,
) -> Option<FileTreeClipboardShortcut> {
    if !available
        || state != ElementState::Pressed
        || repeat
        || modifiers != ModifiersState::CONTROL
    {
        return None;
    }
    match physical_key {
        PhysicalKey::Code(KeyCode::KeyC) => Some(FileTreeClipboardShortcut::Copy),
        PhysicalKey::Code(KeyCode::KeyV) => Some(FileTreeClipboardShortcut::Paste),
        _ => None,
    }
}

/// 终端区滚动条的逐窗格几何（仅 scrollback 非空、内容区够高的可见
/// 窗格各一条）。run_ui 闭包内据此绘制轨道/滑块并处理拖动，闭包后把
/// 目标 `display_offset` 落到对应 grid。几何取自上一帧 `pane_rects_px`
/// （物理像素→逻辑点，滞后一帧无感），与「回到底部」按钮同源。
struct ScrollbarGeom {
    /// 窗格会话 id（点击/拖动后据此定位 grid）。
    sid: SessionId,
    /// 轨道矩形（逻辑点）：窗格右缘内侧、内容区高度（焦点窗格扣 footer）。
    track: egui::Rect,
    /// 滑块矩形（逻辑点）：高度 = 可视占比，位置 = 滚动进度。
    thumb: egui::Rect,
    /// scrollback 行数（拖动反算 `display_offset` 用：进度 ↔ 偏移）。
    scrollback: usize,
}

fn ssh_profile_draft(
    profile: &ssh::SshProfile,
    trusted_host_key: Option<ssh::HostKeyTrust>,
) -> ssh::NewSshProfile {
    ssh::NewSshProfile {
        name: profile.name.clone(),
        host: profile.host.clone(),
        port: profile.port,
        username: profile.username.clone(),
        auth_method: profile.auth_method,
        group_id: profile.group_id.clone(),
        initial_directory: profile.initial_directory.clone(),
        connect_timeout_secs: profile.connect_timeout_secs,
        keep_alive_secs: profile.keep_alive_secs,
        monitor_enabled: profile.monitor_enabled,
        trusted_host_key,
    }
}

fn ssh_host_key_confirmation_is_current(
    current: Option<&ssh::HostKeyTrust>,
    algorithm: &str,
    fingerprint: &str,
) -> bool {
    current
        .is_none_or(|trusted| trusted.algorithm == algorithm && trusted.fingerprint == fingerprint)
}

fn ssh_test_profile(form_id: u64, draft: &ssh::NewSshProfile) -> ssh::SshProfile {
    ssh::SshProfile {
        id: format!("ssh_{form_id:032x}"),
        name: draft.name.clone(),
        host: draft.host.clone(),
        port: draft.port,
        username: draft.username.clone(),
        auth_method: draft.auth_method,
        group_id: draft.group_id.clone(),
        sort_order: 0,
        initial_directory: draft.initial_directory.clone(),
        connect_timeout_secs: draft.connect_timeout_secs,
        keep_alive_secs: draft.keep_alive_secs,
        monitor_enabled: false,
        trusted_host_key: draft.trusted_host_key.clone(),
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

fn ssh_profile_matches_test_target(saved: &ssh::SshProfile, test: &ssh::SshProfile) -> bool {
    saved.host == test.host
        && saved.port == test.port
        && saved.username == test.username
        && saved.auth_method == test.auth_method
}

fn ssh_private_key_submission_is_valid(
    draft: &ssh::NewSshProfile,
    saved_profile: Option<&ssh::SshProfile>,
    saved_binding: Option<&ssh::SshLocalBinding>,
    selected_path: Option<&std::path::Path>,
) -> bool {
    if draft.auth_method != ssh::AuthMethod::PrivateKey {
        return true;
    }
    if let Some(path) = selected_path {
        return path.is_absolute() && path.is_file();
    }
    saved_profile.is_some_and(|profile| {
        profile.host == draft.host
            && profile.port == draft.port
            && profile.username == draft.username
            && profile.auth_method == ssh::AuthMethod::PrivateKey
            && saved_binding.is_some_and(|binding| {
                binding.profile_id == profile.id
                    && binding
                        .private_key_path
                        .as_deref()
                        .is_some_and(|path| path.is_absolute() && path.is_file())
            })
    })
}

/// 「回到底部」按钮要操作的视口所有者。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollToBottomAction {
    /// Lumen 自己的主屏 scrollback。
    LocalGrid,
    /// DECSET 1007 下由备用屏应用自己维护的视口。
    AlternateApp,
}

#[derive(Clone, Copy)]
struct ScrollToBottomTarget {
    sid: SessionId,
    rect: egui::Rect,
    action: ScrollToBottomAction,
}

#[cfg(feature = "input-editor")]
struct AttachmentOverlayItem {
    id: u64,
    label: String,
    texture: egui::TextureId,
    size: egui::Vec2,
    original_size: (u32, u32),
}

#[cfg(feature = "input-editor")]
struct AttachmentOverlay {
    session_id: SessionId,
    cli_name: &'static str,
    rect: egui::Rect,
    items: Vec<AttachmentOverlayItem>,
}

fn scroll_to_bottom_action(
    display_offset: usize,
    rows: usize,
    uses_application_alternate_scroll: bool,
    alternate_scroll_distance_hint: usize,
) -> Option<ScrollToBottomAction> {
    if display_offset > rows {
        Some(ScrollToBottomAction::LocalGrid)
    } else if uses_application_alternate_scroll && alternate_scroll_distance_hint > rows {
        Some(ScrollToBottomAction::AlternateApp)
    } else {
        None
    }
}

fn ssh_file_operation_done_message(operation: lumen_ssh::FileOperation) -> String {
    match operation {
        lumen_ssh::FileOperation::WriteText => "SSH 文件已保存".to_owned(),
        lumen_ssh::FileOperation::CreateDirectory => "SSH 文件夹已创建".to_owned(),
        lumen_ssh::FileOperation::CreateFile => "SSH 文件已创建".to_owned(),
        lumen_ssh::FileOperation::Rename => "SSH 项目已移动".to_owned(),
        lumen_ssh::FileOperation::Delete => "SSH 项目已永久删除".to_owned(),
        lumen_ssh::FileOperation::Download => "SSH 文件已下载".to_owned(),
        lumen_ssh::FileOperation::Upload => "文件已上传到 SSH 服务器".to_owned(),
    }
}

fn ssh_file_error_message(
    operation: ssh_runtime::SshFileAction,
    error: lumen_ssh::FileError,
) -> String {
    let action = match operation {
        ssh_runtime::SshFileAction::Search => "搜索 SSH 文件",
        ssh_runtime::SshFileAction::ReadText => "读取 SSH 文件",
        ssh_runtime::SshFileAction::WriteText => "保存 SSH 文件",
        ssh_runtime::SshFileAction::OpenLocalCopy => "下载并打开 SSH 文件",
        ssh_runtime::SshFileAction::CreateDirectory => "新建 SSH 文件夹",
        ssh_runtime::SshFileAction::CreateFile => "新建 SSH 文件",
        ssh_runtime::SshFileAction::Rename => "移动 SSH 项目",
        ssh_runtime::SshFileAction::Delete => "删除 SSH 项目",
        ssh_runtime::SshFileAction::Download => "下载 SSH 文件",
        ssh_runtime::SshFileAction::Upload => "上传 SSH 文件",
    };
    format!("{action}失败：{error}")
}

fn remote_edit_error_message(error: lumen_protocol::remote::EditFileError) -> String {
    use lumen_protocol::remote::EditFileError;
    match error {
        EditFileError::InvalidRequest => "远程文件编辑请求无效",
        EditFileError::NotFound => "远程文件已不存在",
        EditFileError::PermissionDenied => "没有权限编辑此远程文件",
        EditFileError::TooLarge => "文件超过内置编辑器的 1 MiB 上限",
        EditFileError::NotRegular => "只能编辑普通文本文件",
        EditFileError::Symlink => "为安全起见，内置编辑器不直接编辑符号链接",
        EditFileError::Busy => "远程文件服务正忙，请稍后重试",
        EditFileError::ChangedDuringRead => "读取过程中远程文件发生了变化，请重试",
        EditFileError::LengthMismatch | EditFileError::Integrity => "远程文件传输校验失败，请重试",
        EditFileError::Cancelled => "远程文件编辑操作已取消",
        EditFileError::StaleSession => "远程控制会话已变化，请重新打开文件",
        EditFileError::Io => "远程文件读写失败",
    }
    .to_owned()
}

fn remote_parent_path(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let separator = trimmed.rfind(['/', '\\'])?;
    if separator == 0 || (separator == 2 && trimmed.as_bytes().get(1).copied() == Some(b':')) {
        Some(&trimmed[..=separator])
    } else {
        Some(&trimmed[..separator])
    }
}

struct AppState {
    /// 性能埋点输出（LUMEN_PERF=<路径> 启用）。
    perf: Option<std::fs::File>,
    perf_t0: Instant,
    /// 最近一帧（任何内容）的渲染时刻：事件驱动重绘的 8ms 合帧下限
    /// 以它为基准（整帧负载视角，UI 帧与终端帧都算）。
    last_render_at: Option<Instant>,
    /// 最近一次**终端离屏**真正渲染的时刻：ESU 直渲的 8ms 限频以它
    /// 为基准。与 last_render_at 分开——鼠标驱动的纯 UI 重绘（同步
    /// 区间内跳过终端渲染）不该反向推迟 ESU 完成帧的上屏。
    last_term_render_at: Option<Instant>,
    window: Arc<Window>,
    renderer: Renderer,
    /// 全部 tab（每 tab 1~6 个终端窗格 = [`Session`]，见 [`Tab`]）。
    /// 至少一个；最后一个关闭即退出应用。
    tabs: Vec<Tab>,
    /// 激活 tab 在 `tabs` 中的下标。
    active_tab: usize,
    /// 会话（窗格）id 自增分配器（关闭不回收；通道随会话销毁，残留
    /// 事件无需按 id 过滤）。
    next_session_id: SessionId,
    /// tab id 自增分配器（同上，关闭不回收）。
    next_tab_id: TabId,
    /// M7：headless LLM runner 容器。**与 `tabs` 平行的独立容器**，对齐
    /// [`ssh_runtime::SshRuntime`] 的既有范式；**绝不塞进 `Tab.panes`**（七条硬伤逐条写在
    /// [`llm_runner`] 的模块文档里）。每帧由 [`AppState::pump_llm_runners`] 推进。
    llm_runners: llm_runner::LlmRunnerManager,
    /// 与转发线程共享的「wake 已挂起」标志，用于事件去重（全局一个，
    /// 任一会话的转发线程都可触发，唤醒协议与单会话时代零变化）。
    wake_pending: Arc<AtomicBool>,
    /// 事件循环唤醒句柄（补发 wake / 新建会话的转发线程用）。
    proxy: EventLoopProxy<PtyWake>,
    /// 应用设置（设置页编辑的数据源；变更即写盘）。
    settings: settings::Settings,
    /// P0 本机应用锁：独立 `app_lock.json`，不进入通用设置或云同步。
    app_lock: app_lock::AppLock,
    /// Argon2id/DPAPI 后台作业回包通道（一次只允许一项）。
    lock_crypto_tx: crossbeam_channel::Sender<app_lock::CryptoResponse>,
    lock_crypto_rx: crossbeam_channel::Receiver<app_lock::CryptoResponse>,
    /// 独立锁屏的跨帧输入/错误状态。
    lock_ui: shell::lock_ui::LockUiState,
    /// 非 Windows 的本应用输入空闲兜底；Windows 首选 GetLastInputInfo。
    last_local_input: Instant,
    /// 系统级空闲轮询节流（启用且未锁且配置自动锁定时约 1Hz）。
    next_lock_poll: Instant,
    /// 系统当前是否深色模式（P12 Sync with OS）：启动时取
    /// `window.theme()`、运行中由 `WindowEvent::ThemeChanged` 维护；
    /// 开启跟随时主题按它在深/浅槽位间解析。
    os_dark: bool,
    /// 最近一次写盘的会话列表快照（F4 持久化去重：cwd 上报/结构
    /// 变更都先与它比对，无变化不重复写盘）。None = 本次运行尚未写。
    last_sessions_snapshot: Option<sessions_store::SessionsFile>,
    /// 分隔条拖动改过比例、尚未确认落盘（B1 加固）：正常由
    /// drag_stopped → divider_drag_ended 触发落盘，但 egui 的拖动结束
    /// 事件在边角场景可能错失（拖动中窗口失焦/指针态被清）。指针
    /// 松开的帧看到此标志即兜底落盘（快照一致时内部自动跳过），
    /// 保证「拖过的比例一定进盘」。
    layout_dirty: bool,
    /// 启动首帧的「外壳布局实际应用值」日志已输出（B1 恢复面验收：
    /// 只凭加载日志不能证明 UI 真用了持久化值，首帧布局后把实际
    /// 侧栏/文件树宽与窗格权重打进日志，一次性）。
    layout_apply_logged: bool,
    /// 登录档案：None = 未登录。顶栏头像、头像菜单、设置页
    /// Account 三处 UI 同源此字段；登录写盘 / 登出删盘（profile.json）。
    profile: Option<profile::Profile>,
    /// 当前账号（未登录时为 unclaimed）的 SSH 库存单 owner。UI 与
    /// 后续同步 worker 的回包都只在主线程串行修改它。
    ssh_store: Option<ssh::SshStore>,
    /// 当前登录账号的 SSH 云同步 worker；未登录、账号/token/服务端
    /// 地址不完整或库存加载失败时为 None。
    ssh_sync: Option<ActiveSshSync>,
    /// 数据目录不可用或库存损坏时的只读空库存，保证 SSH 页面仍可打开
    /// 并展示明确错误，而不是把损坏文件覆盖成空数据。
    ssh_empty_inventory: ssh::SshInventory,
    /// SSH transports and terminal state, isolated from local PTYs and remote
    /// mirrors. Sessions remain alive while another mode or the lock is shown.
    ssh_runtime: ssh_runtime::SshRuntime,
    /// 明确发生密码认证/私钥错误的 profile。下次连接必须先让用户重输，
    /// 避免后台反复自动尝试 Credential Manager 中的错误凭据。
    ssh_force_credential_prompt: HashSet<String>,
    /// Stable egui binding for [`SSH_OFFSCREEN_ID`].
    ssh_texture: Option<egui::TextureId>,
    /// Active SSH terminal content rectangle in physical pixels.
    ssh_rect_px: Option<(f32, f32, f32, f32)>,
    modifiers: ModifiersState,
    clipboard: Option<arboard::Clipboard>,
    /// 最近一次按键时刻（端到端延迟埋点用，跟随激活会话即可）。
    last_key_at: Option<Instant>,
    /// 鼠标最近一次的窗口内像素位置。
    mouse_pos: (f64, f64),
    /// F10：鼠标当前悬停的可点击链接（None = 未悬停在链接上）。供
    /// 渲染层画 hover 下划线、egui 帧设手型光标、点击时打开。
    hovered_link: Option<HoverLink>,
    /// F10：上次做过链接探测的单元格 (窗格 id, 绝对行, 列)——鼠标仍在
    /// 同一格时跳过重复探测（避免每像素移动都做行扫描/文件系统校验）。
    hover_probe_cell: Option<(SessionId, u64, usize)>,
    /// 终端滚动条拖动态：`(会话 id, 抓取锚点)`。锚点 = 按下时指针 Y 与
    /// 滑块顶部的差值（逻辑点）——拖动中据它让滑块跟手、不跳。None =
    /// 未拖动。egui 跨帧记住被拖控件，配合 `interact_pointer_pos` 用绝对
    /// 指针位置反算偏移（不靠 delta 累加，无漂移）。
    scrollbar_drag: Option<(SessionId, f32)>,
    /// 鼠标上报态下当前按住的按键集合（索引 0=Left/1=Middle/2=Right；DECSET
    /// 1002 拖动上报需要知道哪些键在按下）。被上报的按下置位、对应释放清位、
    /// 离窗 / 失焦整体清零。用集合而非单值：多键并发时每枚按下都能配到自己的
    /// 释放，不被后按的键覆盖丢失。
    mouse_report_held: [bool; 3],
    /// 最近一次上报的 motion 格子（会话 id, 列, 行）：同格不重复上报，避免
    /// 子格抖动在 Any(1003) 模式下把 PTY 刷爆（xterm 同款节流）。带会话 id
    /// 是因为分屏多窗格共用一套视口坐标系，仅比 (列,行) 会在跨窗格落到同
    /// 一格时漏掉一次移动上报。离窗 / 失焦时清空；切焦点窗格无需额外清——
    /// 节流键含会话 id，新焦点窗格的键与旧值永不相等，首个移动不会被误吞。
    mouse_report_last_cell: Option<(SessionId, usize, usize)>,
    /// M5.3 镜像态（控制端）鼠标上报的**拖动目标镜像窗格** session id：按下时
    /// 记下，拖动 / 释放钉在它（指针捕获，坐标按其矩形夹紧），与本地
    /// `drag_report_target` 对称，但编码后经 `send_input` 转发给被控端。所有键
    /// 抬起后清 None。复用 `mouse_report_held` / `mouse_report_last_cell` 做配对与
    /// 节流（本地与镜像鼠标上报互斥——控制中处远程视图、不操作本地窗格，不会并发）。
    mirror_report_sid: Option<SessionId>,
    /// 拖选边缘 auto-scroll 方向：0=无、+1=向上滚（露出更早 scrollback）、
    /// -1=向下滚（回到更晚内容）。本地终端拖选中鼠标停在内容区上/下边缘外时
    /// 由 `CursorMoved` 置位，`about_to_wait` 据此定时滚动 + 续选。
    autoscroll_drag: i8,
    /// 拖选 auto-scroll 下次 tick 的时刻（`AUTOSCROLL_DRAG_TICK` 节流）；None=可立即 tick。
    autoscroll_at: Option<Instant>,
    /// 上次**普通**左键点击的锚点 `(窗格 id, 选择点)`：Shift+左键「范围快选」从此处
    /// 扩展选区到当前点（保留锚点）。仅普通点击更新（Shift 扩展保持原锚点，供连续
    /// Shift+点击继续扩展）；窗格 id 不符（点了别的窗格）则 Shift 退化为新建。
    last_left_click: Option<(SessionId, SelPoint)>,

    // —— F3 热更（自动更新）——
    /// 后台更新线程 → 主循环的消息通道发送端（克隆给检查/下载线程）。
    update_tx: crossbeam_channel::Sender<update::UpdateMsg>,
    /// 接收端（user_event 里 drain）。
    update_rx: crossbeam_channel::Receiver<update::UpdateMsg>,
    /// 已发现的可更新版本（检测到即记录；随后后台静默下载，见
    /// [`Self::update_ready`]）。
    update_available: Option<update::UpdateInfo>,
    /// 安装包后台静默下载进行中（Warp 式：检测到新版即静默下载，下载完成
    /// 才弹窗，不在下载期间打扰用户）。
    update_downloading: bool,
    /// 已下载就绪的安装包路径（Some = 包已下好、可直接安装）。弹窗只在
    /// **就绪后**出现（点「立即更新」直接拉起安装器、无需再等下载）。
    update_ready: Option<std::path::PathBuf>,
    /// 本次运行「稍后」已点过：暂不再弹窗（重启或新版本会再弹；已下载的
    /// 包保留在 [`Self::update_ready`]，下次弹窗即用）。
    update_dismissed: bool,
    /// auto_check 设置的原子镜像：供运行期定时检查线程读取（设置页开关
    /// 改动时同步），关闭后定时线程跳过本轮，不再打网络。
    update_auto_check: Arc<AtomicBool>,
    /// 生效代理 URL 的镜像（None=直连）：供定时检查线程读取，设置页改动时
    /// 同步刷新。与 `update_auto_check` 同模式（值为 String 故用 Mutex）。
    update_proxy: Arc<std::sync::Mutex<Option<String>>>,

    // —— egui 外壳 ——
    egui_ctx: egui::Context,
    /// 账户 JWT 的**共享可变句柄**（登录后建；心跳 worker 自动续期写回、设备心跳 + 远程控制 WS 共读
    /// 同一份，确保续期后处处用新 token，免 7 天到期后全面 401 掉线）。未登录 / 登出为 None。
    auth_token: Option<Arc<RwLock<String>>>,
    /// M5.2 远程设备状态（心跳 + 设备列表）。
    remote: remote::RemoteState,
    /// M5.3 远程控制 WS 引擎（配对 / 会话 / 数据面中继；part2a 引擎，UI part2b）。
    remote_ws: remote_ws::RemoteWs,
    /// 当前登录上下文是否已尝试恢复上次主动控制目标。只在 WS 成功启动后置位；
    /// 登出、换账号或换服务端时复位。
    remote_restore_attempted: bool,
    /// M5.3 part3d 被控端镜像源签名：订阅会话的 `(tab_id, 各窗格(id,行,列), 焦点下标, 最大化下标)`。
    /// 变化（订阅切换 / 窗格增删 / 任一窗格 resize / 切焦点 / 最大化翻转 / 分隔条拖动改尺寸）即
    /// 重发全部窗格整屏快照 + 布局（`SubscriptionStarted`）。被控期间外为 None。
    mirror_src: Option<MirrorSig>,
    /// M5.3 part3d 被控端上次发给控制端的历史边界 `(base, screen_top)`：实时输出推进
    /// 边界时变化才重发 `HistoryBounds`，使控制端 `screen_top` 近实时——否则控制端首次
    /// 上滚回看会锚到会话起始的陈旧屏位、跳到很旧的历史而非当前屏上方。
    mirror_bounds_sent: Option<(u64, u64)>,
    /// M5.3 被控端远程视口覆盖 `(行, 列)`：被控期间焦点窗格按此 resize（SSH 式跟随
    /// 控制端视图尺寸），覆盖自身窗口尺寸；非被控时 None（恢复窗口尺寸）。
    remote_viewport: Option<(usize, usize)>,
    /// M5.3 part3d 被控端「控制端接管的窗格网格」`(订阅 tab_id, 各窗格 session_id → (行, 列))`。
    ///
    /// 控制端每帧按**自己**镜像区各格像素 + **自己**的 cell 尺寸算目标网格发 `SubViewport`，被控端
    /// **不分前后台**都按此 resize，并在本机 resize 循环里持续沿用——渲染内容大小按控制电脑算，
    /// 控制端 1:1 无裁切、无留白。若不在本机循环沿用，前台 tab 下一帧就会被本机窗口矩形算出的
    /// 尺寸覆盖回去（两端抢 resize、镜像退回「按被控端排版」）。
    ///
    /// 断开 / 不再被控 / 换订阅目标 / 控制端切走远程视图即清空（下一帧本机循环按窗格矩形重算恢复）。
    remote_pane_viewports: Option<ControllerGrids>,
    /// M5.3 part4b 控制端镜像区物理像素矩形 `(x, y, w, h)`：仅控制中+远程视图时为
    /// Some（每帧由终端区矩形换算），供鼠标命中→镜像选区的像素↔单元格换算。
    mirror_rect_px: Option<(f32, f32, f32, f32)>,
    /// M5.3 part3d Phase 4 多窗格镜像各窗格**内容矩形**物理像素 `(session_id, x, y, w, h)`：每帧由
    /// `shell_out.mirror_pane_rects` 换算（按 `mirror_panes` 渲染序配对 session_id）。供鼠标命中→
    /// 点哪个窗格（点击选焦点 + per-pane 选区换算）。多窗格镜像激活时填，否则清空。
    mirror_pane_rects_px: Vec<(session::SessionId, f32, f32, f32, f32)>,
    /// M5.3 part3c-2 #7：粘贴检测到同名、等用户在覆盖模态拍板的待决下载（None = 无待决）。
    pending_paste: Option<PendingPaste>,
    /// SSH 远端项粘贴到 Lumen 本地树时的同名待决下载。SSH 传输使用
    /// 独立 SFTP actor，不能塞进 RemoteWs 的 [`PendingPaste`]。
    pending_ssh_download: Option<PendingSshDownload>,
    /// SSH 文件树的内部复制引用。远端路径不会伪装成本机 CF_HDROP；
    /// 粘贴时才经对应 SSH 会话传输。
    ssh_file_clipboard: Option<SshClipboardItem>,
    /// SSH→SSH 跨目录/跨服务器文件粘贴的临时下载后续上传。
    ssh_paste_chains: HashMap<std::path::PathBuf, SshPasteChain>,
    /// 已完成 SSH→SSH 下载、正在上传的暂存文件；仅这些路径会在上传结束后删除。
    ssh_staged_uploads: HashSet<std::path::PathBuf>,
    /// SSH 远端项导出到 Windows 文件剪贴板的当前代次。每次复制递增；
    /// 较旧代次即使稍后才下载完成，也只能清理暂存，不能反抢系统剪贴板。
    ssh_clipboard_export_generation: u64,
    /// 正在为 Windows 文件剪贴板准备的 SSH 下载：本地暂存路径 → 发起时代次。
    ssh_clipboard_exports: HashMap<std::path::PathBuf, SshClipboardExport>,
    /// 当前写进系统 CF_HDROP 的本地暂存路径。该路径必须至少保留到下一次
    /// SSH 复制成功替换系统剪贴板，否则资源管理器稍后粘贴会读到失效路径。
    ssh_clipboard_ready_path: Option<std::path::PathBuf>,
    /// 粘贴完成后待刷新的目标目录 `(is_remote, dir)`：粘贴写文件到目录后，文件树缓存未更新、新
    /// 文件不显示，故传输完成（本机复制 / 下载 / 上传）时刷新该目录。do_file_paste 设、完成点消费。
    paste_refresh: Option<(bool, String)>,
    /// 上一帧鼠标是否在文件树面板内（shell::show 报回）；保留给鼠标
    /// 焦点仲裁。Ctrl+C/V 已改用下面稳定的 TreeView 键盘焦点。
    filetree_hovered: bool,
    /// 文件树原生 TreeView 是否持有键盘焦点。快捷键以焦点为准，
    /// 不再要求鼠标持续悬停，也不会被 TreeView 自己的 egui 焦点反向阻断。
    filetree_focused: bool,
    /// 本机复制粘贴（local→local，海风哥本轮新增）在途的完成回包通道（done, skipped, errors）。
    /// `Some` = 有一次本机复制在后台 fs 递归中；后台线程复制完经此回主线程弹 toast（并 send
    /// PtyWake 唤醒主循环收包，防空闲不重绘收不到）。同时充当并发闸：在途时拒绝起新本机复制。
    local_copy_rx: Option<std::sync::mpsc::Receiver<(usize, usize, usize)>>,
    /// 片6 虚拟文件剪贴板：OLE 线程 → 主线程「把远程文件下到临时文件」请求通道（user_event
    /// 排空 → `start_clip_fetch`）。资源管理器粘贴远程虚拟文件时由 OLE 线程投递。
    clip_fetch_rx: Option<std::sync::mpsc::Receiver<remote_ws::ClipFetchCmd>>,
    /// 片6 虚拟文件剪贴板服务句柄（专用 STA OLE 线程）。`None` = 未启动 / 启动失败。
    clipboard_svc: Option<virtual_files::ClipboardService>,
    /// M5.3 part3b 控制端镜像离屏纹理的 egui 句柄（`MIRROR_OFFSCREEN_ID`，首次控制时
    /// 注册、后续复用）。控制中每帧把镜像 Terminal 渲染进它，shell 以 Image 铺满终端区。
    mirror_texture: Option<egui::TextureId>,
    /// M5.3 part3d Phase 3c 多窗格镜像各窗格离屏纹理的 egui 句柄（下标对齐
    /// `remote_ws.mirror_panes()`，离屏 id 取 `mirror_pane_offscreen_id(i)`）。退出多窗格 / 非控制
    /// 时整体释放（移入 `pending_tex_free` + drop 离屏）。
    mirror_pane_textures: Vec<egui::TextureId>,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    /// 各窗格离屏纹理的 egui 句柄（键 = 会话 id；离屏重建后原地
    /// 换绑，id 不变）。窗格关闭时移入 [`Self::pending_tex_free`]。
    pane_textures: HashMap<SessionId, egui::TextureId>,
    /// 待注销的 egui 纹理 id：窗格关闭动作可能发生在 run_ui 之后
    /// （本帧 shape 仍引用该纹理），推迟到帧呈现后统一 free。
    pending_tex_free: Vec<egui::TextureId>,
    /// F7② 会话图标——每 tab 当前前台运行程序的 exe 路径（节流轮询写入，
    /// 见 [`Self::probe_session_icons`]）；值 None = 查不到。
    session_icon_exe: HashMap<TabId, Option<std::path::PathBuf>>,
    /// F7② 会话图标纹理缓存（键 = exe 路径）。首抽可能撞上前台进程刚 spawn、
    /// 系统图标未就绪的窗口而抽到通用占位图标，故隔 [`ICON_REFRESH_DELAY`] 延迟
    /// 重抽一次覆盖（[`SessionIcon`] 的 `born`/`refreshed`），治「抽坏被永久冻结、
    /// 不自愈」。`tex: None` = 抽取失败，回退自绘字形。
    session_icon_tex: HashMap<std::path::PathBuf, SessionIcon>,
    /// F7②-remote 被控端会话图标位图缓存（键 = exe 路径）：抽成 top-down RGBA8
    /// 上线给控制端（`Arc` 免每帧 clone bytes）；值 None = 抽取失败。
    session_icon_rgba:
        HashMap<std::path::PathBuf, Option<std::sync::Arc<lumen_protocol::remote::IconBitmap>>>,
    /// F7②-remote 控制端远程图标纹理缓存（键 = 图标位图内容 hash，同图标多 tab
    /// 共享一张纹理）：把被控端传来的 RGBA 贴成本地 egui 纹理。
    remote_icon_tex: HashMap<u64, egui::TextureHandle>,
    /// LLM 草稿图片缩略图纹理，键为（会话 id，附件内部 id）。
    #[cfg(feature = "input-editor")]
    attachment_textures: HashMap<(SessionId, u64), egui::TextureHandle>,
    /// F7② 前台进程轮询的上次时刻（节流；进程快照较重，不必每帧查）。
    last_icon_probe: Option<Instant>,
    /// LLM CLI 识别轮询时刻。与侧栏图标解耦，侧栏隐藏时输入适配仍生效。
    last_llm_cli_probe: Option<Instant>,
    /// 激活 tab 各窗格的矩形（会话 id, 物理像素 x/y/w/h），来自最近
    /// 一帧 egui 布局（鼠标命中/IME 候选框定位用）。tab 结构变更后
    /// 的陈旧条目按 id 解析不到窗格、自然失效。
    pane_rects_px: Vec<(SessionId, (f32, f32, f32, f32))>,
    /// 无边框窗口边缘拖动 resize 的待发起方向：MouseInput 命中窗口外缘时
    /// 置位，下一帧 RedrawRequested 内调 drag_resize_window（窗口操作须在
    /// RedrawRequested 同帧执行，见 drag_window 处注释）。
    pending_resize_dir: Option<winit::window::ResizeDirection>,
    /// 各窗格右上角关闭按钮的命中矩形（物理像素 x/y/w/h；仅多窗格
    /// 时非空），来自最近一帧 egui 布局。raw 鼠标路由对它让位：✕ 的
    /// 点击由 egui 处理（pane_close 动作），按下不聚焦/不建选区。
    pane_close_rects_px: Vec<(f32, f32, f32, f32)>,
    /// 各分隔条命中矩形（物理像素 x/y/w/h；F7③），来自最近一帧
    /// egui 布局。raw 鼠标路由对它让位：按下不聚焦/不建选区、不交出
    /// 终端焦点（调完比例接着打字不该断流），拖动与双击由 egui 侧
    /// 处理（divider_drag / divider_reset）。
    divider_rects_px: Vec<(f32, f32, f32, f32)>,
    /// 侧栏/文件树栏拖宽手柄的命中矩形（物理像素 x/y/w/h；P10），
    /// 来自最近一帧 egui 布局。文件树右缘的手柄向终端区探入数像素，
    /// raw 鼠标路由对它让位（与分隔条同款：按下不聚焦/不建选区/不
    /// 交出终端焦点，拖宽由 egui 面板处理）。
    panel_resize_rects_px: Vec<(f32, f32, f32, f32)>,
    /// 终端是否持有键盘/IME 焦点：点击终端区 true、点击 egui 面板
    /// false。egui 不会为非控件区域持焦点，键盘与 IME 路由全靠它。
    terminal_focused: bool,
    /// egui 主动要求的下次重绘时刻（动画等），about_to_wait 里与
    /// 终端渲染计划合流取 min。事件驱动重绘触发过密（<8ms）时也
    /// 合并进此计划（见 window_event 入口的合帧下限）。
    egui_repaint_at: Option<Instant>,
    /// 上一帧是否有 egui 弹层（右键菜单/头像菜单等 Popup）打开。
    /// 用于检测「弹层关闭」边沿，按关闭方式仲裁焦点归属。
    was_popup_open: bool,
    /// 外壳 UI 的跨帧状态（重命名编辑等）。
    shell_state: shell::ShellState,
    /// B3-8：整窗 resize 事件（WindowEvent::Resized）已到达、等待本帧
    /// RedrawRequested 处理。置位时 divider_resize_held 门控对所有窗格
    /// **无效**——整窗 resize 是 OS 级行为，与分隔条拖动完全独立；
    /// 若拖动状态因焦点/指针事件丢失未被 egui 正常清除，此标志保证
    /// window resize 的 term/PTY resize 不被永久卡住。每帧 RedrawRequested
    /// 处理完窗格 resize 后即清零（单次消耗）。
    window_just_resized: bool,
    /// 背景图纹理（P13）：已成功加载时为 Some，未启用/加载失败时为 None。
    /// egui 层在终端工作区底部绘制；关闭时 free 旧纹理防泄漏。
    bg_texture: Option<background::BgTexture>,
    /// 经典直通模式开关（M4.1 批B，Ctrl+Shift+E 切换）。
    ///
    /// 置位后 [`mode::effective_mode`] 强制返回 [`mode::InputMode::Fallback`]，
    /// 所有按键直通 PTY（= M2 现状）。设计稿 §2「手动逃生」。
    /// **禁止在此字段之外的地方保存输入模式副本**（设计稿铁律）。
    force_fallback: bool,
    /// 命令历史库（M4.1 批D2）：启动时加载，提交时追加写，退出时原子重写。
    /// feature = "input-editor" 门控（Fallback/无 feature 时历史功能禁用）。
    #[cfg(feature = "input-editor")]
    history: history::HistoryStore,
    /// ghost text 缓存（M4.1 批3）：(编辑器 revision, 联想后缀)。
    /// revision 变化时重算；不变时复用上帧结果，避免每帧遍历历史库。
    /// feature = "input-editor" 门控。
    #[cfg(feature = "input-editor")]
    ghost_cache: (u64, Option<String>),
    /// 补全弹窗候选列表（M4.4 批1 + 批2）：Tab 键触发后存入，render 时构造 CompletionView。
    /// feature = "input-editor" 门控。
    #[cfg(feature = "input-editor")]
    completion_candidates: Vec<completion::Completion>,
    /// 命令补全 sidecar 进程管理器（M4.4 批2）。
    /// feature = "input-editor" 门控。
    #[cfg(feature = "input-editor")]
    completion_sidecar: completion_sidecar::CompletionSidecar,
    /// 当前在途的 sidecar 请求 id（M4.4 批2）：用于丢弃过期响应。
    /// 0 = 无在途请求（保留无效值，request 从 1 开始分配）。
    /// feature = "input-editor" 门控。
    #[cfg(feature = "input-editor")]
    completion_req_id: u64,

    // ── footer 鼠标状态机（第十一轮，feature = "input-editor"）──────────
    /// footer 区域的鼠标是否正在拖选中（左键按住未松开）。
    #[cfg(feature = "input-editor")]
    footer_dragging: bool,
    /// footer 拖选锚点（按下时记录，松开前不变）。
    #[cfg(feature = "input-editor")]
    footer_drag_anchor: lumen_editor::Position,
    /// click-count 状态机（单击/双击/三击）。
    #[cfg(feature = "input-editor")]
    footer_click_state: footer_mouse::ClickState,
    /// 右键菜单请求（含菜单弹出的窗口物理像素位置）；Some 时由 egui 帧弹出菜单。
    #[cfg(feature = "input-editor")]
    footer_context_menu_at: Option<(f64, f64)>,
}

/// 提交文本编码为 PTY 载荷（M4.1 批D1/D2）——设计稿 §3.2 步骤 2。
///
/// - 单行：`text + "\r"`
/// - 多行：**无条件**用 `"\x1b[200~" + text + "\x1b[201~\r"` 括号粘贴包裹。
///
/// # 关于多行无条件包裹（6e9635b 实测核验，D2 拍板）
///
/// 原设计草案依赖 `term.bracketed_paste()` 查询决定是否包裹，但实测
/// `term.bracketed_paste()` 始终为 `false`（PSReadLine 未发送 DEC 2004h 声明）。
/// 实测证明：PSReadLine 不声明 bracketed paste，但**确实正确处理** ESC[200~...ESC[201~
/// 序列——将其作为一整块不触发 `>>` 续行。因此改为**无条件**包裹多行：
/// 无论 `term.bracketed_paste()` 返回何值，多行提交始终用 200~/201~ 包装。
/// `bracketed_paste()` 的返回值仅作日志/取证参考，不再影响提交路径。
///
/// 此纯函数无副作用，可独立单测。
#[cfg(feature = "input-editor")]
fn encode_submit(text: &str) -> Vec<u8> {
    let line_count = text.lines().count();
    if line_count > 1 {
        // 多行：无条件括号粘贴包裹（见函数文档，6e9635b 实测核验）。
        let mut buf = Vec::with_capacity(text.len() + 14);
        buf.extend_from_slice(b"\x1b[200~");
        buf.extend_from_slice(text.as_bytes());
        buf.extend_from_slice(b"\x1b[201~\r");
        buf
    } else {
        let mut buf = text.as_bytes().to_vec();
        buf.push(b'\r');
        buf
    }
}

fn effective_session_mode(pane: &Session, force_fallback: bool) -> mode::InputMode {
    let llm_active = pane
        .llm_cli
        .or_else(|| llm_cli::detect(None, &pane.term))
        .is_some_and(|kind| llm_cli::composer_ready(&pane.term, kind));
    if llm_active {
        mode::effective_mode_for_cli(&pane.term, force_fallback, true)
    } else {
        mode::effective_mode(&pane.term, force_fallback)
    }
}

/// Kimi 的原生编辑器会在逐字收到 `/yolo` 时再次打开自己的斜杠补全；
/// 紧随其后的 Enter 会再次接受当前候选，形成 `/yoyolo`。括号粘贴走
/// Kimi 编辑器的原子 paste 路径，不触发补全，然后末尾 CR 只负责提交。
#[cfg(feature = "input-editor")]
fn encode_llm_submit(text: &str, kind: Option<llm_cli::LlmCliKind>, win32_input: bool) -> Vec<u8> {
    let mut buf = if kind == Some(llm_cli::LlmCliKind::Kimi) && text.lines().count() <= 1 {
        let mut buf = Vec::with_capacity(text.len() + 14);
        buf.extend_from_slice(b"\x1b[200~");
        buf.extend_from_slice(text.as_bytes());
        buf.extend_from_slice(b"\x1b[201~\r");
        buf
    } else {
        encode_submit(text)
    };
    if win32_input {
        debug_assert_eq!(buf.last(), Some(&b'\r'));
        buf.pop();
        buf.extend_from_slice(&input::encode_plain_enter(true));
    }
    buf
}

/// LLM CLI 的本地编辑器为空时，方向键/Esc 应留给 CLI 自己的交互
/// 选择器（如 Claude `/resume`），但 Lumen 补全弹层打开时仍由弹层接管。
#[cfg(feature = "input-editor")]
fn llm_cli_native_navigation_passthrough(
    llm_active: bool,
    editor_empty: bool,
    completion_open: bool,
    modifiers: ModifiersState,
    key: PhysicalKey,
) -> bool {
    llm_active
        && editor_empty
        && !completion_open
        && modifiers.is_empty()
        && matches!(
            key,
            PhysicalKey::Code(
                KeyCode::ArrowUp
                    | KeyCode::ArrowDown
                    | KeyCode::ArrowLeft
                    | KeyCode::ArrowRight
                    | KeyCode::Escape
            )
        )
}

/// 普通 Shell 可根据语法自动续行；LLM CLI 的 Enter 始终提交。
///
/// 附件草稿也视为 LLM 输入，避免 `[#1]` 中的 `#` 被 PowerShell 分词器当作
/// 注释，继而把 `[` 误判为未闭合表达式。
#[cfg(feature = "input-editor")]
fn composer_should_insert_newline(
    llm_active: bool,
    has_attachments: bool,
    parser_needs_continuation: bool,
) -> bool {
    !llm_active && !has_attachments && parser_needs_continuation
}

/// 将 app 层 [`action::EditAction`] 转换为 `lumen_editor::EditAction`（M4.1 批D1）。
///
/// 两个枚举结构相同；M4 阶段 app 层将直接引用 lumen_editor 的类型，届时此函数删除。
///
/// # Errors
/// 本函数不返回 Result；任何映射失败（两枚举不同步）在编译期即可发现。
#[cfg(feature = "input-editor")]
fn app_to_editor_action(ea: &action::EditAction) -> lumen_editor::EditAction {
    use action::{EditAction as AEa, Motion as AMotion};
    use lumen_editor::EditAction as EEa;
    use lumen_editor::Motion as EMotion;

    /// 将 app 层 Motion 转为 editor 层 Motion。
    fn to_motion(m: &AMotion) -> EMotion {
        match m {
            AMotion::GraphemeLeft => EMotion::GraphemeLeft,
            AMotion::GraphemeRight => EMotion::GraphemeRight,
            AMotion::WordLeft => EMotion::WordLeft,
            AMotion::WordRight => EMotion::WordRight,
            AMotion::LineStart => EMotion::LineStart,
            AMotion::LineEnd => EMotion::LineEnd,
            AMotion::Up => EMotion::Up,
            AMotion::Down => EMotion::Down,
            AMotion::DocStart => EMotion::DocStart,
            AMotion::DocEnd => EMotion::DocEnd,
        }
    }

    match ea {
        AEa::InsertText(s) => EEa::InsertText(s.clone()),
        AEa::InsertNewline => EEa::InsertNewline,
        AEa::DeleteBackward => EEa::DeleteBackward,
        AEa::DeleteForward => EEa::DeleteForward,
        AEa::DeleteWordBackward => EEa::DeleteWordBackward,
        AEa::Move { motion, extend } => EEa::Move {
            motion: to_motion(motion),
            extend: *extend,
        },
        AEa::SetSelection(s) => EEa::SetSelection(lumen_editor::Selection {
            anchor: lumen_editor::Position {
                line: s.anchor.line,
                byte: s.anchor.byte,
            },
            cursor: lumen_editor::Position {
                line: s.head.line,
                byte: s.head.byte,
            },
        }),
        AEa::SelectAll => EEa::SelectAll,
        AEa::SetText(s) => EEa::SetText(s.clone()),
        AEa::Undo => EEa::Undo,
        AEa::Redo => EEa::Redo,
        AEa::Clear => EEa::Clear,
    }
}

/// lumen_editor::EditAction → app 层 action::Action 转换（footer 鼠标路径用）。
///
/// 仅转换 footer 鼠标事件实际产生的 EditAction 变体（SetSelection / SelectAll）；
/// 其余变体若意外传入，按 SelectAll 保守兜底（不应发生，加 debug 日志）。
///
/// # Errors
/// 不返回 Result；映射失败以 debug log 标注。
#[cfg(feature = "input-editor")]
fn lumen_editor_action_to_app_action(ea: lumen_editor::EditAction) -> action::Action {
    use action::{Action, EditAction as AEa, Position as APos, Selection as ASel};
    use lumen_editor::EditAction as EEa;

    let app_ea = match ea {
        EEa::SetSelection(s) => AEa::SetSelection(ASel {
            anchor: APos {
                line: s.anchor.line,
                byte: s.anchor.byte,
            },
            head: APos {
                line: s.cursor.line,
                byte: s.cursor.byte,
            },
        }),
        EEa::SelectAll => AEa::SelectAll,
        other => {
            log::debug!("[footer_mouse] 意外的 EditAction: {other:?}，兜底 SelectAll");
            AEa::SelectAll
        }
    };
    Action::Edit(app_ea)
}

impl AppState {
    /// 性能埋点：LUMEN_PERF 启用时写一行带时间戳的记录。
    fn perf_log(&mut self, msg: std::fmt::Arguments<'_>) {
        if let Some(f) = self.perf.as_mut() {
            use std::io::Write;
            let t = self.perf_t0.elapsed().as_millis();
            let _ = writeln!(f, "[{t:>7}ms] {msg}");
        }
    }

    /// 在后台执行一次应用锁密码作业。`unlock` 用于区分失败时应回写
    /// 锁屏还是设置页；密码在调用点立即移入 `CryptoRequest` 的
    /// `Zeroizing<String>`，主线程不保留副本。
    fn spawn_lock_crypto(&mut self, request: app_lock::CryptoRequest, unlock: bool) {
        if self.app_lock.crypto_busy() {
            return;
        }
        self.app_lock.set_crypto_busy(true);
        if !app_lock::spawn_crypto(request, self.lock_crypto_tx.clone(), self.proxy.clone()) {
            self.app_lock.set_crypto_busy(false);
            if unlock {
                self.app_lock.mark_storage_error();
                self.prepare_locked_ui();
            } else {
                self.shell_state.settings.security_operation_failed();
            }
        }
        self.window.request_redraw();
    }

    /// 排空后台 Argon2id/DPAPI 结果并在主线程提交状态变更。
    fn drain_lock_crypto(&mut self) {
        let responses: Vec<_> = self.lock_crypto_rx.try_iter().collect();
        for response in responses {
            self.app_lock.set_crypto_busy(false);
            match response {
                app_lock::CryptoResponse::Enable(Ok(verifier)) => {
                    if self.app_lock.finish_enable(verifier).is_ok() {
                        self.shell_state.settings.security_operation_succeeded();
                    } else {
                        self.shell_state.settings.security_operation_failed();
                    }
                }
                app_lock::CryptoResponse::Disable(Ok(true)) => {
                    if self.app_lock.finish_disable().is_ok() {
                        self.shell_state.settings.security_operation_succeeded();
                    } else {
                        self.shell_state.settings.security_operation_failed();
                    }
                }
                app_lock::CryptoResponse::Disable(Ok(false))
                | app_lock::CryptoResponse::ChangePassword(Ok(None)) => {
                    if self.app_lock.unlock_failure(Instant::now()).is_ok() {
                        self.shell_state.settings.security_current_password_wrong();
                    } else {
                        self.app_lock.mark_storage_error();
                        self.prepare_locked_ui();
                    }
                }
                app_lock::CryptoResponse::ChangePassword(Ok(Some(verifier))) => {
                    if self.app_lock.finish_change_password(verifier).is_ok() {
                        self.shell_state.settings.security_operation_succeeded();
                    } else {
                        self.shell_state.settings.security_operation_failed();
                    }
                }
                app_lock::CryptoResponse::Unlock(Ok(true)) => {
                    if self.app_lock.unlock_success().is_ok() {
                        self.finish_app_unlock();
                    } else {
                        self.app_lock.mark_storage_error();
                        self.prepare_locked_ui();
                    }
                }
                app_lock::CryptoResponse::Unlock(Ok(false)) => {
                    if self.app_lock.unlock_failure(Instant::now()).is_ok() {
                        self.lock_ui
                            .set_error(shell::lock_ui::LockUiError::WrongPassword);
                    } else {
                        self.app_lock.mark_storage_error();
                        self.prepare_locked_ui();
                    }
                }
                app_lock::CryptoResponse::Enable(Err(_))
                | app_lock::CryptoResponse::Disable(Err(_))
                | app_lock::CryptoResponse::ChangePassword(Err(_)) => {
                    self.shell_state.settings.security_operation_failed();
                }
                app_lock::CryptoResponse::Unlock(Err(_)) => {
                    self.app_lock.mark_storage_error();
                    self.prepare_locked_ui();
                }
            }
            self.window.request_redraw();
        }
    }

    /// 进入锁屏后的本地状态清理。这里只收回本机输入与隐藏业务 UI；
    /// PTY、远程 WS、镜像订阅和文件服务都不停止。
    fn prepare_locked_ui(&mut self) {
        self.release_held_report_buttons();
        self.terminal_focused = false;
        self.last_key_at = None;
        self.hovered_link = None;
        self.hover_probe_cell = None;
        self.scrollbar_drag = None;
        self.autoscroll_drag = 0;
        self.autoscroll_at = None;
        self.shell_state.renaming = None;
        self.shell_state.pane_renaming = None;
        self.shell_state.ssh_session_renaming = None;
        self.shell_state.renaming_device = None;
        self.shell_state.history_search.open = false;
        self.shell_state.history_search.query.clear();
        self.shell_state.completion.open = false;
        self.shell_state.completion.passive = false;
        self.shell_state.login.close_for_app_lock();
        self.shell_state.settings.clear_sensitive_for_app_lock();
        self.shell_state.remote_ui.reset();
        self.shell_state.ssh_ui.close_for_app_lock();
        // 表单测试不是已建立的工作会话；锁屏关闭表单时一并丢弃其
        // 一次性 probe 与凭据副本。正式 SSH actor 仍继续后台运行。
        self.ssh_runtime.cancel_any_connection_test();
        // 丢弃一次性对话框会清零秘密，并关闭它尚未认证的会话；已经建立
        // 的 SSH actor 与远程控制会话仍继续在后台运行。
        self.discard_ssh_credential_dialog();
        self.lock_ui.clear();
        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                pane.selecting = false;
                #[cfg(feature = "input-editor")]
                {
                    pane.preedit = None;
                }
            }
        }
        #[cfg(target_os = "windows")]
        snap_layouts::update_button_rect(0, 0, 0, 0);
        app_lock::set_window_capture_protection(&self.window, true);
        self.update_window_title();
        self.window.request_redraw();
    }

    /// 由快捷键、设置页、空闲计时或系统恢复进入应用锁。
    fn lock_now(&mut self) -> bool {
        match self.app_lock.lock() {
            Ok(true) => {
                self.prepare_locked_ui();
                true
            }
            Ok(false) => false,
            Err(e) => {
                log::error!("应用锁状态写盘失败: {e}");
                self.shell_state.settings.security_operation_failed();
                self.shell_state.toast.push(
                    shell::toast::ToastKind::Error,
                    i18n::strings().security_operation_failed,
                );
                self.window.request_redraw();
                false
            }
        }
    }

    /// 密码验证且清除锁定标记成功后恢复本机 UI。锁定期间累积的 PTY
    /// 输出已持续消化；把全部窗格标记成欠帧，解锁首帧即显示最新状态。
    fn finish_app_unlock(&mut self) {
        self.lock_ui.clear();
        let now = Instant::now();
        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                pane.term_frame_due_since = Some(now.checked_sub(REDRAW_ABS_CAP).unwrap_or(now));
            }
        }
        self.last_local_input = now;
        self.terminal_focused = self.terminal_focus_allowed();
        self.update_window_title();
        app_lock::set_window_capture_protection(&self.window, false);
        self.window.request_redraw();
    }

    /// 锁定期间的唯一绘制路径。它不构造 `ShellInput`、不读取终端纹理、
    /// 账户、设备、配对或文件树，只提交完全不透明的解锁界面。
    fn render_app_lock(&mut self) -> Option<shell::lock_ui::LockUiOutput> {
        let frame = self.renderer.acquire_frame()?;
        let render_t0 = Instant::now();
        let raw_input = self.egui_state.take_egui_input(&self.window);
        let pal = shell::theme::shell_palette(settings::theme_info(
            self.settings.effective_theme_id(self.os_dark),
        ));
        let input = shell::lock_ui::LockUiInput {
            busy: self.app_lock.crypto_busy(),
            retry_remaining: self.app_lock.retry_remaining(render_t0),
            remote_active: self.remote_ws.session.is_some(),
            caps_lock: app_lock::caps_lock_on(),
            storage_error: self.app_lock.config().storage_error(),
        };
        let ctx = self.egui_ctx.clone();
        let lock_ui = &mut self.lock_ui;
        let mut lock_output = None;
        let full_output = ctx.run_ui(raw_input, |ui| {
            lock_output = Some(shell::lock_ui::show(ui, lock_ui, input, &pal));
        });

        self.egui_state
            .handle_platform_output(&self.window, full_output.platform_output);

        let ppp = self.egui_ctx.pixels_per_point();
        let clipped = self.egui_ctx.tessellate(full_output.shapes, ppp);
        let (sw, sh) = self.renderer.surface_size();
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [sw, sh],
            pixels_per_point: ppp,
        };
        let device = self.renderer.device();
        let queue = self.renderer.queue();
        for (id, delta) in &full_output.textures_delta.set {
            self.egui_renderer.update_texture(device, queue, *id, delta);
        }
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("lumen app lock frame"),
        });
        let user_cmds =
            self.egui_renderer
                .update_buffers(device, queue, &mut encoder, &clipped, &screen);
        let surface_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lumen app lock pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &surface_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.renderer.theme().background.to_wgpu()),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            self.egui_renderer.render(&mut pass, &clipped, &screen);
        }
        queue.submit(user_cmds.into_iter().chain([encoder.finish()]));
        frame.present();
        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }
        for id in self.pending_tex_free.drain(..) {
            self.egui_renderer.free_texture(&id);
        }

        let repaint_delay = full_output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(Duration::MAX, |v| v.repaint_delay);
        self.egui_repaint_at = if repaint_delay == Duration::ZERO {
            Some(render_t0 + Duration::from_millis(8))
        } else if repaint_delay < Duration::from_secs(3600) {
            Some(render_t0 + repaint_delay)
        } else {
            None
        };
        self.last_render_at = Some(render_t0);
        lock_output
    }

    /// 按设置与系统深浅模式应用当前生效主题（P12）：终端配色（含
    /// 行排版缓存失效）+ 外壳 egui 样式联动。设置页主题/槽位/Sync
    /// with OS 变更与系统深浅切换共用此链路。
    fn apply_theme(&mut self) {
        let info = settings::theme_info(self.settings.effective_theme_id(self.os_dark));
        self.renderer.set_theme(info.theme());
        let pal = shell::theme::shell_palette(info);
        // 问题5：将 panel_outline 描边色注入 renderer，更新 footer 上边框颜色。
        let [r, g, b, _] = pal.panel_outline.to_array();
        self.renderer.set_footer_border_color(r, g, b);
        shell::theme::apply_style(&self.egui_ctx, &pal);
        info!("主题已应用：{}（id {}）", info.name, info.id);
    }

    /// 加载或重载背景图纹理（P13）。
    ///
    /// - 启用且有路径：解码图片 → 上传 GPU → 更新 `bg_texture`；
    ///   路径变更时先 free 旧纹理（防泄漏），再加载新纹理。
    /// - 禁用或路径清除：free 旧纹理、置 `bg_texture = None`。
    /// - 加载失败（文件不存在/解码失败/尺寸超限）：toast error，
    ///   `bg_texture` 置 None（本次运行视为未启用，不改写 settings）。
    fn apply_background_image(&mut self) {
        let bg = &self.settings.appearance.background;
        let should_load = bg.enabled && bg.path.is_some();
        let path = bg.path.clone();

        // 先 free 旧纹理（无论是关闭还是换图）。
        if let Some(old) = self.bg_texture.take() {
            self.pending_tex_free.push(old.texture_id);
        }

        if !should_load {
            // 禁用或无路径：关闭透明通路。
            self.renderer.set_transparent_background(false);
            return;
        }

        // should_load 已保证 path.is_some()（第 427 行），此处不可为 None。
        let path_str = path
            .as_deref()
            .expect("path is Some: checked by should_load above");
        match background::load_background_texture(
            path_str,
            self.renderer.device(),
            self.renderer.queue(),
            &mut self.egui_renderer,
        ) {
            Ok(tex) => {
                log::info!("背景图已加载：{path_str} ({}×{})", tex.width, tex.height);
                self.renderer.set_transparent_background(true);
                self.bg_texture = Some(tex);
            }
            Err(e) => {
                log::error!("背景图加载失败：{e}");
                self.shell_state.toast.push(
                    shell::toast::ToastKind::Error,
                    i18n::fmt1(i18n::strings().toast_bg_load_failed_fmt, &e),
                );
                // 加载失败 = 本次运行禁用背景图（不改写 settings）。
                self.renderer.set_transparent_background(false);
                self.window.request_redraw();
            }
        }
    }

    /// 焦点窗格 = 激活 tab 的焦点窗格（键盘/IME/滚轮/选区/粘贴/块
    /// 操作的路由目标；`tabs` 恒非空——空仅出现在退出流程，调用方
    /// 已挡）。
    fn focused_pane(&self) -> &Session {
        self.tabs[self.active_tab].focused_pane()
    }

    /// 焦点窗格（可变）。
    fn focused_pane_mut(&mut self) -> &mut Session {
        self.tabs[self.active_tab].focused_pane_mut()
    }

    /// M7：推进全部 headless LLM runner（排空读线程事件 → 非阻塞收尸 → 停止宽限 /
    /// 空闲回收 → 回收已退出且读完的 runner）。
    ///
    /// **调用点约束（两条，都踩过）**：
    /// 1. 必须在 `wake_pending.store(false, SeqCst)` **之后**——否则读线程在清标志前 swap 到
    ///    `true` 而事件还没被 drain，就是一次丢唤醒。
    /// 2. 必须在 `if let Some(sub_id) = self.remote_ws.sub_target()` 那层嵌套**之外**——
    ///    headless 会话与手机订阅哪个 tab 完全无关，塞进去会让「手机切走 tab 后 LLM 事件停摆」。
    ///
    /// 片 2 把泵接上：事件在这里被排空、状态机与环形缓冲被推进（这是 §6.7「手机断线不杀
    /// 进程、事件进带 seq 的缓冲」硬契约成立的前提）。**片 4 接上了上行分发**：
    /// `remote_ws.rs` 那个显式 no-op 臂已换成 `apply_llm_frame`，两个方向都收在
    /// `RemoteWs::pump_llm` 里（见下）。返回是否有动作，供调用方决定要不要请求重绘。
    ///
    /// **本方法刻意插在 `pump_remote` 的那段长注释之前**：那段以「唯一状态变更入口」开头、
    /// 含一节「返回值」的注释是仓库既有的、挂在 `pump_remote` 头上的历史遗留块，
    /// 从它中间插一个函数会把它劈成两半、改掉既有函数的文档归属。
    fn pump_llm_runners(&mut self) -> bool {
        let events = self.llm_runners.pump();
        for (id, seq, ev) in &events {
            // `RunnerEvent` 的 `Debug` 已按 `lumen_protocol::llm` 的脱敏包装
            //（`LlmText` 只打字符数、`LlmPath` 打 `<redacted>`、`LlmToolInput` 只打字段数与
            // 字节数）产出，不会把对话正文写进日志。
            log::trace!("{id} seq={seq} {ev:?}");
        }
        // ★ M7 片 4 在 main.rs 上的**唯一一处接线**：入站 LLM 指令 → runner 操作、
        // runner 事件 → LlmFrame 上行。`remote_ws` 与 `llm_runners` 是本结构的两个
        // **不相交字段**，借用检查器允许同时可变借出。逻辑全在
        // `remote_ws/llm.rs`（见其模块文档一：为什么执行点必须在这里而不在 `apply_relay`）。
        let piped = self.remote_ws.pump_llm(&mut self.llm_runners, &events);
        piped || !events.is_empty()
    }

    /// 唯一状态变更入口（M4.1 批B）——设计稿 §6。
    ///
    /// **凡绕过此方法直接改状态的代码，code review 一律打回。**
    ///
    /// winit 事件处理层只做「事件 → keymap → Action」翻译，不直接碰
    /// editor / pty / term 状态；M4 远程消息反序列化为同一 `Action` 后
    /// 经由此方法执行，保证本地与远端行为一致。
    ///
    /// 批B 实现范围：
    /// - `Term(TermAction)` 完整实现（VT 编码下沉、写 PTY、翻屏、块跳转）。
    /// - `Edit(_)` / `Composer(_)` 仅记 debug log，批D 接编辑器时填充。
    ///
    /// # 返回值
    /// 返回 [`Vec<action::StateEvent>`]，消费方后批接（渲染 / 历史库 /
    /// 状态条 / M4 状态增量同步）。批B 仅返回 `ModeChanged` 和
    /// `FallbackToggled` 事件，其余批次逐步填充。
    /// M5.3：处理远程控制 WS（收帧 + 被控端执行远程输入 + 被控端整屏快照转发）。
    /// 在 `user_event` 调用（`PtyWake` 失焦也送达），故配对码/远程输入/镜像不再卡到
    /// 焦点回来才更新（bug2）。在 PTY drain 之前调：先发整屏快照、再由 tee 转发实时
    /// 增量，保证控制端镜像「快照先于增量」的顺序。
    fn pump_remote(&mut self) {
        if !self.remote_ws.is_running() {
            self.invalidate_stale_text_editor_source();
            let edit_events = self.remote_ws.take_edit_events();
            if !edit_events.is_empty() {
                self.apply_remote_edit_events(edit_events);
            }
            return;
        }
        let changed = self.remote_ws.poll();
        self.invalidate_stale_text_editor_source();
        // 控制端：清理停滞的在途 Fetch（超时删半成品临时文件）。仅在有事件唤醒 pump_remote
        // 时运行（传输中 FileChunk 持续唤醒、足以及时清理）；对端彻底静默时退而依赖会话结束
        // (clear) 与下次启动 (start 清目录) 兜底，临时文件不会无界堆积。
        self.remote_ws.sweep_fetch_stalls();
        let edit_events = self.remote_ws.take_edit_events();
        if !edit_events.is_empty() {
            self.apply_remote_edit_events(edit_events);
        }
        // H2：会话结束（不再控制）→ 清待决覆盖粘贴，否则覆盖模态会在死会话上复活下载。
        if !self.remote_ws.is_controlling() {
            self.pending_paste = None;
        }
        // part3d 控制端：进入远程视图且尚未订阅任何会话 → 自动订阅列表首个，使镜像立即有内容
        // （否则用户须先手动点列表项；首个=被控端侧栏顺序首位）。订阅后本分支不再触发。
        if self.remote_ws.is_controlling()
            && self.settings.layout.view_mode.is_remote()
            && self.remote_ws.subscribed_tab().is_none()
        {
            if let Some(first) = self.remote_ws.remote_tabs().first().map(|t| t.id) {
                self.remote_ws.subscribe_tab(first);
            }
        }
        // 被控端：执行控制端转发来的远程输入（Phase 4：按 (tab_id, session_id) 路由到目标窗格 PTY +
        // **per-pane** 本地输入优先仲裁）。仲裁口径：仅当远程输入目标窗格 == 被控端**当前正在打字的窗格**
        // （激活 tab 的焦点窗格）且最近有本地输入时丢弃——这样窗格 A 本地打字不会误丢发往窗格 B 的远程
        // 输入（跨窗格零干扰）。目标窗格按 id 查（非下标，关窗格 Vec 重排，D1）。
        let mut applied = false;
        let remote_input = self.remote_ws.take_input();
        if !remote_input.is_empty() {
            let local_recent = self
                .last_key_at
                .is_some_and(|t| t.elapsed() < REMOTE_INPUT_ARBITRATION);
            let local_pane = {
                let tab = &self.tabs[self.active_tab];
                (tab.id, tab.focused_pane().id)
            };
            for (tab_id, sid, bytes) in remote_input {
                if local_recent && (tab_id, sid) == local_pane {
                    log::debug!("本地输入优先：丢弃发往窗格 {sid} 的远程输入");
                    continue;
                }
                let Some(ti) = self.tabs.iter().position(|t| t.id == tab_id) else {
                    continue; // 目标会话已关，丢弃。
                };
                let Some(pi) = self.tabs[ti].panes.iter().position(|p| p.id == sid) else {
                    continue; // 目标窗格已关，丢弃。
                };
                self.tabs[ti].panes[pi].term.grid_mut().scroll_to_bottom();
                if let Err(e) = self.tabs[ti].panes[pi].write_user_input(&bytes) {
                    error!("远程输入写窗格 {sid} PTY 失败: {e:#}");
                }
                applied = true;
            }
        }
        // 被控端：执行控制端发来的远程窗格操作（需求②①：新建/关闭/最大化切换/换位），按 (tab_id,session_id)
        // 查窗格（非下标，D1）。布局/窗格集变化经 SubscriptionStarted 重发同步回控制端（两端一致）。
        for (tab_id, sid, op) in self.remote_ws.take_pane_ops() {
            use lumen_protocol::remote::PaneOpKind;
            let Some(ti) = self.tabs.iter().position(|t| t.id == tab_id) else {
                continue; // 目标会话已关。
            };
            // New 无窗格目标（在整个 tab 加格），先于按 session_id 查窗格处理（修①远程新建窗格）。
            if matches!(op, PaneOpKind::New) {
                self.new_remote_pane_in(ti);
                continue;
            }
            let Some(pi) = self.tabs[ti].panes.iter().position(|p| p.id == sid) else {
                continue; // 目标窗格已关。
            };
            match op {
                PaneOpKind::New => unreachable!("New 已在上方处理"),
                PaneOpKind::Close => {
                    // 远程关窗格；但若会关掉被控端**最后一个 tab 的最后一格**（致 app 退出），拒绝——
                    // 控制端不应远程关停被控端进程。其余情况（关多格之一 / 关多 tab 之一）正常关。
                    if self.tabs.len() == 1 && self.tabs[ti].panes.len() == 1 {
                        log::warn!("远程关窗格被拒：这是被控端最后一个窗格，不远程关停 app");
                    } else {
                        let _ = self.close_pane(ti, pi); // 非最后 tab，不退出（返回 false）。
                    }
                }
                PaneOpKind::ToggleMaximize => self.toggle_maximize_pane(ti, pi),
                PaneOpKind::SwapWith { other } => {
                    if let Some(pj) = self.tabs[ti].panes.iter().position(|p| p.id == other) {
                        // 最大化期间禁换位（同本地 pane_swap 规则）。
                        if pi != pj && self.tabs[ti].maximized.is_none() {
                            self.tabs[ti].panes.swap(pi, pj);
                            let tab = &mut self.tabs[ti];
                            if tab.focused == pi {
                                tab.focused = pj;
                            } else if tab.focused == pj {
                                tab.focused = pi;
                            }
                            self.persist_sessions(); // 窗格顺序即持久化顺序（同本地换位）。
                        }
                    }
                }
            }
        }
        // 被控端：part3d 多会话镜像——推会话列表 + 按控制端订阅的会话推焦点窗格快照
        // （被控端自身焦点不动，需求 c/e）。
        let controlled = matches!(
            self.remote_ws.session.as_ref().map(|s| s.role),
            Some(lumen_protocol::remote::Role::Controlled)
        );
        if controlled {
            // part3d Phase 2（需求 d）：先执行控制端的远程增删会话请求，使本帧 tab 列表即时反映。
            for req_id in self.remote_ws.take_new_tab_reqs() {
                if self.tabs.len() >= lumen_protocol::remote::REMOTE_MAX_SESSIONS as usize {
                    self.remote_ws.send_new_tab_result(
                        req_id,
                        None,
                        Some(lumen_protocol::remote::RemoteOpErr::LimitReached),
                    );
                } else if let Some(new_id) = self.new_tab_unfocused() {
                    self.persist_sessions(); // 不夺焦变体不落盘，结构性变更须显式落盘。
                    self.remote_ws
                        .send_new_tab_result(req_id, Some(new_id), None);
                } else {
                    self.remote_ws.send_new_tab_result(
                        req_id,
                        None,
                        Some(lumen_protocol::remote::RemoteOpErr::Io),
                    );
                }
            }
            for (req_id, tab_id) in self.remote_ws.take_close_tab_reqs() {
                if let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) {
                    if self.tabs.len() <= 1 {
                        // 拒绝关被控端最后一个会话：否则 close_tab 触发被控端退出、会话骤断。
                        log::warn!("拒绝远程关闭被控端最后一个会话 tab_id={tab_id}");
                        self.remote_ws.send_close_tab_result(
                            req_id,
                            tab_id,
                            Some(lumen_protocol::remote::RemoteOpErr::Io),
                        );
                    } else {
                        // 非最后一个 → close_tab 必返回 false（不退出）。关被控端当前焦点 tab 时
                        // close_tab 内部会切到邻位（焦点必须落到存活 tab，无法避免）；关后台 tab 不扰焦。
                        let _ = self.close_tab(idx);
                        self.remote_ws.send_close_tab_result(req_id, tab_id, None);
                    }
                } else {
                    self.remote_ws.send_close_tab_result(
                        req_id,
                        tab_id,
                        Some(lumen_protocol::remote::RemoteOpErr::NotFound),
                    );
                }
            }
            // part3d Phase 3 尺寸同步：订阅期间该会话的绝对网格**恒由控制端拥有**（SSH 式），
            // 被控端前台后台一律跟随——渲染内容大小按控制电脑算，控制端 1:1 无裁切无留白。
            // 尺寸同时记进 `remote_pane_viewports`，本机 resize 循环据此沿用（否则前台 tab
            // 下一帧就被本机窗口矩形算出的尺寸覆盖回去，两端抢 resize）。
            if let Some((vp_tab, sizes)) = self.remote_ws.take_sub_viewport() {
                if let Some(ti) = self.tabs.iter().position(|t| t.id == vp_tab) {
                    let mut resized_any = false;
                    let mut owned: HashMap<session::SessionId, (usize, usize)> =
                        HashMap::with_capacity(sizes.len());
                    for (sid, rows, cols) in sizes {
                        let (r, c) = (usize::from(rows).max(1), usize::from(cols).max(1));
                        owned.insert(sid, (r, c));
                        if let Some(pane) = self.tabs[ti].panes.iter_mut().find(|p| p.id == sid) {
                            let g = pane.term.grid();
                            if (g.rows(), g.cols()) != (r, c) {
                                pane.term.resize(r, c);
                                resized_any = true;
                                if let Err(e) = pane.pty.resize(rows.max(1), cols.max(1)) {
                                    log::warn!("订阅会话窗格 {sid} PTY resize 失败: {e:#}");
                                }
                            }
                        }
                    }
                    // 空清单 = 控制端释放接管（切走远程视图）：清表，下一帧本机按窗格矩形重算。
                    self.remote_pane_viewports = (!owned.is_empty()).then_some((vp_tab, owned));
                    if resized_any {
                        // 强制下一次镜像快照携带新几何，避免控制端短暂沿用旧尺寸。
                        self.mirror_src = None;
                    }
                    // 接管尺寸变 / 释放接管都要让本机重排一帧（被控端可能正空闲不重绘）。
                    self.window.request_redraw();
                }
            }
            // part3d Phase 3 布局比例**双向**同步（被控端侧）：①应用控制端发来的比例到目标 tab 布局
            // （前后台均应用——比例不抢绝对网格，被控端按自身窗口×权重出格：前台立即重排、后台静默存待
            // 切前台）；②把被控端自身比例变化（用户拖了订阅 tab 的分隔条）发回控制端。回声由
            // `sub_layout_baseline` 免疫（应用对端比例即更新基线，故不会当本地改动回发）。
            if self.remote_ws.is_controlled() {
                if let Some((lt, rw, cw)) = self.remote_ws.take_sub_layout() {
                    if let Some(ti) = self.tabs.iter().position(|t| t.id == lt) {
                        let n = self.tabs[ti].panes.len();
                        if let Some(lay) = shell::layout::PaneLayout::from_weights(n, &rw, &cw) {
                            self.tabs[ti].layout = lay;
                        }
                    }
                    self.remote_ws.note_sub_layout_baseline(lt, rw, cw);
                }
                if let Some(sub_id) = self.remote_ws.sub_target() {
                    if let Some(ti) = self.tabs.iter().position(|t| t.id == sub_id) {
                        let rw = self.tabs[ti].layout.row_weights().to_vec();
                        let cw = self.tabs[ti].layout.col_weights().to_vec();
                        self.remote_ws.send_sub_layout_if_changed(sub_id, rw, cw);
                    }
                }
            }
            // part3d：推会话(tab)列表 + 概览状态（K6 去重，变化才发；busy 是布尔判定不含
            // spinner 字形、不刷链路）。F7②-remote：仅被控端采集会话图标位图上线（控制端
            // 不需要；进程快照较重，非被控端不做）。图标随 tab_states 一起 K6 去重——前台
            // exe 稳定则图标字节稳定，不额外刷链路。
            let controlled = self.remote_ws.is_controlled();
            if controlled {
                self.refresh_remote_tab_icons();
            }
            let tab_states: Vec<lumen_protocol::remote::TabState> = self
                .tabs
                .iter()
                .map(|t| lumen_protocol::remote::TabState {
                    id: t.id,
                    name: t.display_name(),
                    path: t.cwd_path(),
                    busy: t.is_busy(),
                    unseen: t.has_unseen(),
                    pane_count: t.panes.len() as u32,
                    icon: if controlled {
                        self.remote_tab_icon(t.id)
                    } else {
                        None
                    },
                })
                .collect();
            self.remote_ws.push_tab_list(tab_states);
            // 订阅目标刚变（含重订同一会话）→ 复位 mirror_src 强制重发 SubscriptionStarted，
            // 否则重订同一会话因窗格 key 未变不重发、控制端镜像空白。
            if self.remote_ws.take_sub_dirty() {
                self.mirror_src = None;
            }
            // part3d Phase 3：把控制端订阅会话的**全部窗格**快照 + 布局推过去（几何签名变 或
            // 刚订阅 → 发 SubscriptionStarted 含各窗格整屏快照 + 行列权重 + 最大化；签名未变、仅焦点
            // 窗格边界推进 → 刷 HistoryBounds 供焦点/单窗格回看）。被控端自身焦点不动（需求 c/e）。
            if let Some(sub_id) = self.remote_ws.sub_target() {
                if let Some(ti) = self.tabs.iter().position(|t| t.id == sub_id) {
                    // 廉价几何签名（不含整屏快照）：各窗格 (id,行,列) + 焦点下标 + 最大化下标。
                    let sig: MirrorSig = {
                        let tab = &self.tabs[ti];
                        (
                            tab.id,
                            tab.panes
                                .iter()
                                .map(|p| {
                                    let g = p.term.grid();
                                    (p.id, g.rows() as u16, g.cols() as u16)
                                })
                                .collect(),
                            tab.focused as u32,
                            tab.maximized.map(|m| m as u32),
                        )
                    };
                    // 焦点窗格当前历史边界（HistoryBounds 刷新，焦点/单窗格回看锚点跟实时推进）。
                    let (fbase, fscreen_top) = {
                        let g = self.tabs[ti].focused_pane().term.grid();
                        let st = g.absolute_cursor_line().saturating_sub(g.cursor.row as u64);
                        (st.saturating_sub(g.scrollback_len() as u64), st)
                    };
                    if self.mirror_src.as_ref() != Some(&sig) {
                        // 几何变 / 刚订阅：发全部窗格整屏快照 + 布局。
                        let panes: Vec<lumen_protocol::remote::PaneSnapshot> = self.tabs[ti]
                            .panes
                            .iter()
                            .map(|p| {
                                let g = p.term.grid();
                                let st =
                                    g.absolute_cursor_line().saturating_sub(g.cursor.row as u64);
                                let base = st.saturating_sub(g.scrollback_len() as u64);
                                lumen_protocol::remote::PaneSnapshot {
                                    session_id: p.id,
                                    rows: g.rows() as u16,
                                    cols: g.cols() as u16,
                                    snapshot: remote_mirror::screen_snapshot_vt(&p.term),
                                    base,
                                    screen_top: st,
                                    custom_title: p.custom_title.clone(),
                                }
                            })
                            .collect();
                        let row_weights = self.tabs[ti].layout.row_weights().to_vec();
                        let col_weights = self.tabs[ti].layout.col_weights().to_vec();
                        let maximized = self.tabs[ti].maximized.map(|m| m as u32);
                        let focused = self.tabs[ti].focused as u32;
                        self.mirror_src = Some(sig);
                        self.remote_ws.send_subscription_started(
                            sub_id,
                            focused,
                            panes,
                            row_weights,
                            col_weights,
                            maximized,
                        );
                        self.mirror_bounds_sent = Some((fbase, fscreen_top));
                    } else if self.mirror_bounds_sent != Some((fbase, fscreen_top)) {
                        self.remote_ws.send_history_bounds(fbase, fscreen_top);
                        self.mirror_bounds_sent = Some((fbase, fscreen_top));
                    }
                } else {
                    // 订阅会话已不存在（被关）：复位，待控制端改订阅 / 收 TabClosed 回退。
                    self.mirror_src = None;
                }
            } else {
                self.mirror_src = None;
            }
            // part3d：应答控制端回看请求——按绝对行号从目标窗格序列化历史行回传。`target=None` 走
            // 旧 HistoryRows（单窗格镜像焦点窗格）；`Some(sid)` 走 HistoryRowsForPane（多窗格指定窗格，
            // 按 id 查、非下标，D1）。各窗格 base/screen_top 独立（绝对行号体系按窗格独立）。
            let reqs = self.remote_ws.take_history_reqs();
            if !reqs.is_empty() {
                if let Some(sub_id) = self.remote_ws.sub_target() {
                    if let Some(ti) = self.tabs.iter().position(|t| t.id == sub_id) {
                        for (target, top, count) in reqs {
                            let count = usize::from(count.min(remote_ws::HISTORY_CHUNK_MAX));
                            let term = match target {
                                Some(sid) => match self.tabs[ti].panes.iter().find(|p| p.id == sid)
                                {
                                    Some(p) => &p.term,
                                    None => continue, // 目标窗格已关，丢弃该请求。
                                },
                                None => &self.tabs[ti].focused_pane().term,
                            };
                            let (lines, base, screen_top) =
                                remote_mirror::history_rows_vt(term, top, count);
                            match target {
                                Some(sid) => self
                                    .remote_ws
                                    .send_history_rows_for_pane(sid, top, base, screen_top, lines),
                                None => self
                                    .remote_ws
                                    .send_history_rows(top, base, screen_top, lines),
                            }
                        }
                    }
                }
            }
            // part3d 修③：远程文件树跟**控制端订阅的会话** cwd（不再跟被控端自身焦点 tab——两端焦点不同
            // tab 时树对不上控制端正在看的会话）。未订阅时回退被控端焦点 tab（至少有树）。cwd 未知不推。
            if let Some(cwd) = self
                .remote_ws
                .sub_target()
                .and_then(|sid| self.tabs.iter().find(|t| t.id == sid))
                .or_else(|| self.tabs.get(self.active_tab))
                .and_then(remote_root_cwd)
            {
                self.remote_ws.send_root_changed(cwd);
            }
            // part3c-2 Option B 文件服务：控制端按需 ListDir → 被控端后台读盘 → 回包发回。
            // 读盘走后台线程（慢速网络盘不冻 UI），结果经 svc 通道由 drain_service 发回。
            for (req_id, path, show_hidden) in self.remote_ws.take_listdir_reqs() {
                self.remote_ws.spawn_list_dir(req_id, path, show_hidden);
            }
            for (req_id, path, show_hidden) in self.remote_ws.take_listdir_recursive_reqs() {
                self.remote_ws
                    .spawn_list_dir_recursive(req_id, path, show_hidden);
            }
            self.remote_ws.drain_service();
        } else {
            self.mirror_src = None;
            self.mirror_bounds_sent = None;
        }
        // 片8 控制端：递归枚举好的远程子树 → 构造系统剪贴板虚拟文件目录（资源管理器可粘贴整棵树）。
        if let Some(entries) = self.remote_ws.take_clip_dir_ready() {
            log::debug!("[片8] 控制端构造虚拟文件目录: {} 项", entries.len());
            if let Some(svc) = self.clipboard_svc.as_ref() {
                svc.set_remote_dir(entries);
            }
        }
        // 被控端视口跟随（SSH 式）：应用控制端请求的视口尺寸，覆盖自身窗口尺寸；
        // 非被控时清除以恢复窗口尺寸（resize 循环据 remote_viewport 决定焦点窗格尺寸）。
        if controlled {
            if let Some((r, c)) = self.remote_ws.take_viewport() {
                let dims = (usize::from(r).max(1), usize::from(c).max(1));
                if self.remote_viewport != Some(dims) {
                    self.remote_viewport = Some(dims);
                    if self.app_lock.is_locked() {
                        // 锁屏不跑本机 shell 布局；被控端焦点窗格仍须
                        // 立即跟随控制端视口，保证授权远控在锁定期间
                        // 完整可用。只改 Terminal/PTY，不触碰本机 UI。
                        let pane = self.focused_pane_mut();
                        let current = {
                            let grid = pane.term.grid();
                            (grid.rows(), grid.cols())
                        };
                        if current != dims {
                            pane.term.resize(dims.0, dims.1);
                            if let Err(e) = pane.pty.resize(r.max(1), c.max(1)) {
                                log::warn!("锁定期间远程视口 PTY resize 失败: {e:#}");
                            }
                            // 网格签名变化后下次 pump 强制重发镜像快照。
                            self.mirror_src = None;
                        }
                    } else {
                        self.window.request_redraw();
                    }
                }
            }
        } else if self.remote_viewport.take().is_some() {
            // 断开/不再被控：恢复窗口尺寸（下一帧 resize 循环按矩形重算）。
            self.window.request_redraw();
        }
        // 控制端接管的窗格网格：不再被控 或 订阅目标已换 / 取消时释放，本机 resize
        // 循环下一帧按窗格矩形重算，被控端立即回到自己窗口的排版。
        if let Some((owned_tab, _)) = self.remote_pane_viewports.as_ref() {
            let still_owned = controlled && self.remote_ws.sub_target() == Some(*owned_tab);
            if !still_owned {
                self.remote_pane_viewports = None;
                self.window.request_redraw();
            }
        }
        if changed || applied {
            self.window.request_redraw();
        }
    }

    fn text_editor_source_is_current(&self, source: &shell::text_editor::TextFileSource) -> bool {
        use shell::text_editor::TextFileSource;
        match source {
            TextFileSource::Remote { generation, .. } => {
                self.remote_ws.is_controlling() && *generation == self.remote_ws.edit_generation()
            }
            TextFileSource::Ssh {
                runtime_id,
                session_id,
                ..
            } => {
                *runtime_id == self.ssh_runtime.runtime_id()
                    && self.ssh_runtime.contains_session(*session_id)
            }
        }
    }

    fn invalidate_stale_text_editor_source(&mut self) {
        let stale_sources = self
            .shell_state
            .text_editor
            .sources()
            .filter(|source| !self.text_editor_source_is_current(source))
            .cloned()
            .collect::<Vec<_>>();
        for source in stale_sources {
            self.shell_state
                .text_editor
                .invalidate_source(&source, i18n::strings().text_editor_source_invalidated);
        }
    }

    fn apply_remote_edit_events(&mut self, events: Vec<remote_ws::EditEvent>) {
        use shell::text_editor::{SaveFailure, TextFileSource};
        let current_generation = self.remote_ws.edit_generation();
        for event in events {
            match event {
                remote_ws::EditEvent::Loaded { token, bytes, .. } => {
                    if matches!(
                        self.shell_state.text_editor.source_for_token(token),
                        Some(TextFileSource::Remote { generation, .. })
                            if *generation == current_generation
                    ) {
                        self.shell_state.text_editor.apply_loaded(token, Ok(bytes));
                    }
                }
                remote_ws::EditEvent::Saved { token, .. } => {
                    let path = match self.shell_state.text_editor.source_for_token(token) {
                        Some(TextFileSource::Remote { generation, path })
                            if *generation == current_generation =>
                        {
                            Some(path.clone())
                        }
                        _ => None,
                    };
                    if let Some(path) = path {
                        if self.shell_state.text_editor.apply_saved(token, Ok(())) {
                            if let Some(parent) = remote_parent_path(&path) {
                                self.remote_ws.refresh_remote_path(parent);
                            }
                        }
                    }
                }
                remote_ws::EditEvent::Conflict { token, .. } => {
                    if matches!(
                        self.shell_state.text_editor.source_for_token(token),
                        Some(TextFileSource::Remote { generation, .. })
                            if *generation == current_generation
                    ) {
                        self.shell_state
                            .text_editor
                            .apply_saved(token, Err(SaveFailure::Conflict));
                    }
                }
                remote_ws::EditEvent::Error { token, error, .. } => {
                    let message = remote_edit_error_message(error);
                    if matches!(
                        self.shell_state.text_editor.source_for_token(token),
                        Some(TextFileSource::Remote { generation, .. })
                            if *generation == current_generation
                    ) && !self
                        .shell_state
                        .text_editor
                        .apply_saved(token, Err(SaveFailure::Message(message.clone())))
                    {
                        self.shell_state
                            .text_editor
                            .apply_loaded(token, Err(message));
                    }
                }
            }
        }
        self.window.request_redraw();
    }

    fn open_text_editor(&mut self, source: shell::text_editor::TextFileSource) {
        if !self.text_editor_source_is_current(&source) {
            self.invalidate_stale_text_editor_source();
            self.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                i18n::strings().text_editor_source_invalidated,
            );
            self.window.request_redraw();
            return;
        }
        if let Some(request) = self.shell_state.text_editor.request_open(source) {
            self.start_text_editor_load(request);
        }
        self.terminal_focused = false;
        self.window.request_redraw();
    }

    fn start_text_editor_load(&mut self, request: shell::text_editor::LoadRequest) {
        if !self.text_editor_source_is_current(&request.source) {
            self.shell_state.text_editor.invalidate_source(
                &request.source,
                i18n::strings().text_editor_source_invalidated,
            );
            return;
        }
        let token = request.token;
        let result = match request.source {
            shell::text_editor::TextFileSource::Remote { path, .. } => {
                self.remote_ws.start_edit_fetch(token, path);
                Ok(())
            }
            shell::text_editor::TextFileSource::Ssh {
                session_id, path, ..
            } => self.ssh_runtime.read_text(session_id, token, path),
        };
        if let Err(error) = result {
            self.shell_state.text_editor.apply_loaded(token, Err(error));
        }
    }

    fn start_text_editor_save(&mut self, request: shell::text_editor::SaveRequest) {
        use shell::text_editor::{SaveFailure, TextFileSource};
        if !self.text_editor_source_is_current(&request.source) {
            self.shell_state.text_editor.invalidate_source(
                &request.source,
                i18n::strings().text_editor_source_invalidated,
            );
            return;
        }
        if !self.shell_state.text_editor.mark_saving(&request) {
            return;
        }
        let token = request.token;
        let result = match request.source {
            TextFileSource::Remote { path, .. } => {
                self.remote_ws.start_edit_save(
                    token,
                    path,
                    request.bytes,
                    lumen_protocol::remote::FileRevision {
                        len: request.expected_len,
                        sha256: request.expected_sha256,
                    },
                    request.force,
                );
                Ok(())
            }
            TextFileSource::Ssh {
                session_id, path, ..
            } => self.ssh_runtime.write_text(
                session_id,
                token,
                path,
                request.bytes,
                request.expected_sha256,
                request.force,
            ),
        };
        if let Err(error) = result {
            self.shell_state
                .text_editor
                .apply_saved(token, Err(SaveFailure::Message(error)));
        }
    }

    /// 控制端镜像视图是否生效（控制中 且 处于「远程」视图）：决定键盘是否转发给
    /// 被控端而非本地执行（bug3：切回「本地」视图则本地输入、不转发、不画镜像）。
    fn is_mirror_active(&self) -> bool {
        self.remote_ws.is_controlling() && self.settings.layout.view_mode.is_remote()
    }

    /// 被控端：查该窗格的网格是否由控制端接管（`SubViewport` 记入 `remote_pane_viewports`），
    /// 是则返回控制端要求的 `(行, 列)`。仅**当前订阅**会话的窗格算数——tab_id 不匹配（本机其他
    /// 会话）或窗格不在控制端上报的清单里（订阅后新开的窗格，控制端下一帧才带上）返回 `None`，
    /// 由调用方回退到本机窗格矩形计算。
    fn controller_owned_grid(
        &self,
        tab_id: session::TabId,
        session_id: session::SessionId,
    ) -> Option<(usize, usize)> {
        controller_owned_pane_grid(self.remote_pane_viewports.as_ref(), tab_id, session_id)
    }

    /// 当前工作模式和覆盖层是否允许把键盘/IME 焦点交给终端。
    fn terminal_focus_allowed(&self) -> bool {
        let ssh_overlay = self.settings.layout.view_mode.is_ssh()
            && (self.shell_state.ssh_credentials.is_some()
                || self.ssh_runtime.active_blocks_input());
        let overlay = self.shell_state.settings.open
            || self.shell_state.login.open
            || self.shell_state.history_search.open
            || (self.shell_state.completion.open && !self.shell_state.completion.passive)
            || self.shell_state.renaming.is_some()
            || self.shell_state.pane_renaming.is_some()
            || self.shell_state.ssh_session_renaming.is_some()
            || self.shell_state.filetree.dialog_open()
            || self.shell_state.text_editor.is_visible()
            || ssh_overlay;
        if overlay {
            return false;
        }
        !self.settings.layout.view_mode.is_ssh() || self.ssh_runtime.active_accepts_input()
    }

    /// 工作模式切换的唯一状态入口。保持后台本地 PTY / 远程连接运行，
    /// 只切换本机可见域并清理上一模式遗留的交互状态。
    fn switch_view_mode(&mut self, next: settings::ViewMode) -> bool {
        if self.settings.layout.view_mode == next {
            return false;
        }

        // 必须在改 view_mode 前补发 Release：远程镜像输入路由依赖旧模式。
        self.release_held_report_buttons();
        self.settings.layout.view_mode = next;
        if !next.is_ssh() {
            // 切走时丢弃并清零一次性凭据，同时关闭对应的待认证会话；
            // 已建立的 actor 保持后台运行。
            self.discard_ssh_credential_dialog();
        }

        self.terminal_focused = self.terminal_focus_allowed();
        self.filetree_hovered = false;
        self.filetree_focused = false;
        self.hovered_link = None;
        self.hover_probe_cell = None;
        self.scrollbar_drag = None;
        self.autoscroll_drag = 0;
        self.autoscroll_at = None;
        self.last_left_click = None;
        self.pane_rects_px.clear();
        self.pane_close_rects_px.clear();
        self.divider_rects_px.clear();
        self.panel_resize_rects_px.clear();
        self.ssh_rect_px = None;
        self.mirror_rect_px = None;
        self.mirror_pane_rects_px.clear();
        self.remote_ws.clear_mirror_selection();

        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                pane.selecting = false;
                #[cfg(feature = "input-editor")]
                {
                    pane.preedit = None;
                }
            }
        }

        if next.is_remote() {
            self.remote.request_refresh();
        }
        self.update_window_title();
        self.window.request_redraw();
        true
    }

    fn ensure_ssh_sync_worker(&mut self) {
        let server_url = cloud::server_url();
        let Some((account_id, server_url)) = ssh_sync_identity(self.profile.as_ref(), &server_url)
        else {
            // 地址被清空时只停止同步，不抹掉仍供远程功能使用的当前账号 token。
            drop(self.ssh_sync.take());
            return;
        };
        let Some(token) = self.auth_token.clone() else {
            drop(self.ssh_sync.take());
            return;
        };
        let token_is_present = token
            .read()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        if !token_is_present {
            drop(self.ssh_sync.take());
            return;
        }

        let Some(store) = self.ssh_store.as_ref() else {
            drop(self.ssh_sync.take());
            return;
        };
        if !should_trigger_ssh_sync_after_local_change(Some(&account_id), store.account_id()) {
            log::warn!("拒绝启动 SSH 同步：worker 账号与库存作用域不一致（worker={account_id}）");
            drop(self.ssh_sync.take());
            return;
        }
        let Some(snapshot) = store.sync_snapshot() else {
            drop(self.ssh_sync.take());
            return;
        };

        if self.ssh_sync.as_ref().is_some_and(|active| {
            active.account_id == account_id
                && active.server_url == server_url
                && Arc::ptr_eq(&active.token, &token)
        }) {
            return;
        }

        // 防御性处理绕过标准切换入口的状态漂移。地址变化且账号/token
        // 未变时不能清 token；账号或 token 句柄变化时先清旧句柄再 drop。
        if let Some(active) = self.ssh_sync.as_ref() {
            if active.account_id != account_id || !Arc::ptr_eq(&active.token, &token) {
                clear_shared_token(&active.token);
            }
        }
        drop(self.ssh_sync.take());
        if token
            .read()
            .map(|value| value.trim().is_empty())
            .unwrap_or(true)
        {
            return;
        }

        let proxy = self.proxy.clone();
        let notifier: ssh::SshSyncNotifier = Arc::new(move || {
            // 同步数据只留在 worker 自身事件通道；自定义事件仅负责唤醒
            // winit 主线程，绝不携带账号、配置或错误正文。
            let _ = proxy.send_event(PtyWake);
        });
        match ssh::SshSyncWorker::start_with_notifier(
            server_url.clone(),
            token.clone(),
            snapshot,
            Some(notifier),
        ) {
            Ok(worker) => {
                info!("SSH 配置同步已启动：account={account_id}");
                self.ssh_sync = Some(ActiveSshSync {
                    account_id,
                    server_url,
                    token,
                    worker,
                    last_failure: None,
                });
            }
            Err(error) => {
                log::error!("启动 SSH 配置同步线程失败: {error}");
                self.shell_state.toast.push(
                    shell::toast::ToastKind::Error,
                    format!("SSH 配置同步启动失败: {error}"),
                );
                self.window.request_redraw();
            }
        }
    }

    fn invalidate_account_for_server_url_change(&mut self) {
        let had_account_state =
            self.profile.is_some() || self.auth_token.is_some() || self.ssh_sync.is_some();
        if !had_account_state {
            return;
        }

        // token 未携带签发 origin，绝不能把旧地址签发的 bearer token 或
        // 账号 SSH 快照送往用户刚填写的新地址。先原地抹 token、停掉
        // 全部账号网络 worker，再删除登录档案并切回 Local 库存。
        self.stop_account_bound_workers();
        profile::Profile::delete();
        self.profile = None;
        self.reload_ssh_account_context();
        if let Some(service) = self.clipboard_svc.as_ref() {
            service.clear();
        }
        self.shell_state.toast.push(
            shell::toast::ToastKind::Warn,
            "服务器地址已更改，为保护账号数据，请重新登录",
        );
        self.window.request_redraw();
    }

    fn stop_account_bound_workers(&mut self) {
        if self.shell_state.text_editor.is_open() {
            self.shell_state.text_editor.close_without_prompt();
            self.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                i18n::strings().text_editor_closed_account_change,
            );
            self.window.request_redraw();
        }

        // 顺序是安全约束：先原地抹空所有旧 token Arc，再 drop worker。
        if let Some(active) = self.ssh_sync.as_ref() {
            clear_shared_token(&active.token);
        }
        if let Some(token) = self.auth_token.as_ref() {
            let already_cleared = self
                .ssh_sync
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(&active.token, token));
            if !already_cleared {
                clear_shared_token(token);
            }
        }
        drop(self.ssh_sync.take());
        self.remote.stop();
        self.remote_ws.stop();
        self.remote_restore_attempted = false;
        self.auth_token = None;
    }

    fn persist_remote_restore_target(&mut self) {
        let Some((device_id, device_name)) = self
            .remote_ws
            .controller_peer()
            .map(|(id, name)| (id.to_owned(), name.to_owned()))
        else {
            return;
        };
        if let Some(profile) = self.profile.as_mut() {
            if profile.remember_remote_restore_target(&device_id, &device_name) {
                profile.save();
            }
        }
    }

    fn clear_remote_restore_target(&mut self) {
        if let Some(profile) = self.profile.as_mut() {
            if profile.clear_remote_restore_target() {
                profile.save();
            }
        }
    }

    fn reload_ssh_account_context(&mut self) {
        // SSH 连接和全部瞬时 UI 都属于旧账号。切换库存前先清掉，
        // 防止旧 profile id、凭据对话框或 actor 落到新账号页面。
        self.discard_ssh_credential_dialog();
        self.ssh_runtime = ssh_runtime::SshRuntime::default();
        self.ssh_force_credential_prompt.clear();
        self.shell_state.ssh_ui = shell::ssh_ui::SshUiState::default();
        self.ssh_rect_px = None;
        self.terminal_focused = self.terminal_focus_allowed();

        self.ssh_store = match paths::data_dir() {
            Some(data_root) => {
                match load_ssh_store_for_profile(
                    &data_root,
                    self.profile.as_ref(),
                    &cloud::server_url(),
                ) {
                    Ok(store) => Some(store),
                    Err(error) => {
                        log::error!("切换账号后加载 SSH 库存失败（保留原文件）: {error}");
                        self.shell_state
                            .toast
                            .push(shell::toast::ToastKind::Error, error.to_string());
                        None
                    }
                }
            }
            None => {
                self.shell_state.toast.push(
                    shell::toast::ToastKind::Error,
                    "无法解析 Lumen 数据目录，SSH 配置本次不可写",
                );
                None
            }
        };
        self.auth_token = profile_auth_token(self.profile.as_ref(), &cloud::server_url());
        self.remote_restore_attempted = false;
        self.ensure_ssh_sync_worker();
        self.window.request_redraw();
    }

    fn notify_ssh_local_change(&self) {
        let Some(active) = self.ssh_sync.as_ref() else {
            return;
        };
        let Some(store) = self.ssh_store.as_ref() else {
            return;
        };
        if !should_trigger_ssh_sync_after_local_change(Some(&active.account_id), store.account_id())
        {
            return;
        }
        if let Some(snapshot) = store.sync_snapshot() {
            active.worker.update_snapshot_and_trigger(snapshot);
        }
    }

    fn show_ssh_sync_failure_once(&mut self, account_id: &str, message: String) {
        let should_show = self
            .ssh_sync
            .as_mut()
            .filter(|active| active.account_id == account_id)
            .is_some_and(|active| {
                if active.last_failure.as_deref() == Some(message.as_str()) {
                    false
                } else {
                    active.last_failure = Some(message.clone());
                    true
                }
            });
        if should_show {
            log::warn!("SSH 配置同步失败：account={account_id}, {message}");
            self.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                format!("SSH 配置同步失败: {message}"),
            );
            self.window.request_redraw();
        }
    }

    fn drain_ssh_sync(&mut self) {
        let events = self
            .ssh_sync
            .as_ref()
            .map(|active| {
                std::iter::from_fn(|| active.worker.poll()).collect::<Vec<ssh::SshSyncEvent>>()
            })
            .unwrap_or_default();

        for event in events {
            let event_account = match &event {
                ssh::SshSyncEvent::Completed(completed) => {
                    completed.snapshot.account_id().to_owned()
                }
                ssh::SshSyncEvent::Failed(failed) => failed.account_id.clone(),
            };
            let worker_account = self
                .ssh_sync
                .as_ref()
                .map(|active| active.account_id.as_str());
            let store_account = self.ssh_store.as_ref().and_then(ssh::SshStore::account_id);
            if !should_apply_ssh_sync_event(worker_account, store_account, &event_account) {
                log::warn!(
                    "丢弃过期 SSH 同步事件：event_account={event_account}, worker_account={worker_account:?}, store_account={store_account:?}"
                );
                continue;
            }

            match event {
                ssh::SshSyncEvent::Failed(failed) => {
                    // 失败事件不触碰 SshStore；worker 自身按退避计划重试。
                    self.show_ssh_sync_failure_once(
                        &failed.account_id,
                        format!("{}: {}", failed.error.code(), failed.error.user_message()),
                    );
                }
                ssh::SshSyncEvent::Completed(completed) => {
                    let applied = {
                        let Some(store) = self.ssh_store.as_mut() else {
                            continue;
                        };
                        let profiles_before = store
                            .inventory()
                            .profiles()
                            .iter()
                            .map(|profile| {
                                (profile.id.clone(), store.binding(&profile.id).cloned())
                            })
                            .collect::<Vec<_>>();
                        match store.apply_sync_completed(completed) {
                            Ok(report) => {
                                let removed_profiles = profiles_before
                                    .iter()
                                    .filter(|(id, _)| store.inventory().profile(id).is_none())
                                    .map(|(id, _)| id.clone())
                                    .collect::<Vec<_>>();
                                let changed_bindings = profiles_before
                                    .into_iter()
                                    .filter_map(|(id, before)| {
                                        let after = store.binding(&id).cloned();
                                        (before != after).then_some((before, after))
                                    })
                                    .collect::<Vec<_>>();
                                let snapshot = store.sync_snapshot();
                                let pending_mutations = store.pending_sync_mutations();
                                Ok((
                                    report,
                                    snapshot,
                                    pending_mutations,
                                    removed_profiles,
                                    changed_bindings,
                                ))
                            }
                            Err(error) => Err(error),
                        }
                    };

                    let Ok((
                        report,
                        snapshot,
                        pending_mutations,
                        removed_profiles,
                        changed_bindings,
                    )) = applied
                    else {
                        let error = applied.expect_err("上方已匹配错误分支");
                        self.show_ssh_sync_failure_once(
                            &event_account,
                            format!("应用服务端回包失败: {error}"),
                        );
                        continue;
                    };

                    if let Some(active) = self
                        .ssh_sync
                        .as_mut()
                        .filter(|active| active.account_id == event_account)
                    {
                        active.last_failure = None;
                        if let Some(snapshot) = snapshot {
                            active.worker.update_snapshot(snapshot);
                            if should_continue_ssh_sync(report.has_more, pending_mutations) {
                                active.worker.trigger();
                            }
                        }
                    }

                    // 只断开真正被远端删除（或被服务端拒绝并清理）的 profile；
                    // 远端编辑、移动和删组保留正在运行的 SSH actor。
                    if !removed_profiles.is_empty() {
                        let selected_removed = self
                            .shell_state
                            .ssh_ui
                            .selected_profile_id()
                            .is_some_and(|selected| {
                                removed_profiles.iter().any(|id| id == selected)
                            });
                        for profile_id in &removed_profiles {
                            self.discard_ssh_credential_dialog_for_profile(profile_id);
                            self.ssh_runtime.remove_profile(profile_id);
                            self.ssh_force_credential_prompt.remove(profile_id);
                        }
                        if selected_removed {
                            self.shell_state.ssh_ui.select_profile(None);
                        }
                        self.ssh_rect_px = None;
                        self.terminal_focused = self.terminal_focus_allowed();
                    }
                    for (before, after) in &changed_bindings {
                        self.delete_obsolete_ssh_secrets(before.as_ref(), after.as_ref());
                    }
                    info!(
                        "SSH 配置同步完成：ack={} changes={} rejected={} cursor={} deferred={}",
                        report.acknowledged,
                        report.applied_changes,
                        report.rejected.len(),
                        report.server_cursor,
                        report.deferred_changes
                    );
                    self.window.request_redraw();
                }
            }
        }
    }

    /// 串行施加 SSH 页面动作。`SshStore` 是唯一写 owner；每次 CRUD
    /// 内部会把库存与本机凭据绑定作为同一 generation 原子提交。
    fn apply_ssh_ui_actions(&mut self, actions: Vec<shell::ssh_ui::SshUiAction>) {
        use shell::ssh_ui::SshUiAction;

        for action in actions {
            let action = match action {
                SshUiAction::ConnectProfile { id } => {
                    self.shell_state.ssh_ui.select_profile(Some(id.clone()));
                    self.begin_ssh_profile_connect(&id);
                    continue;
                }
                SshUiAction::SubmitProfile(submission) => {
                    self.apply_ssh_profile_submission(*submission);
                    continue;
                }
                SshUiAction::CancelConnectionTest { form_id } => {
                    if self.ssh_runtime.cancel_connection_test(form_id) {
                        self.window.request_redraw();
                    }
                    continue;
                }
                action => action,
            };

            let Some(store) = self.ssh_store.as_mut() else {
                self.shell_state.toast.push(
                    shell::toast::ToastKind::Error,
                    "SSH 配置存储不可用，请先处理启动时的存储错误",
                );
                continue;
            };

            let mut deleted_profile = None;
            let result = match action {
                SshUiAction::CreateGroup { name } => store.create_group(&name).map(|_| ()),
                SshUiAction::RenameGroup { id, name } => store.rename_group(&id, &name),
                SshUiAction::DeleteGroup { id } => store.delete_group(&id),
                SshUiAction::DeleteProfile { id } => {
                    let binding = store.binding(&id).cloned();
                    let result = store.delete_profile(&id);
                    if result.is_ok() {
                        deleted_profile = Some((id, binding));
                    }
                    result
                }
                SshUiAction::MoveProfile {
                    id,
                    target_group_id,
                    target_index,
                } => store.move_profile(&id, target_group_id.as_deref(), target_index),
                SshUiAction::ConnectProfile { .. }
                | SshUiAction::SubmitProfile(_)
                | SshUiAction::CancelConnectionTest { .. } => {
                    unreachable!("特殊 SSH UI 动作已在上方处理")
                }
            };
            match result {
                Ok(()) => {
                    self.notify_ssh_local_change();
                    if let Some((id, binding)) = deleted_profile {
                        self.discard_ssh_credential_dialog_for_profile(&id);
                        self.ssh_runtime.remove_profile(&id);
                        self.ssh_force_credential_prompt.remove(&id);
                        self.delete_obsolete_ssh_secrets(binding.as_ref(), None);
                        self.ssh_rect_px = None;
                        self.terminal_focused = false;
                    }
                    self.window.request_redraw();
                }
                Err(error) => {
                    error!("SSH 配置变更失败: {error}");
                    self.shell_state
                        .toast
                        .push(shell::toast::ToastKind::Error, error.to_string());
                    self.window.request_redraw();
                }
            }
        }
    }

    fn apply_ssh_profile_submission(&mut self, submission: shell::ssh_ui::SshProfileSubmission) {
        use shell::ssh_ui::ProfileSubmitIntent;

        match submission.intent() {
            ProfileSubmitIntent::Save => self.save_ssh_profile_submission(submission),
            ProfileSubmitIntent::TestConnection => {
                self.begin_ssh_connection_test(submission);
            }
        }
    }

    fn save_ssh_profile_submission(&mut self, mut submission: shell::ssh_ui::SshProfileSubmission) {
        let form_id = submission.form_id();
        self.ssh_runtime.cancel_connection_test(form_id);
        let host_key_verified_for_endpoint = submission.host_key_verified_for_current_endpoint();
        let editing_id = submission.take_editing_id();
        let draft = submission.take_draft();
        let mut password = zeroize::Zeroizing::new(submission.take_password());
        let private_key_path = submission.take_private_key_path();
        let mut key_passphrase = zeroize::Zeroizing::new(submission.take_key_passphrase());

        // UI 会禁用无效提交，但主线程必须在任何 inventory mutation 前再次
        // 复核本机私钥。编辑且 endpoint/auth 均未变化时允许沿用现有绑定；
        // 新建、切换认证方式或修改 endpoint 都必须显式选择文件。
        let private_key_submission_is_valid = {
            let store = self.ssh_store.as_ref();
            let saved_profile = editing_id.as_deref().and_then(|profile_id| {
                store.and_then(|store| store.inventory().profile(profile_id))
            });
            let saved_binding = editing_id
                .as_deref()
                .and_then(|profile_id| store.and_then(|store| store.binding(profile_id)));
            ssh_private_key_submission_is_valid(
                &draft,
                saved_profile,
                saved_binding,
                private_key_path.as_deref(),
            )
        };
        if !private_key_submission_is_valid {
            self.shell_state.toast.push(
                shell::toast::ToastKind::Error,
                "请选择当前设备上存在的 SSH 私钥文件后再保存",
            );
            self.window.request_redraw();
            return;
        }

        let saved = (|| -> Result<(String, Option<ssh::SshLocalBinding>), String> {
            let store = self
                .ssh_store
                .as_mut()
                .ok_or_else(|| "SSH 配置存储不可用，请先处理启动时的存储错误".to_owned())?;
            match editing_id {
                Some(id) => {
                    let previous = store.binding(&id).cloned();
                    if host_key_verified_for_endpoint {
                        store
                            .update_profile_with_verified_host_key(&id, draft)
                            .map_err(|error| error.to_string())?;
                    } else {
                        store
                            .update_profile(&id, draft)
                            .map_err(|error| error.to_string())?;
                    }
                    Ok((id, previous))
                }
                None => store
                    .create_profile(draft)
                    .map(|id| (id, None))
                    .map_err(|error| error.to_string()),
            }
        })();

        let (profile_id, previous_binding) = match saved {
            Ok(saved) => saved,
            Err(error) => {
                log::error!("SSH 服务器表单保存失败: {error}");
                self.shell_state
                    .toast
                    .push(shell::toast::ToastKind::Error, error);
                self.window.request_redraw();
                return;
            }
        };
        let profile = self
            .ssh_store
            .as_ref()
            .and_then(|store| store.inventory().profile(&profile_id))
            .cloned();
        let Some(profile) = profile else {
            self.shell_state.toast.push(
                shell::toast::ToastKind::Error,
                "SSH 服务器已保存，但无法重新读取配置",
            );
            self.window.request_redraw();
            return;
        };

        let mut credential_save_failed = false;
        match profile.auth_method {
            ssh::AuthMethod::Password if !password.is_empty() => {
                let credential_submission = shell::SshCredentialSubmission::from_password(
                    profile.id.clone(),
                    profile.host.clone(),
                    profile.port,
                    profile.username.clone(),
                    std::mem::take(&mut *password),
                );
                if self
                    .save_ssh_credential_submission(&profile, credential_submission)
                    .is_err()
                {
                    credential_save_failed = true;
                }
            }
            ssh::AuthMethod::PrivateKey => {
                if let Some(path) = private_key_path {
                    let credential_submission = shell::SshCredentialSubmission::from_private_key(
                        profile.id.clone(),
                        profile.host.clone(),
                        profile.port,
                        profile.username.clone(),
                        path,
                        std::mem::take(&mut *key_passphrase),
                    );
                    if self
                        .save_ssh_credential_submission(&profile, credential_submission)
                        .is_err()
                    {
                        credential_save_failed = true;
                    }
                }
            }
            ssh::AuthMethod::Password | ssh::AuthMethod::Agent
                if !password.is_empty() || !key_passphrase.is_empty() =>
            {
                log::warn!("忽略认证方式已变化的 SSH 表单凭据缓冲");
            }
            ssh::AuthMethod::Password | ssh::AuthMethod::Agent => {}
        }

        let current_binding = self
            .ssh_store
            .as_ref()
            .and_then(|store| store.binding(&profile_id))
            .cloned();
        self.delete_obsolete_ssh_secrets(previous_binding.as_ref(), current_binding.as_ref());
        self.notify_ssh_local_change();
        self.shell_state
            .ssh_ui
            .select_profile(Some(profile_id.clone()));
        if credential_save_failed {
            let message = if profile.auth_method == ssh::AuthMethod::PrivateKey {
                "SSH 服务器已保存，但无法安全绑定本机私钥"
            } else {
                "SSH 服务器已保存，但密码无法安全写入本机凭据存储"
            };
            self.shell_state
                .toast
                .push(shell::toast::ToastKind::Error, message);
        }
        self.window.request_redraw();
    }

    fn begin_ssh_connection_test(&mut self, mut submission: shell::ssh_ui::SshProfileSubmission) {
        let form_id = submission.form_id();
        let connection_revision = submission.connection_revision();
        let editing_id = submission.take_editing_id();
        let draft = submission.take_draft();
        let mut password = zeroize::Zeroizing::new(submission.take_password());
        let private_key_path = submission.take_private_key_path();
        let mut key_passphrase = zeroize::Zeroizing::new(submission.take_key_passphrase());
        let profile = ssh_test_profile(form_id, &draft);

        let saved_credential = || -> Option<lumen_ssh::Credential> {
            let profile_id = editing_id.as_deref()?;
            let saved_profile = self.ssh_store.as_ref()?.inventory().profile(profile_id)?;
            if !ssh_profile_matches_test_target(saved_profile, &profile) {
                return None;
            }
            self.load_saved_ssh_credential(saved_profile).ok().flatten()
        };

        let credential = match profile.auth_method {
            ssh::AuthMethod::Password if !password.is_empty() => Some(
                lumen_ssh::Credential::password(std::mem::take(&mut *password)),
            ),
            ssh::AuthMethod::PrivateKey => match private_key_path {
                Some(path) if path.is_absolute() && path.is_file() => {
                    let passphrase = (!key_passphrase.is_empty()).then(|| {
                        lumen_ssh::SecretString::new(std::mem::take(&mut *key_passphrase))
                    });
                    Some(lumen_ssh::Credential::private_key(path, passphrase))
                }
                Some(_) => None,
                None => saved_credential(),
            },
            ssh::AuthMethod::Password => saved_credential(),
            ssh::AuthMethod::Agent => Some(lumen_ssh::Credential::agent()),
        };
        let Some(credential) = credential else {
            let message = match profile.auth_method {
                ssh::AuthMethod::Password => "请输入密码，或先保存本机密码后再测试连接",
                ssh::AuthMethod::PrivateKey => "请选择当前设备上存在的 SSH 私钥文件",
                ssh::AuthMethod::Agent => unreachable!("SSH Agent 不需要本机表单凭据"),
            };
            self.ssh_runtime
                .fail_connection_test(form_id, connection_revision, &profile, message);
            self.window.request_redraw();
            return;
        };

        if let Err(error) = self.ssh_runtime.start_connection_test(
            form_id,
            connection_revision,
            &profile,
            credential,
        ) {
            log::warn!("启动 SSH 连接测试失败: {error}");
        }
        self.window.request_redraw();
    }

    /// 删除旧 binding 中已不再被新 binding 引用的 Credential Manager
    /// 秘密。库存 generation 已在调用前提交；清理失败只产生不含 target
    /// 的安全提示，不回滚库存，也不影响其他 profile。
    fn delete_obsolete_ssh_secrets(
        &mut self,
        previous: Option<&ssh::SshLocalBinding>,
        current: Option<&ssh::SshLocalBinding>,
    ) {
        let Some(previous) = previous else {
            return;
        };
        let retained = [
            current.and_then(|binding| binding.password_credential_ref.as_deref()),
            current.and_then(|binding| binding.key_passphrase_credential_ref.as_deref()),
        ];
        let mut cleanup_failed = false;
        for raw_reference in [
            previous.password_credential_ref.as_deref(),
            previous.key_passphrase_credential_ref.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if retained
                .into_iter()
                .flatten()
                .any(|value| value == raw_reference)
            {
                continue;
            }
            let Ok(reference) = raw_reference.parse::<ssh::CredentialReference>() else {
                log::warn!("跳过格式非法的旧 SSH 本机凭据引用");
                cleanup_failed = true;
                continue;
            };
            if let Err(error) = ssh::delete_secret(&reference) {
                log::warn!("清理旧 SSH 本机凭据失败: {error}");
                cleanup_failed = true;
            }
        }
        if cleanup_failed {
            self.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                "SSH 配置已保存，但旧的本机凭据未能完全清理",
            );
        }
    }

    fn open_ssh_credential_dialog(
        &mut self,
        session_id: ssh_runtime::SshSessionId,
        profile: &ssh::SshProfile,
        kind: shell::SshCredentialKind,
    ) {
        // 凭据弹窗是全局单槽；新请求覆盖旧请求前必须关闭旧的待认证
        // 会话，不能在会话栏留下永远无法继续的空 Shell。
        self.discard_ssh_credential_dialog();
        self.terminal_focused = false;
        // 上一次失败原因（认证失败等）带进对话框内显示（红字）——错误
        // 文本属于这里，不该只挤在顶栏（海风哥：密码错误文本位置不对）。
        let error_text = self.ssh_runtime.session_error_detail(session_id);
        self.shell_state.ssh_credentials = Some(
            shell::SshCredentialDialog::open(
                session_id,
                profile.id.clone(),
                profile.name.clone(),
                profile.host.clone(),
                profile.port,
                profile.username.clone(),
                kind,
            )
            .with_error_text(error_text),
        );
    }

    fn discard_ssh_credential_dialog(&mut self) {
        if let Some(dialog) = self.shell_state.ssh_credentials.take() {
            self.ssh_runtime.close_session(dialog.session_id());
        }
    }

    fn discard_ssh_credential_dialog_for_profile(&mut self, profile_id: &str) {
        let matches = self
            .shell_state
            .ssh_credentials
            .as_ref()
            .is_some_and(|dialog| dialog.profile_id() == profile_id);
        if matches {
            self.discard_ssh_credential_dialog();
        }
    }

    fn clear_ssh_credential_dialog_for_session(
        &mut self,
        session_id: ssh_runtime::SshSessionId,
    ) -> bool {
        let matches = self
            .shell_state
            .ssh_credentials
            .as_ref()
            .is_some_and(|dialog| dialog.session_id() == session_id);
        if matches {
            // 会话由调用方关闭；这里只清零并丢弃它的弹窗秘密。
            self.shell_state.ssh_credentials.take();
        }
        matches
    }

    /// 从当前作用域的 local binding 与 Credential Manager 重新组装一次性
    /// transport credential。任何缺失/损坏都返回空，让调用方提示用户；
    /// 这里不缓存 SecretString，host-key 确认后的重试也会再次读取 vault。
    fn load_saved_ssh_credential(
        &self,
        profile: &ssh::SshProfile,
    ) -> Result<Option<lumen_ssh::Credential>, ()> {
        let Some(binding) = self
            .ssh_store
            .as_ref()
            .and_then(|store| store.binding(&profile.id))
        else {
            return Ok(None);
        };
        let read_bound_secret = |raw: &str, expected_slot: ssh::CredentialSlot| -> Result<_, ()> {
            let reference = raw.parse::<ssh::CredentialReference>().map_err(|_| ())?;
            if reference.profile_id() != profile.id || reference.slot() != expected_slot {
                return Err(());
            }
            ssh::read_secret(&reference).map_err(|_| ())
        };

        match profile.auth_method {
            ssh::AuthMethod::Password => {
                let Some(raw) = binding.password_credential_ref.as_deref() else {
                    return Ok(None);
                };
                let Some(secret) = read_bound_secret(raw, ssh::CredentialSlot::Password)? else {
                    return Ok(None);
                };
                if secret.is_empty() {
                    return Ok(None);
                }
                Ok(Some(lumen_ssh::Credential::password(secret)))
            }
            ssh::AuthMethod::PrivateKey => {
                let Some(path) = binding.private_key_path.as_ref() else {
                    return Ok(None);
                };
                if !path.is_absolute() || !path.is_file() {
                    return Ok(None);
                }
                let passphrase = match binding.key_passphrase_credential_ref.as_deref() {
                    Some(raw) => {
                        let Some(secret) =
                            read_bound_secret(raw, ssh::CredentialSlot::KeyPassphrase)?
                        else {
                            return Ok(None);
                        };
                        if secret.is_empty() {
                            return Ok(None);
                        }
                        Some(secret)
                    }
                    None => None,
                };
                Ok(Some(lumen_ssh::Credential::private_key(
                    path.clone(),
                    passphrase,
                )))
            }
            ssh::AuthMethod::Agent => Ok(Some(lumen_ssh::Credential::agent())),
        }
    }

    fn begin_ssh_profile_connect(&mut self, profile_id: &str) {
        let profile = self
            .ssh_store
            .as_ref()
            .and_then(|store| store.inventory().profile(profile_id))
            .cloned();
        let Some(profile) = profile else {
            self.shell_state.toast.push(
                shell::toast::ToastKind::Error,
                "SSH 服务器配置不存在或存储不可用",
            );
            return;
        };

        let (session_id, intent) = self.ssh_runtime.select_for_connect(&profile);
        self.apply_ssh_connect_intent(session_id, &profile, intent);
        self.update_window_title();
        self.window.request_redraw();
    }

    fn continue_ssh_profile_connect(
        &mut self,
        session_id: ssh_runtime::SshSessionId,
        profile_id: &str,
    ) {
        let profile = self
            .ssh_store
            .as_ref()
            .and_then(|store| store.inventory().profile(profile_id))
            .cloned();
        let Some(profile) = profile else {
            self.ssh_runtime.close_session(session_id);
            self.shell_state.toast.push(
                shell::toast::ToastKind::Error,
                "SSH 服务器配置不存在或存储不可用",
            );
            return;
        };
        let Some(intent) = self.ssh_runtime.connect_intent(session_id, &profile) else {
            self.shell_state
                .toast
                .push(shell::toast::ToastKind::Error, "SSH 会话已关闭，请重新连接");
            return;
        };
        self.apply_ssh_connect_intent(session_id, &profile, intent);
        self.window.request_redraw();
    }

    fn apply_ssh_connect_intent(
        &mut self,
        session_id: ssh_runtime::SshSessionId,
        profile: &ssh::SshProfile,
        intent: ssh_runtime::ConnectIntent,
    ) {
        use ssh_runtime::ConnectIntent;
        match intent {
            ConnectIntent::AlreadyRunning => {
                self.terminal_focused = self.terminal_focus_allowed();
            }
            ConnectIntent::Password => {
                if self.ssh_force_credential_prompt.contains(&profile.id) {
                    self.open_ssh_credential_dialog(
                        session_id,
                        profile,
                        shell::SshCredentialKind::Password,
                    );
                } else {
                    match self.load_saved_ssh_credential(profile) {
                        Ok(Some(credential)) => {
                            self.start_ssh_connection(session_id, profile, credential);
                        }
                        Ok(None) => self.open_ssh_credential_dialog(
                            session_id,
                            profile,
                            shell::SshCredentialKind::Password,
                        ),
                        Err(()) => {
                            log::warn!("读取 SSH 本机密码凭据失败，将要求重新输入");
                            self.ssh_force_credential_prompt.insert(profile.id.clone());
                            self.shell_state.toast.push(
                                shell::toast::ToastKind::Warn,
                                "本机 SSH 凭据不可用，请重新输入",
                            );
                            self.open_ssh_credential_dialog(
                                session_id,
                                profile,
                                shell::SshCredentialKind::Password,
                            );
                        }
                    }
                }
            }
            ConnectIntent::PrivateKey => {
                if self.ssh_force_credential_prompt.contains(&profile.id) {
                    self.open_ssh_credential_dialog(
                        session_id,
                        profile,
                        shell::SshCredentialKind::PrivateKey,
                    );
                } else {
                    match self.load_saved_ssh_credential(profile) {
                        Ok(Some(credential)) => {
                            self.start_ssh_connection(session_id, profile, credential);
                        }
                        Ok(None) => self.open_ssh_credential_dialog(
                            session_id,
                            profile,
                            shell::SshCredentialKind::PrivateKey,
                        ),
                        Err(()) => {
                            log::warn!("读取 SSH 本机私钥口令失败，将要求重新输入");
                            self.ssh_force_credential_prompt.insert(profile.id.clone());
                            self.shell_state.toast.push(
                                shell::toast::ToastKind::Warn,
                                "本机 SSH 凭据不可用，请重新输入",
                            );
                            self.open_ssh_credential_dialog(
                                session_id,
                                profile,
                                shell::SshCredentialKind::PrivateKey,
                            );
                        }
                    }
                }
            }
            ConnectIntent::Agent => {
                self.start_ssh_connection(session_id, profile, lumen_ssh::Credential::agent());
            }
            ConnectIntent::AwaitingHostKey | ConnectIntent::HostKeyChanged => {
                self.terminal_focused = false;
            }
        }
    }

    fn start_ssh_connection(
        &mut self,
        session_id: ssh_runtime::SshSessionId,
        profile: &ssh::SshProfile,
        credential: lumen_ssh::Credential,
    ) {
        match self.ssh_runtime.start(session_id, profile, credential) {
            Ok(()) => {
                self.terminal_focused = false;
                self.window.request_redraw();
            }
            Err(error) => {
                self.terminal_focused = false;
                self.shell_state
                    .toast
                    .push(shell::toast::ToastKind::Error, error);
                self.window.request_redraw();
            }
        }
    }

    /// 把对话框 submission 事务性保存到本机安全存储并组装本次连接凭据。
    ///
    /// 顺序固定为：写新随机 Credential Manager target → 原子提交 binding →
    /// 删除旧 target。binding 失败时立即幂等删除新 target。任何严格复核失败
    /// 都发生在写 secret 之前。
    fn save_ssh_credential_submission(
        &mut self,
        profile: &ssh::SshProfile,
        mut submission: shell::SshCredentialSubmission,
    ) -> Result<lumen_ssh::Credential, String> {
        let expected_kind = match profile.auth_method {
            ssh::AuthMethod::Password => shell::SshCredentialKind::Password,
            ssh::AuthMethod::PrivateKey => shell::SshCredentialKind::PrivateKey,
            ssh::AuthMethod::Agent => {
                log::warn!("拒绝为 SSH agent 认证保存交互式凭据");
                return Err(i18n::strings().ssh_cred_toast_stale.to_owned());
            }
        };
        if !submission.matches_target(
            &profile.id,
            &profile.host,
            profile.port,
            &profile.username,
            expected_kind,
        ) {
            log::warn!("拒绝已过期或认证方式不匹配的 SSH 凭据提交");
            return Err(i18n::strings().ssh_cred_toast_stale.to_owned());
        }

        let previous = self
            .ssh_store
            .as_ref()
            .and_then(|store| store.binding(&profile.id))
            .cloned();
        let mut binding = previous.clone().unwrap_or_else(|| ssh::SshLocalBinding {
            profile_id: profile.id.clone(),
            private_key_path: None,
            password_credential_ref: None,
            key_passphrase_credential_ref: None,
        });

        let credential = match submission.kind() {
            shell::SshCredentialKind::Password => {
                if submission.password().is_empty() {
                    return Err(i18n::strings().ssh_cred_toast_empty_password.to_owned());
                }
                let reference =
                    ssh::CredentialReference::password(&profile.id).map_err(|error| {
                        log::warn!("创建 SSH 密码凭据引用失败: {error}");
                        i18n::strings().ssh_cred_toast_invalid_id.to_owned()
                    })?;
                binding.private_key_path = None;
                binding.password_credential_ref = Some(reference.target());
                binding.key_passphrase_credential_ref = None;
                let transaction =
                    ssh::write_secret_with_commit(&reference, submission.password(), || {
                        self.ssh_store
                            .as_mut()
                            .ok_or(())
                            .and_then(|store| store.upsert_binding(binding).map_err(|_| ()))
                    });
                if let Err(error) = transaction {
                    return Err(match error {
                        ssh::CredentialTransactionError::Write(error) => {
                            log::warn!("写入 SSH 本机密码凭据失败: {error}");
                            i18n::fmt1(i18n::strings().ssh_cred_toast_write_failed_fmt, error)
                        }
                        ssh::CredentialTransactionError::Commit { rollback_error, .. } => {
                            log::warn!("提交 SSH 本机密码绑定失败");
                            if let Some(error) = rollback_error {
                                log::warn!("回滚新 SSH 本机密码凭据失败: {error}");
                            }
                            i18n::strings().ssh_cred_toast_commit_failed.to_owned()
                        }
                    });
                }
                lumen_ssh::Credential::password(submission.take_password())
            }
            shell::SshCredentialKind::PrivateKey => {
                let Some(path) = submission.private_key_path() else {
                    return Err(i18n::strings().ssh_cred_toast_stale.to_owned());
                };
                if !path.is_absolute() || !path.is_file() {
                    return Err(i18n::strings().ssh_cred_toast_stale.to_owned());
                }
                let new_reference = if submission.key_passphrase().is_empty() {
                    None
                } else {
                    let reference =
                        ssh::CredentialReference::key_passphrase(&profile.id).map_err(|error| {
                            log::warn!("创建 SSH 私钥口令引用失败: {error}");
                            i18n::strings().ssh_cred_toast_invalid_id.to_owned()
                        })?;
                    Some(reference)
                };
                binding.private_key_path = submission.private_key_path().map(ToOwned::to_owned);
                binding.password_credential_ref = None;
                binding.key_passphrase_credential_ref =
                    new_reference.as_ref().map(ssh::CredentialReference::target);
                let transaction = match new_reference.as_ref() {
                    Some(reference) => ssh::write_secret_with_commit(
                        reference,
                        submission.key_passphrase(),
                        || {
                            self.ssh_store
                                .as_mut()
                                .ok_or(())
                                .and_then(|store| store.upsert_binding(binding).map_err(|_| ()))
                        },
                    ),
                    None => self
                        .ssh_store
                        .as_mut()
                        .ok_or(ssh::CredentialTransactionError::Commit {
                            error: (),
                            rollback_error: None,
                        })
                        .and_then(|store| {
                            store.upsert_binding(binding).map_err(|_| {
                                ssh::CredentialTransactionError::Commit {
                                    error: (),
                                    rollback_error: None,
                                }
                            })
                        }),
                };
                if let Err(error) = transaction {
                    match error {
                        ssh::CredentialTransactionError::Write(error) => {
                            log::warn!("写入 SSH 本机私钥口令失败: {error}");
                        }
                        ssh::CredentialTransactionError::Commit { rollback_error, .. } => {
                            log::warn!("提交 SSH 本机私钥绑定失败");
                            if let Some(error) = rollback_error {
                                log::warn!("回滚新 SSH 本机私钥口令失败: {error}");
                            }
                        }
                    }
                    return Err(match error {
                        ssh::CredentialTransactionError::Write(error) => {
                            log::warn!("写入 SSH 本机私钥口令失败: {error}");
                            i18n::fmt1(i18n::strings().ssh_cred_toast_write_failed_fmt, error)
                        }
                        ssh::CredentialTransactionError::Commit { .. } => {
                            i18n::strings().ssh_cred_toast_commit_failed.to_owned()
                        }
                    });
                }
                let path = submission
                    .take_private_key_path()
                    .expect("私钥路径已在上方严格校验");
                let passphrase = (!submission.key_passphrase().is_empty())
                    .then(|| lumen_ssh::SecretString::new(submission.take_key_passphrase()));
                lumen_ssh::Credential::private_key(path, passphrase)
            }
        };

        let current = self
            .ssh_store
            .as_ref()
            .and_then(|store| store.binding(&profile.id))
            .cloned();
        self.delete_obsolete_ssh_secrets(previous.as_ref(), current.as_ref());
        self.ssh_force_credential_prompt.remove(&profile.id);
        Ok(credential)
    }

    fn apply_ssh_runtime_action(&mut self, action: shell::SshRuntimeAction) {
        use shell::SshRuntimeAction;
        match action {
            SshRuntimeAction::NewSession { profile_id } => {
                self.shell_state
                    .ssh_ui
                    .select_profile(Some(profile_id.clone()));
                self.begin_ssh_profile_connect(&profile_id);
            }
            SshRuntimeAction::ConnectWithCredential {
                session_id,
                submission,
            } => {
                let profile_id = submission.profile_id().to_owned();
                let profile = self
                    .ssh_store
                    .as_ref()
                    .and_then(|store| store.inventory().profile(&profile_id))
                    .cloned();
                let Some(profile) = profile else {
                    self.ssh_runtime.close_session(session_id);
                    self.shell_state
                        .toast
                        .push(shell::toast::ToastKind::Error, "SSH 服务器配置已不存在");
                    return;
                };
                if !self
                    .ssh_runtime
                    .accepts_credential_for(session_id, &profile)
                {
                    self.ssh_runtime.close_session(session_id);
                    self.shell_state.toast.push(
                        shell::toast::ToastKind::Error,
                        "SSH 凭据请求已过期，请重新连接",
                    );
                    return;
                }
                match self.save_ssh_credential_submission(&profile, submission) {
                    Ok(credential) => {
                        self.start_ssh_connection(session_id, &profile, credential);
                    }
                    Err(reason) => {
                        // 失败原因直接上 toast（写入凭据管理器/配置变更/ID 不合法
                        // 等），不再用一句笼统文案让用户无从判断（海风哥反馈）。
                        self.shell_state
                            .toast
                            .push(shell::toast::ToastKind::Error, reason);
                        let kind = match profile.auth_method {
                            ssh::AuthMethod::Password => shell::SshCredentialKind::Password,
                            ssh::AuthMethod::PrivateKey => shell::SshCredentialKind::PrivateKey,
                            ssh::AuthMethod::Agent => {
                                self.ssh_runtime.close_session(session_id);
                                return;
                            }
                        };
                        self.open_ssh_credential_dialog(session_id, &profile, kind);
                    }
                }
            }
            SshRuntimeAction::ActivateSession { session_id } => {
                if self.ssh_runtime.activate_session(session_id) {
                    let active_profile_id = self
                        .ssh_runtime
                        .active_view()
                        .map(|view| view.profile_id.clone());
                    if let Some(active_profile_id) = active_profile_id {
                        self.shell_state
                            .ssh_ui
                            .select_profile(Some(active_profile_id));
                    }
                    self.terminal_focused = self.ssh_runtime.active_accepts_input();
                    self.update_window_title();
                    self.window.request_redraw();
                }
            }
            SshRuntimeAction::CloseSession { session_id } => {
                let cleared_dialog = self.clear_ssh_credential_dialog_for_session(session_id);
                if self.ssh_runtime.close_session(session_id) || cleared_dialog {
                    let active_profile_id = self
                        .ssh_runtime
                        .active_view()
                        .map(|view| view.profile_id.clone());
                    if let Some(active_profile_id) = active_profile_id {
                        self.shell_state
                            .ssh_ui
                            .select_profile(Some(active_profile_id));
                    }
                    self.terminal_focused = self.ssh_runtime.active_accepts_input();
                    self.ssh_rect_px = None;
                    self.update_window_title();
                    self.window.request_redraw();
                }
            }
            SshRuntimeAction::RenameSession { session_id, name } => {
                if self.ssh_runtime.rename_session(session_id, &name) {
                    self.update_window_title();
                    self.window.request_redraw();
                }
            }
            SshRuntimeAction::Disconnect => {
                self.ssh_runtime.disconnect_active();
                self.terminal_focused = false;
                self.window.request_redraw();
            }
            SshRuntimeAction::DisconnectSession { session_id } => {
                let disconnected_active = self
                    .ssh_runtime
                    .active_view()
                    .is_some_and(|view| view.session_id == session_id);
                self.ssh_runtime.disconnect_session(session_id);
                if disconnected_active {
                    self.terminal_focused = false;
                }
                self.window.request_redraw();
            }
            SshRuntimeAction::ReconnectSession { session_id } => {
                let profile_id = self
                    .ssh_runtime
                    .session_views()
                    .into_iter()
                    .find(|view| view.session_id == session_id)
                    .map(|view| view.profile_id);
                if let Some(profile_id) = profile_id {
                    self.continue_ssh_profile_connect(session_id, &profile_id);
                }
                self.terminal_focused = self.ssh_runtime.active_accepts_input();
                self.window.request_redraw();
            }
            SshRuntimeAction::KillProcess {
                session_id,
                pid,
                force,
            } => {
                if let Err(error) = self.ssh_runtime.kill_process(session_id, pid, force) {
                    self.shell_state
                        .toast
                        .push(shell::toast::ToastKind::Error, error);
                }
                self.window.request_redraw();
            }
            SshRuntimeAction::QueryPort { session_id, port } => {
                if let Err(error) = self.ssh_runtime.query_port(session_id, port) {
                    self.shell_state
                        .toast
                        .push(shell::toast::ToastKind::Error, error);
                }
                self.window.request_redraw();
            }
            SshRuntimeAction::SearchProcesses { session_id, query } => {
                if let Err(error) = self.ssh_runtime.search_processes(session_id, &query) {
                    self.shell_state
                        .toast
                        .push(shell::toast::ToastKind::Error, error);
                }
                self.window.request_redraw();
            }
            SshRuntimeAction::DismissKillFeedback { session_id } => {
                if self.ssh_runtime.dismiss_kill_feedback(session_id) {
                    self.window.request_redraw();
                }
            }
            SshRuntimeAction::DismissHostKey { session_id } => {
                self.ssh_runtime.dismiss_unknown_host_key(session_id);
                self.terminal_focused = false;
                self.window.request_redraw();
            }
            SshRuntimeAction::TrustHostKey {
                session_id,
                profile_id,
                algorithm,
                fingerprint,
            } => {
                let current_profile = self
                    .ssh_store
                    .as_ref()
                    .and_then(|store| store.inventory().profile(&profile_id))
                    .cloned();
                let exact_pending = current_profile.as_ref().is_some_and(|profile| {
                    let trust_is_current = ssh_host_key_confirmation_is_current(
                        profile.trusted_host_key.as_ref(),
                        &algorithm,
                        &fingerprint,
                    );
                    trust_is_current
                        && self.ssh_runtime.unknown_host_key_matches(
                            session_id,
                            profile,
                            &algorithm,
                            &fingerprint,
                        )
                });
                if !exact_pending {
                    self.ssh_runtime.dismiss_unknown_host_key(session_id);
                    self.shell_state.toast.push(
                        shell::toast::ToastKind::Error,
                        "SSH 主机密钥确认已过期，请重新连接",
                    );
                    return;
                }
                let Some(profile) = current_profile else {
                    return;
                };
                let draft = ssh_profile_draft(
                    &profile,
                    Some(ssh::HostKeyTrust {
                        algorithm: algorithm.clone(),
                        fingerprint: fingerprint.clone(),
                    }),
                );
                let result = self
                    .ssh_store
                    .as_mut()
                    .ok_or_else(|| "SSH 配置存储不可用".to_owned())
                    .and_then(|store| {
                        store
                            .update_profile(&profile_id, draft)
                            .map_err(|error| error.to_string())
                    });
                if let Err(error) = result {
                    self.shell_state
                        .toast
                        .push(shell::toast::ToastKind::Error, error);
                    return;
                }
                self.notify_ssh_local_change();
                if self
                    .ssh_runtime
                    .confirm_unknown_host_key(session_id, &algorithm, &fingerprint)
                {
                    // Never cache/reuse the credential that reached the first
                    // host-key probe. Password/private-key modes prompt again.
                    self.continue_ssh_profile_connect(session_id, &profile_id);
                }
            }
        }
    }

    fn drain_ssh_runtime(&mut self) {
        let mut outcome = self.ssh_runtime.drain();
        for profile_id in &outcome.credential_failures {
            self.ssh_force_credential_prompt.insert(profile_id.clone());
        }
        for profile_id in &outcome.connected_profiles {
            self.ssh_force_credential_prompt.remove(profile_id);
        }
        if self.settings.layout.view_mode.is_ssh() && self.ssh_runtime.active_blocks_input() {
            self.terminal_focused = false;
        } else if outcome.active_became_connected
            && self.settings.layout.view_mode.is_ssh()
            && self.ssh_runtime.active_accepts_input()
            && self.shell_state.ssh_credentials.is_none()
        {
            self.terminal_focused = true;
        }
        if !self.app_lock.is_locked()
            && self.settings.layout.view_mode.is_ssh()
            && (outcome.active_terminal_changed
                || outcome.active_status_changed
                || outcome.sessions_changed
                || outcome.connection_test_changed)
        {
            self.window.request_redraw();
        }
        if outcome.sessions_changed && self.settings.layout.view_mode.is_ssh() {
            self.update_window_title();
        }
        if !outcome.file_events.is_empty() {
            self.apply_ssh_file_runtime_events(std::mem::take(&mut outcome.file_events));
        }
        if !outcome.editor_invalidated_sessions.is_empty() {
            self.invalidate_ssh_clipboard_exports(&outcome.editor_invalidated_sessions);
            let runtime_id = self.ssh_runtime.runtime_id();
            for session_id in outcome.editor_invalidated_sessions {
                let sources = self
                    .shell_state
                    .text_editor
                    .sources()
                    .filter(|source| {
                        matches!(
                            source,
                            shell::text_editor::TextFileSource::Ssh {
                                runtime_id: source_runtime,
                                session_id: source_session,
                                ..
                            } if *source_runtime == runtime_id && *source_session == session_id
                        )
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                for source in sources {
                    self.shell_state
                        .text_editor
                        .invalidate_source(&source, i18n::strings().text_editor_source_invalidated);
                }
            }
            self.window.request_redraw();
        }
    }

    /// SSH 文件树“复制”的唯一入口：内部引用供 Lumen 三种模式互粘，同时
    /// 预下载到独立临时目录；下载完成后才以 CF_HDROP 交给 Windows 资源管理器。
    fn copy_ssh_file_to_clipboards(&mut self, item: SshClipboardItem) {
        self.ssh_file_clipboard = Some(item.clone());
        self.remote_ws.clear_file_clipboard();
        self.remote_ws.cancel_clip_dir();
        if let Some(service) = self.clipboard_svc.as_ref() {
            service.clear();
        }
        let mut clipboard_sequence = system_clipboard_sequence_number();
        if let Some(clipboard) = self.clipboard.as_mut() {
            if clipboard.clear().is_ok() {
                clipboard_sequence = system_clipboard_sequence_number().or(clipboard_sequence);
            }
        }

        self.ssh_clipboard_export_generation =
            next_ssh_clipboard_generation(self.ssh_clipboard_export_generation);
        let generation = self.ssh_clipboard_export_generation;
        let destination = match create_ssh_clipboard_staging_path(generation, &item) {
            Ok(path) => path,
            Err(error) => {
                self.shell_state.toast.push(
                    shell::toast::ToastKind::Error,
                    i18n::fmt1(i18n::strings().ssh_clipboard_prepare_failed_fmt, error),
                );
                self.window.request_redraw();
                return;
            }
        };
        self.ssh_clipboard_exports.insert(
            destination.clone(),
            SshClipboardExport {
                generation,
                session_id: item.session_id,
                clipboard_sequence,
            },
        );
        if let Err(error) =
            self.ssh_runtime
                .download_file(item.session_id, item.path, destination.clone(), false)
        {
            self.ssh_clipboard_exports.remove(&destination);
            remove_ssh_clipboard_staging_path(&destination);
            self.shell_state.toast.push(
                shell::toast::ToastKind::Error,
                i18n::fmt1(i18n::strings().ssh_clipboard_prepare_failed_fmt, error),
            );
            self.window.request_redraw();
            return;
        }
        self.shell_state.toast.push(
            shell::toast::ToastKind::Info,
            i18n::fmt1(i18n::strings().ssh_clipboard_preparing_fmt, item.name),
        );
        self.window.request_redraw();
    }

    /// 返回 `true` 表示该下载属于 SSH→系统剪贴板链路，调用方不得再按普通
    /// SSH 下载刷新文件树或显示“下载完成”通用提示。
    fn complete_ssh_clipboard_export(&mut self, local_path: std::path::PathBuf) -> bool {
        let Some(export) = self.ssh_clipboard_exports.remove(&local_path) else {
            return false;
        };
        if !ssh_clipboard_export_is_current(export.generation, self.ssh_clipboard_export_generation)
        {
            remove_ssh_clipboard_staging_path(&local_path);
            return true;
        }
        if clipboard_changed_since(
            export.clipboard_sequence,
            system_clipboard_sequence_number(),
        ) {
            remove_ssh_clipboard_staging_path(&local_path);
            self.shell_state.toast.push(
                shell::toast::ToastKind::Info,
                i18n::strings().ssh_clipboard_changed,
            );
            return true;
        }

        if clipboard_files::copy_files(std::slice::from_ref(&local_path)) {
            if let Some(previous) = self.ssh_clipboard_ready_path.replace(local_path.clone()) {
                if previous != local_path {
                    remove_ssh_clipboard_staging_path(&previous);
                }
            }
            self.shell_state.toast.push(
                shell::toast::ToastKind::Info,
                i18n::strings().ssh_clipboard_ready,
            );
        } else {
            remove_ssh_clipboard_staging_path(&local_path);
            self.shell_state.toast.push(
                shell::toast::ToastKind::Error,
                i18n::strings().ssh_clipboard_write_failed,
            );
        }
        true
    }

    /// 返回 `true` 表示该失败属于 SSH→系统剪贴板链路。旧代次静默清理；
    /// 只有当前复制显示错误，避免连续复制时旧任务失败干扰最新结果。
    fn fail_ssh_clipboard_export(&mut self, local_path: &std::path::Path, message: &str) -> bool {
        let Some(export) = self.ssh_clipboard_exports.remove(local_path) else {
            return false;
        };
        remove_ssh_clipboard_staging_path(local_path);
        if ssh_clipboard_export_is_current(export.generation, self.ssh_clipboard_export_generation)
        {
            self.shell_state.toast.push(
                shell::toast::ToastKind::Error,
                i18n::fmt1(i18n::strings().ssh_clipboard_prepare_failed_fmt, message),
            );
        }
        true
    }

    fn invalidate_ssh_clipboard_exports(&mut self, session_ids: &[ssh_runtime::SshSessionId]) {
        let paths = self
            .ssh_clipboard_exports
            .iter()
            .filter(|(_, export)| session_ids.contains(&export.session_id))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        for path in paths {
            let _ = self.fail_ssh_clipboard_export(&path, "SSH 连接已断开");
        }
    }

    fn apply_ssh_filetree_intent(&mut self, intent: shell::ssh_filetree::SshFileTreeIntent) {
        use shell::ssh_filetree::SshFileTreeIntent;
        match intent {
            SshFileTreeIntent::Select {
                session_id,
                path,
                is_directory,
            } => {
                self.ssh_runtime.select_file(session_id, path, is_directory);
            }
            SshFileTreeIntent::ClearSelection { session_id } => {
                self.ssh_runtime.clear_file_selection(session_id);
            }
            SshFileTreeIntent::ToggleDirectory { session_id, path } => {
                if self.ssh_runtime.toggle_directory(session_id, &path) {
                    self.window.request_redraw();
                }
            }
            SshFileTreeIntent::RefreshDirectory { session_id, path } => {
                if self.ssh_runtime.refresh_directory(session_id, &path) {
                    self.window.request_redraw();
                }
            }
            SshFileTreeIntent::ChangeDirectory { session_id, path } => {
                let command = shell::filetree::cd_command_posix(&path);
                if !command.is_empty() {
                    match self.ssh_runtime.send_input_to(session_id, command) {
                        Ok(()) => self.terminal_focused = true,
                        Err(error) => self
                            .shell_state
                            .toast
                            .push(shell::toast::ToastKind::Error, error),
                    }
                }
            }
            SshFileTreeIntent::OpenLocalCopy {
                session_id,
                path,
                name,
                ..
            } => {
                if let Err(error) = self.ssh_runtime.open_local_copy(session_id, path, &name) {
                    self.shell_state
                        .toast
                        .push(shell::toast::ToastKind::Error, error);
                }
            }
            SshFileTreeIntent::Edit {
                session_id, path, ..
            } => {
                self.open_text_editor(shell::text_editor::TextFileSource::Ssh {
                    runtime_id: self.ssh_runtime.runtime_id(),
                    session_id,
                    path,
                });
            }
            SshFileTreeIntent::Search { session_id, query } => {
                if let Err(error) = self.ssh_runtime.search_files(session_id, query) {
                    self.shell_state
                        .toast
                        .push(shell::toast::ToastKind::Error, error);
                }
                self.window.request_redraw();
            }
            SshFileTreeIntent::CopyFiles {
                session_id,
                path,
                name,
                is_directory: _,
                size,
            } => {
                self.copy_ssh_file_to_clipboards(SshClipboardItem {
                    session_id,
                    path,
                    name,
                    size,
                });
            }
            SshFileTreeIntent::PasteInto {
                session_id,
                directory,
            } => {
                self.paste_into_ssh(session_id, directory);
            }
            SshFileTreeIntent::MoveEntry {
                session_id,
                source_path,
                source_is_directory,
                target_directory,
            } => {
                if let Err(error) = self.ssh_runtime.move_entry(
                    session_id,
                    source_path,
                    source_is_directory,
                    target_directory,
                ) {
                    self.shell_state
                        .toast
                        .push(shell::toast::ToastKind::Error, error);
                }
                self.window.request_redraw();
            }
            SshFileTreeIntent::CreateEntry { .. }
            | SshFileTreeIntent::Delete { .. }
            | SshFileTreeIntent::Rename { .. } => {
                // The shell intercepts these intents and opens the shared
                // create/rename/permanent-delete modal before they reach main.
            }
        }
    }

    fn apply_ssh_file_runtime_events(&mut self, events: Vec<ssh_runtime::SshFileRuntimeEvent>) {
        use shell::text_editor::{SaveFailure, TextFileSource};
        let runtime_id = self.ssh_runtime.runtime_id();
        for event in events {
            match event {
                ssh_runtime::SshFileRuntimeEvent::TextLoaded {
                    session_id,
                    document_token,
                    bytes,
                    ..
                } => {
                    let source_matches = matches!(
                        self.shell_state
                            .text_editor
                            .source_for_token(document_token),
                        Some(TextFileSource::Ssh {
                            runtime_id: source_runtime,
                            session_id: source_session,
                            ..
                        }) if *source_runtime == runtime_id && *source_session == session_id
                    );
                    if source_matches {
                        self.shell_state
                            .text_editor
                            .apply_loaded(document_token, Ok(bytes));
                    }
                }
                ssh_runtime::SshFileRuntimeEvent::TextSaved {
                    session_id,
                    document_token,
                    ..
                } => {
                    let source_matches = matches!(
                        self.shell_state
                            .text_editor
                            .source_for_token(document_token),
                        Some(TextFileSource::Ssh {
                            runtime_id: source_runtime,
                            session_id: source_session,
                            ..
                        }) if *source_runtime == runtime_id && *source_session == session_id
                    );
                    if source_matches {
                        self.shell_state
                            .text_editor
                            .apply_saved(document_token, Ok(()));
                    }
                }
                ssh_runtime::SshFileRuntimeEvent::TextConflict {
                    session_id,
                    document_token,
                } => {
                    let source_matches = matches!(
                        self.shell_state
                            .text_editor
                            .source_for_token(document_token),
                        Some(TextFileSource::Ssh {
                            runtime_id: source_runtime,
                            session_id: source_session,
                            ..
                        }) if *source_runtime == runtime_id && *source_session == session_id
                    );
                    if source_matches {
                        self.shell_state
                            .text_editor
                            .apply_saved(document_token, Err(SaveFailure::Conflict));
                    }
                }
                ssh_runtime::SshFileRuntimeEvent::LocalCopyReady { local_path, .. } => {
                    shell::filetree::open_with_default(&local_path);
                }
                ssh_runtime::SshFileRuntimeEvent::OperationComplete {
                    operation,
                    local_path,
                    ..
                } => {
                    if operation == lumen_ssh::FileOperation::Download {
                        if let Some(local_path) = local_path {
                            if self.complete_ssh_clipboard_export(local_path.clone()) {
                                continue;
                            }
                            if let Some(chain) = self.ssh_paste_chains.remove(&local_path) {
                                match self.ssh_runtime.upload_entry(
                                    chain.target_session_id,
                                    local_path.clone(),
                                    chain.target_directory,
                                    chain.destination_name,
                                    false,
                                ) {
                                    Ok(()) => {
                                        self.ssh_staged_uploads.insert(local_path);
                                    }
                                    Err(error) => {
                                        remove_staged_path(&local_path);
                                        self.shell_state
                                            .toast
                                            .push(shell::toast::ToastKind::Error, error);
                                    }
                                }
                            } else if let Some(parent) = local_path.parent() {
                                self.shell_state.filetree.refresh_dir(parent);
                            }
                        }
                    } else if operation == lumen_ssh::FileOperation::Upload {
                        if let Some(local_path) = local_path {
                            if self.ssh_staged_uploads.remove(&local_path) {
                                remove_staged_path(&local_path);
                            }
                        }
                    }
                    self.shell_state.toast.push(
                        shell::toast::ToastKind::Info,
                        ssh_file_operation_done_message(operation),
                    );
                }
                ssh_runtime::SshFileRuntimeEvent::Error {
                    session_id,
                    document_token,
                    operation,
                    error,
                    local_path,
                } => {
                    let message = ssh_file_error_message(operation, error);
                    if let Some(local_path) = local_path {
                        if self.fail_ssh_clipboard_export(&local_path, &message) {
                            continue;
                        }
                        self.ssh_paste_chains.remove(&local_path);
                        if self.ssh_staged_uploads.remove(&local_path) {
                            remove_staged_path(&local_path);
                        }
                    }
                    if let Some(token) = document_token {
                        let source_matches = matches!(
                            self.shell_state.text_editor.source_for_token(token),
                            Some(TextFileSource::Ssh {
                                runtime_id: source_runtime,
                                session_id: source_session,
                                ..
                            }) if *source_runtime == runtime_id
                                && *source_session == session_id
                        );
                        if source_matches
                            && !self
                                .shell_state
                                .text_editor
                                .apply_saved(token, Err(SaveFailure::Message(message.clone())))
                        {
                            self.shell_state
                                .text_editor
                                .apply_loaded(token, Err(message));
                        }
                    } else {
                        self.shell_state
                            .toast
                            .push(shell::toast::ToastKind::Error, message);
                    }
                }
            }
        }
        self.window.request_redraw();
    }

    #[cfg(feature = "input-editor")]
    fn close_passive_completion(&mut self) {
        if self.shell_state.completion.passive {
            self.shell_state.completion.open = false;
            self.shell_state.completion.passive = false;
            self.shell_state.completion.selected = 0;
            self.shell_state.completion.popup_rect = None;
            self.shell_state.completion.last_scrolled_selected = None;
            self.completion_candidates.clear();
        }
    }

    #[cfg(feature = "input-editor")]
    fn show_cached_llm_slash_candidates(&mut self, ti: usize, pi: usize, prefix: &str) -> bool {
        let commands: Vec<llm_cli::SlashCommand> = self.tabs[ti].panes[pi]
            .slash_probe
            .commands
            .iter()
            .filter(|item| item.command.starts_with(prefix))
            .cloned()
            .collect();
        if commands.is_empty() {
            return false;
        }
        if ti != self.active_tab || pi != self.tabs[ti].focused {
            return true;
        }
        self.completion_candidates = commands
            .into_iter()
            .map(|item| completion::Completion {
                display: if item.description.is_empty() {
                    item.command.clone()
                } else {
                    format!("{}  {}", item.command, item.description)
                },
                replacement: item.command,
                is_dir: false,
                replace_range: Some((0, prefix.len())),
            })
            .collect();
        self.shell_state.completion.open = true;
        self.shell_state.completion.passive = true;
        self.shell_state.completion.selected = self
            .shell_state
            .completion
            .selected
            .min(self.completion_candidates.len().saturating_sub(1));
        self.terminal_focused = true;
        self.window.request_redraw();
        true
    }

    #[cfg(feature = "input-editor")]
    fn write_llm_slash_probe_clear(&mut self, ti: usize, pi: usize, context: &str) -> bool {
        if let Err(error) = self.tabs[ti].panes[pi].write_user_input(b"\x15") {
            log::error!("{context}: {error:#}");
            return false;
        }
        true
    }

    #[cfg(feature = "input-editor")]
    fn write_llm_slash_probe_escape(&mut self, ti: usize, pi: usize, context: &str) -> bool {
        if let Err(error) = self.tabs[ti].panes[pi].write_user_input(b"\x1b") {
            log::error!("{context}: {error:#}");
            return false;
        }
        true
    }

    #[cfg(feature = "input-editor")]
    fn write_llm_slash_probe_next(
        &mut self,
        ti: usize,
        pi: usize,
        steps: usize,
        context: &str,
    ) -> bool {
        let win32_input = self.tabs[ti].panes[pi].term.win32_input()
            && std::env::var_os("LUMEN_NO_WIN32_INPUT").is_none();
        let bytes = input::encode_plain_arrow_down(win32_input).repeat(steps.max(1));
        if let Err(error) = self.tabs[ti].panes[pi].write_user_input(&bytes) {
            log::error!("{context}: {error:#}");
            return false;
        }
        true
    }

    #[cfg(feature = "input-editor")]
    fn begin_llm_slash_probe_clear(
        &mut self,
        ti: usize,
        pi: usize,
        resume_after_clear: bool,
        context: &str,
    ) -> bool {
        if !self.write_llm_slash_probe_clear(ti, pi, context) {
            return false;
        }
        let probe = &mut self.tabs[ti].panes[pi].slash_probe;
        probe.stop_scan();
        probe.clearing = true;
        probe.escape_sent = false;
        probe.clear_at = Some(Instant::now() + LLM_SLASH_CLEAR_STAGE_DELAY);
        probe.resume_after_clear = resume_after_clear;
        probe.probe_deadline = None;
        self.close_passive_completion();
        true
    }

    /// 把本地编辑器中的 `/prefix` 临时镜像给 CLI，从 CLI 自己的菜单
    /// 自动遍历原生菜单并累计全部命令。遍历完成立即 Ctrl+U 清掉影子
    /// 输入；后续前缀优先筛选完整会话缓存，不让 CLI 原生菜单与 Lumen
    /// 弹层长期重叠。
    #[cfg(feature = "input-editor")]
    fn sync_llm_slash_probe(&mut self, ti: usize, pi: usize) {
        if ti >= self.tabs.len() || pi >= self.tabs[ti].panes.len() {
            return;
        }
        let is_llm = {
            let pane = &self.tabs[ti].panes[pi];
            pane.llm_cli
                .or_else(|| llm_cli::detect(None, &pane.term))
                .is_some_and(|kind| llm_cli::composer_ready(&pane.term, kind))
        };
        let next = if is_llm {
            let text = self.tabs[ti].panes[pi].editor.view().text();
            llm_cli::slash_prefix(&text).map(str::to_owned)
        } else {
            None
        };
        let current = self.tabs[ti].panes[pi].slash_probe.shadow.clone();
        let probe_active = {
            let probe = &self.tabs[ti].panes[pi].slash_probe;
            probe.scanning() || probe.clearing
        };
        let can_filter_active_probe =
            llm_cli::can_filter_active_probe(probe_active, &current, next.as_deref());
        if can_filter_active_probe {
            // 裸 `/` 的全量扫描尚未结束时，继续输入字符只改变 Lumen
            // 本地筛选条件。不要中断扫描后对 Kimi 执行 Ctrl+U→Esc→
            // 重新探测，否则每输入一个字符都会让编辑器短暂停住。
            let prefix = next.as_deref().expect("活动扫描筛选必须有斜杠前缀");
            self.completion_req_id = 0;
            self.completion_candidates.clear();
            self.shell_state.completion.open = false;
            self.shell_state.completion.passive = false;
            self.show_cached_llm_slash_candidates(ti, pi, prefix);
            self.window.request_redraw();
            return;
        }
        if next.as_deref() == (!current.is_empty()).then_some(current.as_str()) {
            return;
        }

        if !current.is_empty() {
            if self.begin_llm_slash_probe_clear(ti, pi, true, "清理 LLM CLI 斜杠探测输入失败")
            {
                return;
            }
            self.tabs[ti].panes[pi].slash_probe.clear_active();
        }
        self.close_passive_completion();

        if let Some(prefix) = next {
            self.completion_req_id = 0;
            self.completion_candidates.clear();
            self.shell_state.completion.open = false;
            self.shell_state.completion.passive = false;
            let prefix_complete = self.tabs[ti].panes[pi].slash_probe.prefix_complete(&prefix);
            if prefix_complete {
                self.show_cached_llm_slash_candidates(ti, pi, &prefix);
                return;
            }
            if let Err(error) = self.tabs[ti].panes[pi].write_user_input(prefix.as_bytes()) {
                log::error!("写入 LLM CLI 斜杠探测前缀失败: {error:#}");
                return;
            }
            self.tabs[ti].panes[pi]
                .slash_probe
                .begin_probe(prefix, Instant::now() + LLM_SLASH_PROBE_TIMEOUT);
        }
    }

    #[cfg(feature = "input-editor")]
    fn clear_llm_slash_shadow(&mut self, ti: usize, pi: usize) {
        if !self.tabs[ti].panes[pi].slash_probe.shadow.is_empty() {
            self.write_llm_slash_probe_clear(ti, pi, "提交前清理 LLM CLI 斜杠探测输入失败");
        }
        self.tabs[ti].panes[pi].slash_probe.clear();
        self.close_passive_completion();
    }

    /// PTY 输出更新后累计 CLI 当前可视菜单；第一次拿到菜单时启动自动
    /// 下移扫描，直到精确页码到尾、选中项回环或兜底停止条件触发。
    #[cfg(feature = "input-editor")]
    fn refresh_llm_slash_candidates(&mut self, ti: usize, pi: usize) {
        let prefix = self.tabs[ti].panes[pi].slash_probe.shadow.clone();
        if prefix.is_empty() {
            return;
        }
        let kind = {
            let pane = &self.tabs[ti].panes[pi];
            pane.llm_cli.or_else(|| llm_cli::detect(None, &pane.term))
        };
        let snapshot =
            kind.map(|kind| llm_cli::slash_menu(&self.tabs[ti].panes[pi].term, &prefix, kind));
        if self.tabs[ti].panes[pi].slash_probe.clearing {
            return;
        }
        let Some(snapshot) = snapshot else {
            return;
        };
        if snapshot.commands.is_empty() {
            return;
        }
        let commands_changed = {
            let probe = &mut self.tabs[ti].panes[pi].slash_probe;
            let commands_changed = probe.merge_commands(snapshot.commands.clone());
            if !probe.scanning() {
                let now = Instant::now();
                let command_count = probe.commands.len();
                probe.begin_scan(
                    &snapshot,
                    command_count,
                    now + LLM_SLASH_SCAN_INTERVAL,
                    now + LLM_SLASH_SCAN_TIMEOUT,
                );
            }
            commands_changed
        };
        if commands_changed
            || !self.shell_state.completion.open
            || !self.shell_state.completion.passive
        {
            // 首屏拿到即展示，后台滚动只负责持续追加。扫描无论是否能识别
            // 页码，都不能再阻塞菜单与本地编辑器。
            let display_prefix = {
                let text = self.tabs[ti].panes[pi].editor.view().text();
                llm_cli::slash_prefix(&text)
                    .map(str::to_owned)
                    .unwrap_or(prefix)
            };
            self.show_cached_llm_slash_candidates(ti, pi, &display_prefix);
        }
    }

    /// 定时推进斜杠菜单全量扫描与探测清理。扫描不能只依赖 PTY 输出：
    /// 下一次方向键需要稳定节拍；Ctrl+U/Esc 后也可能没有任何回显，
    /// 纯输出驱动会永久保留 shadow，让终端纹理看起来卡死。
    #[cfg(feature = "input-editor")]
    fn poll_llm_slash_probe_clear(&mut self, now: Instant) -> Option<Instant> {
        let scan_due = self
            .tabs
            .iter()
            .enumerate()
            .flat_map(|(ti, tab)| {
                tab.panes.iter().enumerate().filter_map(move |(pi, pane)| {
                    (!pane.slash_probe.clearing
                        && pane.slash_probe.scan_at.is_some_and(|at| now >= at))
                    .then_some((ti, pi))
                })
            })
            .collect::<Vec<_>>();
        for (ti, pi) in scan_due {
            let prefix = self.tabs[ti].panes[pi].slash_probe.shadow.clone();
            let kind = {
                let pane = &self.tabs[ti].panes[pi];
                pane.llm_cli.or_else(|| llm_cli::detect(None, &pane.term))
            };
            let Some(kind) = kind else {
                self.tabs[ti].panes[pi].slash_probe.stop_scan();
                continue;
            };
            let snapshot = llm_cli::slash_menu(&self.tabs[ti].panes[pi].term, &prefix, kind);
            let decision = {
                let probe = &mut self.tabs[ti].panes[pi].slash_probe;
                probe.merge_commands(snapshot.commands.clone());
                let command_count = probe.commands.len();
                probe.observe_scan(
                    &snapshot,
                    command_count,
                    now,
                    LLM_SLASH_SCAN_MAX_STEPS,
                    LLM_SLASH_SCAN_MAX_STAGNANT_STEPS,
                )
            };
            match decision {
                llm_cli::SlashScanDecision::Advance => {
                    let steps = llm_cli::slash_scan_advance_steps(kind, &snapshot);
                    if self.write_llm_slash_probe_next(ti, pi, steps, "遍历 LLM CLI 斜杠菜单失败")
                    {
                        self.tabs[ti].panes[pi]
                            .slash_probe
                            .schedule_scan(now + LLM_SLASH_SCAN_INTERVAL);
                    } else if !self.begin_llm_slash_probe_clear(
                        ti,
                        pi,
                        false,
                        "取消未完成的 LLM CLI 斜杠菜单扫描失败",
                    ) {
                        self.tabs[ti].panes[pi].slash_probe.clear_active();
                        self.close_passive_completion();
                    }
                }
                llm_cli::SlashScanDecision::Finish => {
                    self.tabs[ti].panes[pi]
                        .slash_probe
                        .mark_prefix_complete(prefix);
                    if !self.begin_llm_slash_probe_clear(
                        ti,
                        pi,
                        true,
                        "清理已完整采集的 LLM CLI 斜杠菜单失败",
                    ) {
                        self.tabs[ti].panes[pi].slash_probe.clear_active();
                        self.window.request_redraw();
                    }
                }
            }
        }

        let timed_out = self
            .tabs
            .iter()
            .enumerate()
            .flat_map(|(ti, tab)| {
                tab.panes.iter().enumerate().filter_map(move |(pi, pane)| {
                    pane.slash_probe.probe_timed_out(now).then_some((ti, pi))
                })
            })
            .collect::<Vec<_>>();
        for (ti, pi) in timed_out {
            if !self.begin_llm_slash_probe_clear(ti, pi, false, "取消超时的 LLM CLI 斜杠探测失败")
            {
                self.tabs[ti].panes[pi].slash_probe.clear_active();
                self.close_passive_completion();
            }
        }

        let mut due = Vec::new();
        for (ti, tab) in self.tabs.iter().enumerate() {
            for (pi, pane) in tab.panes.iter().enumerate() {
                let probe = &pane.slash_probe;
                if !probe.clearing {
                    continue;
                }
                let Some(at) = probe.clear_at else {
                    due.push((ti, pi, false));
                    continue;
                };
                if now < at {
                    continue;
                }
                let kind = pane.llm_cli.or_else(|| llm_cli::detect(None, &pane.term));
                let send_escape = llm_cli::slash_clear_stage(kind, probe.escape_sent)
                    == llm_cli::SlashClearStage::SendEscape;
                due.push((ti, pi, send_escape));
            }
        }

        for (ti, pi, send_escape) in due {
            if send_escape {
                if self.write_llm_slash_probe_escape(ti, pi, "关闭 Kimi 原生斜杠菜单失败")
                {
                    let probe = &mut self.tabs[ti].panes[pi].slash_probe;
                    probe.escape_sent = true;
                    probe.clear_at = Some(now + LLM_SLASH_CLEAR_STAGE_DELAY);
                } else {
                    self.tabs[ti].panes[pi].slash_probe.clear_active();
                    self.close_passive_completion();
                }
                continue;
            }

            let resume = self.tabs[ti].panes[pi].slash_probe.resume_after_clear;
            self.tabs[ti].panes[pi].slash_probe.clear_active();
            if resume {
                self.sync_llm_slash_probe(ti, pi);
            }
            self.window.request_redraw();
        }

        self.tabs
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .flat_map(|pane| {
                [
                    pane.slash_probe.clear_at,
                    pane.slash_probe.probe_deadline,
                    pane.slash_probe.scan_at,
                ]
                .into_iter()
                .flatten()
            })
            .min()
    }

    fn dispatch(
        &mut self,
        action: action::Action,
        ti: usize,
        pi: usize,
    ) -> Vec<action::StateEvent> {
        use action::{Action, StateEvent, TermAction};

        let mut events = Vec::new();

        match action {
            // ── Edit：M4.1 批D1 —— 发给编辑器状态机 ──────────────────
            #[cfg(feature = "input-editor")]
            Action::Edit(ref ea) => {
                // 双重门控：必须在 Compose 态才走编辑器路径
                let current_mode =
                    effective_session_mode(&self.tabs[ti].panes[pi], self.force_fallback);
                if current_mode == mode::InputMode::Compose {
                    // 任意编辑动作 → 退出历史导航态（设计稿 §8：编辑即回到当前）。
                    // 仅在正在导航时才重置（is_navigating 纯判断，无副作用）。
                    if self.history.is_navigating() {
                        self.history.exit_navigation();
                    }
                    // app 层 EditAction → lumen_editor::EditAction 转换
                    let editor_action = app_to_editor_action(ea);
                    let _outcome = self.tabs[ti].panes[pi].editor.apply(&editor_action);
                    // 编辑器变更驱动 request_redraw，走设计稿 §7.4「节拍纪律」；
                    // 不走 PTY debounce（编辑器修改不触发 pty 写入）。
                    self.window.request_redraw();
                    events.push(StateEvent::EditorRevision(
                        self.tabs[ti].panes[pi].editor.revision(),
                    ));
                } else {
                    log::debug!("[dispatch] Edit({ea:?}) 非 Compose 态（{current_mode:?}）拒绝");
                }
            }
            #[cfg(not(feature = "input-editor"))]
            Action::Edit(ea) => {
                log::debug!("[dispatch] Edit({ea:?}) input-editor feature 未启用，忽略");
            }

            // ── Composer：M4.1 批D1 —— 提交全链路 ────────────────────
            #[cfg(feature = "input-editor")]
            Action::Composer(ref ca) => {
                use action::ComposerAction;
                let current_mode =
                    effective_session_mode(&self.tabs[ti].panes[pi], self.force_fallback);
                match ca {
                    ComposerAction::Submit if current_mode == mode::InputMode::Compose => {
                        // M4.2 批2：续行检测——文档末尾未闭合（引号/括号/here-string/
                        // 块注释、行尾管道 `|` 或续行反引号）时，Enter 自动换行而非提交
                        // （设计稿 §4），复用 lumen-editor tokenizer 判定。LLM CLI 不套用
                        // Shell 语法续行：Enter 始终提交，Shift+Enter 负责手动换行。
                        let should_insert_newline = {
                            let pane = &self.tabs[ti].panes[pi];
                            let llm_active = pane.llm_cli.is_some()
                                || llm_cli::detect(None, &pane.term).is_some();
                            composer_should_insert_newline(
                                llm_active,
                                !pane.attachments.is_empty(),
                                pane.editor.needs_continuation(),
                            )
                        };
                        if should_insert_newline {
                            self.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::InsertNewline);
                            events.push(StateEvent::EditorRevision(
                                self.tabs[ti].panes[pi].editor.revision(),
                            ));
                            self.window.request_redraw();
                        } else {
                            // 探测前缀仍在 CLI 原生编辑器里时不能紧跟着提交：
                            // Kimi 会把 `Ctrl+U` 与命令文本并发处理，形成
                            // `/y` + `/yolo` 之类的重复。先完成定时清理，
                            // 保留本地草稿，用户下一次 Enter 再提交。
                            if !self.tabs[ti].panes[pi].slash_probe.shadow.is_empty() {
                                if !self.begin_llm_slash_probe_clear(
                                    ti,
                                    pi,
                                    false,
                                    "提交前清理 LLM CLI 斜杠探测输入失败",
                                ) {
                                    self.tabs[ti].panes[pi].slash_probe.clear_active();
                                }
                                self.window.request_redraw();
                                return events;
                            }
                            // 步骤 1：门控（双重检查，keymap 已检查过一次）
                            // 步骤 2：编码。Kimi 的单行也必须走括号粘贴，
                            // 防止最终命令重新触发其原生斜杠补全。
                            let raw_text = self.tabs[ti].panes[pi].editor.view().text();
                            let compiled_text = self.tabs[ti].panes[pi]
                                .attachments
                                .compile_prompt(&raw_text);
                            let llm_kind = {
                                let pane = &self.tabs[ti].panes[pi];
                                pane.llm_cli.or_else(|| llm_cli::detect(None, &pane.term))
                            };
                            let win32_input = self.tabs[ti].panes[pi].term.win32_input()
                                && std::env::var_os("LUMEN_NO_WIN32_INPUT").is_none();
                            self.clear_llm_slash_shadow(ti, pi);
                            let payload = encode_llm_submit(&compiled_text, llm_kind, win32_input);
                            // 步骤 3：滚动到底 + 写 PTY
                            self.tabs[ti].panes[pi].term.grid_mut().scroll_to_bottom();
                            if let Err(e) = self.tabs[ti].panes[pi].write_user_input(&payload) {
                                log::error!("提交写 PTY 失败: {e:#}");
                            }
                            // 步骤 4：清空编辑器缓冲 + 记录 pending_submit + 写历史库
                            let submitted_at = std::time::Instant::now();
                            // 取当前 cwd（OSC 9;9 上报值）
                            let cwd = self.tabs[ti].panes[pi]
                                .term
                                .cwd()
                                .map(|p| p.display().to_string());
                            // 写历史库并取条目下标（用于块闭合时回填）
                            let history_idx = self.history.append_submitted(raw_text.clone(), cwd);
                            // 退出历史导航态（提交 = 新命令基线）
                            self.history.exit_navigation();
                            // 同步 abandoned 到历史库
                            let abandoned = self.tabs[ti].panes[pi]
                                .editor
                                .abandoned()
                                .map(|s| s.to_owned());
                            self.history.set_abandoned(abandoned);
                            self.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::Clear);
                            // 清 IME preedit
                            self.tabs[ti].panes[pi].preedit = None;
                            // 清退出码角标（提交新命令时角标已无意义）
                            self.tabs[ti].panes[pi].exit_badge = None;
                            self.tabs[ti].panes[pi].pending_submit =
                                Some((raw_text.clone(), submitted_at, history_idx));
                            self.tabs[ti].panes[pi].attachments.advance_draft();
                            events.push(StateEvent::SubmittedText {
                                text: raw_text,
                                submitted_at,
                                history_idx,
                            });
                            self.window.request_redraw();
                        }
                    }
                    ComposerAction::CancelLine if current_mode == mode::InputMode::Compose => {
                        // Ctrl+C 缓冲非空：清空并存放弃稿
                        let text = self.tabs[ti].panes[pi].editor.view().text();
                        self.tabs[ti].panes[pi].editor.stash_abandoned(text.clone());
                        // 同步 abandoned 到历史库
                        self.history.set_abandoned(Some(text));
                        self.tabs[ti].panes[pi]
                            .editor
                            .apply(&lumen_editor::EditAction::Clear);
                        let session_id = self.tabs[ti].panes[pi].id;
                        let discarded = self.tabs[ti].panes[pi].attachments.discard_draft();
                        self.attachment_textures
                            .retain(|(sid, _), _| *sid != session_id);
                        for path in discarded {
                            if let Err(error) = std::fs::remove_file(&path) {
                                if error.kind() != std::io::ErrorKind::NotFound {
                                    log::warn!(
                                        "清理已取消的图片附件失败 {}: {error}",
                                        path.display()
                                    );
                                }
                            }
                        }
                        self.window.request_redraw();
                        events.push(StateEvent::EditorRevision(
                            self.tabs[ti].panes[pi].editor.revision(),
                        ));
                    }
                    ComposerAction::HistoryPrev if current_mode == mode::InputMode::Compose => {
                        // ↑ 历史向上导航（M4.1 批D2）
                        // 同步 abandoned 到历史库（每次进入导航前刷新）
                        let abandoned = self.tabs[ti].panes[pi]
                            .editor
                            .abandoned()
                            .map(|s| s.to_owned());
                        self.history.set_abandoned(abandoned);
                        let current = self.tabs[ti].panes[pi].editor.view().text();
                        if let Some(text) = self.history.navigate_up(&current) {
                            self.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::SetText(text));
                            // 光标移到行末（历史条目视觉跟手）
                            self.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::Move {
                                    motion: lumen_editor::Motion::DocEnd,
                                    extend: false,
                                });
                            self.window.request_redraw();
                            events.push(StateEvent::EditorRevision(
                                self.tabs[ti].panes[pi].editor.revision(),
                            ));
                        }
                    }
                    ComposerAction::HistoryNext if current_mode == mode::InputMode::Compose => {
                        // ↓ 历史向下导航（M4.1 批D2）
                        if let Some(text) = self.history.navigate_down() {
                            self.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::SetText(text));
                            self.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::Move {
                                    motion: lumen_editor::Motion::DocEnd,
                                    extend: false,
                                });
                            self.window.request_redraw();
                            events.push(StateEvent::EditorRevision(
                                self.tabs[ti].panes[pi].editor.revision(),
                            ));
                        }
                    }
                    _ => {
                        log::debug!(
                            "[dispatch] Composer({ca:?}) 非 Compose 态（{current_mode:?}）或占位 variant，忽略"
                        );
                    }
                }
            }
            #[cfg(not(feature = "input-editor"))]
            Action::Composer(ca) => {
                log::debug!("[dispatch] Composer({ca:?}) input-editor feature 未启用，忽略");
            }

            // ── Term：本批完整实现 ─────────────────────────────────────
            Action::Term(ta) => match ta {
                TermAction::Interrupt => {
                    // M5.3 part4：镜像视图生效（控制中+远程视图）则把中断转发给被控端。
                    if self.is_mirror_active() {
                        self.remote_ws.send_input(b"\x03");
                    } else if let Err(e) = self.tabs[ti].panes[pi].write_user_input(b"\x03") {
                        log::error!("写入 PTY 失败（Interrupt）: {e:#}");
                    }
                }

                TermAction::SendKey(ks) => {
                    // KeyStroke → winit KeyEvent 的反向转换（批D 可能走此路径）
                    // 批B 暂时通过 PassThrough 路径处理，此分支为 M4 远程预留。
                    log::debug!("[dispatch] SendKey({ks:?}) 暂由 PassThrough 处理");
                }

                TermAction::SendText(text) => {
                    if let Err(e) = self.tabs[ti].panes[pi].write_user_input(text.as_bytes()) {
                        log::error!("写入 PTY 失败（SendText）: {e:#}");
                    }
                }

                TermAction::Scroll(dir) => {
                    let rows = self.tabs[ti].panes[pi].term.grid().rows() as isize;
                    let delta = match dir {
                        action::ScrollDir::Up => rows - 1,
                        action::ScrollDir::Down => -(rows - 1),
                    };
                    self.tabs[ti].panes[pi]
                        .term
                        .grid_mut()
                        .scroll_display(delta);
                    self.window.request_redraw();
                }

                TermAction::JumpBlock(dir) => {
                    if self.tabs[ti].panes[pi].jump_block(dir) {
                        self.window.request_redraw();
                    }
                }

                TermAction::PasteClipboard => {
                    // M5.3 part4b 镜像态（控制中+远程视图）：粘贴转发给被控端 PTY
                    // （bracketed paste 按被控端模式包裹），不写本地 / 不进编辑器。
                    if self.is_mirror_active() {
                        if let Some(Ok(text)) = self.clipboard.as_mut().map(|c| c.get_text()) {
                            self.remote_ws.send_paste(&text);
                            self.window.request_redraw();
                        }
                        return Vec::new();
                    }
                    // Compose 态粘贴进编辑器而非 PTY（海风哥第十三轮实测 bug：
                    // keymap 注释早已声明此语义但 dispatch 从未分流，Ctrl+V
                    // 一直直写命令行）。dispatch 内实时查模式（与 Submit 的
                    // 二次门控同理，防按键时刻与执行时刻模式漂移）；编辑器
                    // 路径复用 Edit(InsertText)：替换选区/undo/revision/重绘
                    // 全部继承，多行文本直接落多行编辑（设计稿 §4 Ctrl+V 行）。
                    #[cfg(feature = "input-editor")]
                    {
                        let mode =
                            effective_session_mode(&self.tabs[ti].panes[pi], self.force_fallback);
                        if mode == mode::InputMode::Compose {
                            let is_llm = self.tabs[ti].panes[pi].llm_cli.is_some()
                                || llm_cli::detect(None, &self.tabs[ti].panes[pi].term).is_some();
                            if is_llm {
                                let clipboard_image =
                                    llm_attachments::read_clipboard_image(&mut self.clipboard);
                                if let Ok(Some(image)) = clipboard_image {
                                    let workspace = self.tabs[ti].panes[pi]
                                        .term
                                        .cwd()
                                        .map(std::path::Path::to_path_buf)
                                        .or_else(|| std::env::current_dir().ok())
                                        .unwrap_or_else(|| std::path::PathBuf::from("."));
                                    let session_id = self.tabs[ti].panes[pi].id;
                                    match self.tabs[ti].panes[pi].attachments.add_rgba(
                                        &workspace,
                                        session_id,
                                        image.width,
                                        image.height,
                                        &image.rgba,
                                    ) {
                                        Ok(label) => {
                                            return self.dispatch(
                                                action::Action::Edit(
                                                    action::EditAction::InsertText(label),
                                                ),
                                                ti,
                                                pi,
                                            );
                                        }
                                        Err(error) => {
                                            log::error!("保存 LLM CLI 图片附件失败: {error:#}");
                                            self.shell_state.toast.push(
                                                shell::toast::ToastKind::Error,
                                                format!("粘贴图片失败：{error:#}"),
                                            );
                                            return Vec::new();
                                        }
                                    }
                                } else if let Err(error) = clipboard_image {
                                    log::error!("读取 LLM CLI 剪贴板图片失败: {error:#}");
                                    self.shell_state.toast.push(
                                        shell::toast::ToastKind::Error,
                                        format!("读取剪贴板图片失败：{error:#}"),
                                    );
                                    return Vec::new();
                                }
                            }
                            let text = self
                                .clipboard
                                .as_mut()
                                .and_then(|c| c.get_text().ok())
                                .unwrap_or_default();
                            if !text.is_empty() {
                                return self.dispatch(
                                    action::Action::Edit(action::EditAction::InsertText(text)),
                                    ti,
                                    pi,
                                );
                            }
                            if is_llm {
                                self.shell_state.toast.push(
                                    shell::toast::ToastKind::Warn,
                                    "剪贴板中没有可读取的图片或文本",
                                );
                                self.window.request_redraw();
                            }
                            return Vec::new();
                        }
                    }
                    self.tabs[ti].panes[pi].paste_clipboard(&mut self.clipboard);
                }

                TermAction::CopySelection => {
                    // M5.3 part4b 镜像态：复制显示的镜像终端选区到本地剪贴板。
                    if self.is_mirror_active() {
                        if let Some(text) = self.remote_ws.copy_mirror_active() {
                            // 仅写入成功才清选区——失败/不可用时保留，便于重试。
                            match self.clipboard.as_mut().map(|c| c.set_text(text)) {
                                Some(Ok(())) => {
                                    self.remote_ws.clear_mirror_active_selection();
                                    self.window.request_redraw();
                                }
                                Some(Err(e)) => error!("写剪贴板失败: {e}"),
                                None => log::warn!("剪贴板不可用，复制跳过"),
                            }
                        }
                    } else if let Some(text) =
                        self.tabs[ti].panes[pi].copy_selection(&mut self.clipboard)
                    {
                        self.tabs[ti].panes[pi].selection = None;
                        self.show_copied_toast(&text);
                        self.window.request_redraw();
                    }
                }

                TermAction::CopyBlock => {
                    self.tabs[ti].panes[pi].copy_selected_block(&mut self.clipboard);
                    self.tabs[ti].panes[pi].selected_block = None;
                    self.window.request_redraw();
                }

                TermAction::ScrollToBottom => {
                    self.tabs[ti].panes[pi].term.grid_mut().scroll_to_bottom();
                }

                TermAction::Paste(text) => {
                    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
                    let payload = if self.tabs[ti].panes[pi].term.bracketed_paste() {
                        let mut p = Vec::with_capacity(normalized.len() + 12);
                        p.extend_from_slice(b"\x1b[200~");
                        p.extend_from_slice(normalized.as_bytes());
                        p.extend_from_slice(b"\x1b[201~");
                        p
                    } else {
                        normalized.into_bytes()
                    };
                    if let Err(e) = self.tabs[ti].panes[pi].write_user_input(&payload) {
                        log::error!("写入 PTY 失败（Paste）: {e:#}");
                    }
                }

                TermAction::ToggleFallback => {
                    self.force_fallback = !self.force_fallback;
                    // 第十八轮：同步持久化设置并立即写盘，重启后恢复。
                    // 对齐 language_changed 模式：直接调用 settings.save()，
                    // 失败弹 toast 告知用户（写不进盘不影响终端使用）。
                    self.settings.classic_mode = self.force_fallback;
                    if let Some(err) = self.settings.save() {
                        self.shell_state.toast.push(
                            shell::toast::ToastKind::Error,
                            i18n::fmt1(i18n::strings().toast_settings_save_failed_fmt, &err),
                        );
                    }
                    let s = i18n::strings();
                    let msg = if self.force_fallback {
                        s.toast_fallback_enabled
                    } else {
                        s.toast_fallback_disabled
                    };
                    self.shell_state
                        .toast
                        .push(shell::toast::ToastKind::Info, msg);
                    self.window.request_redraw();
                    events.push(StateEvent::FallbackToggled(self.force_fallback));
                }

                // ── CopyEditorSelection（第十一轮，input-editor feature）──
                // 复制 Compose 态编辑器选区文本到剪贴板。
                // 有选区时复制并 toast；无选区时静默无操作。
                #[cfg(feature = "input-editor")]
                TermAction::CopyEditorSelection => {
                    let view = self.tabs[ti].panes[pi].editor.view();
                    if view.has_selection() {
                        let sel = view.selection();
                        let (start, end) = sel.ordered();
                        // 从各行拼接选区文本
                        let mut text = String::new();
                        for row in start.line..=end.line {
                            let line = view.line(row);
                            let from = if row == start.line { start.byte } else { 0 };
                            let to = if row == end.line {
                                end.byte
                            } else {
                                line.len()
                            };
                            text.push_str(&line[from..to]);
                            if row < end.line {
                                text.push('\n');
                            }
                        }
                        if let Some(cb) = self.clipboard.as_mut() {
                            if let Err(e) = cb.set_text(text.clone()) {
                                log::warn!("复制编辑器选区失败: {e}");
                                self.shell_state.toast.push(
                                    shell::toast::ToastKind::Warn,
                                    i18n::strings().toast_copy_failed,
                                );
                            } else {
                                let preview: String = text.chars().take(40).collect();
                                let preview = if text.len() > preview.len() {
                                    format!("{preview}…")
                                } else {
                                    preview
                                };
                                self.shell_state.toast.push(
                                    shell::toast::ToastKind::Info,
                                    i18n::fmt1(i18n::strings().toast_copied_fmt, preview),
                                );
                            }
                        }
                    }
                }

                // ── CutEditorSelection（第十一轮，input-editor feature）───
                // 剪切 Compose 态编辑器选区：复制 + 删除选区。
                #[cfg(feature = "input-editor")]
                TermAction::CutEditorSelection => {
                    // 先复制（复用同一逻辑）
                    let has_sel = self.tabs[ti].panes[pi].editor.view().has_selection();
                    if has_sel {
                        // 触发内部 CopyEditorSelection 逻辑（递归 dispatch 不合适，内联）
                        // 用块作用域限制 view 借用，确保在访问 clipboard 前已释放。
                        let text = {
                            let view = self.tabs[ti].panes[pi].editor.view();
                            let sel = view.selection();
                            let (start, end) = sel.ordered();
                            let mut t = String::new();
                            for row in start.line..=end.line {
                                let line = view.line(row);
                                let from = if row == start.line { start.byte } else { 0 };
                                let to = if row == end.line {
                                    end.byte
                                } else {
                                    line.len()
                                };
                                t.push_str(&line[from..to]);
                                if row < end.line {
                                    t.push('\n');
                                }
                            }
                            t
                        };
                        if let Some(cb) = self.clipboard.as_mut() {
                            if let Err(e) = cb.set_text(text.clone()) {
                                log::warn!("剪切编辑器选区（复制阶段）失败: {e}");
                            }
                        }
                        // 删除选区
                        let outcome = self.tabs[ti].panes[pi]
                            .editor
                            .apply(&lumen_editor::EditAction::DeleteBackward);
                        if outcome.doc_changed {
                            self.window.request_redraw();
                            events.push(StateEvent::EditorRevision(
                                self.tabs[ti].panes[pi].editor.revision(),
                            ));
                        }
                    }
                }

                // ── 无 feature 时的死代码分支（保持编译通过）─────────────
                #[cfg(not(feature = "input-editor"))]
                TermAction::CopyEditorSelection | TermAction::CutEditorSelection => {}
            },
        }

        // 每次 dispatch 后推导当前模式，若变化则发 ModeChanged 事件。
        #[cfg(feature = "input-editor")]
        self.sync_llm_slash_probe(ti, pi);
        // 此推导调用符合设计稿「按键处理后实时计算」的纪律，不缓存。
        let _current_mode = effective_session_mode(&self.tabs[ti].panes[pi], self.force_fallback);
        // 批B：ModeChanged 事件待消费方（状态条）就绪后填充。
        // events.push(StateEvent::ModeChanged(current_mode));

        events
    }

    /// 按会话 id 定位窗格：返回 (tab 下标, 窗格下标)。
    fn find_pane(&self, sid: SessionId) -> Option<(usize, usize)> {
        self.tabs
            .iter()
            .enumerate()
            .find_map(|(ti, t)| t.panes.iter().position(|p| p.id == sid).map(|pi| (ti, pi)))
    }

    /// 鼠标当前位置命中的激活 tab 窗格下标（且未被 egui 弹层盖住）；
    /// 不在任何窗格上返回 None。
    ///
    /// 终端区鼠标交互（选区/块点击/滚轮）以此为闸，不依赖 egui 的
    /// consumed（CentralPanel 覆盖终端区，悬停即视为「在 egui 区域
    /// 上」，consumed 对鼠标无判别力）。右键菜单等弹层可能盖在终端
    /// 上：面板与 CentralPanel 同属 Background 层，弹层在更高层——
    /// 命中非背景层即视为「鼠标在 egui 弹层上」，交互归 egui。
    /// 矩形按会话 id 配对（来自上一帧布局）：tab 结构刚变更时陈旧
    /// 条目在当前激活 tab 里解析不到窗格，自然返回 None。
    fn pane_under_mouse(&self) -> Option<usize> {
        // 窗格关闭按钮的命中区让位（F5 批2）：✕ 上的点击/滚轮/右键
        // 都不算「在窗格上」，点击由 egui 侧的 pane_close 动作处理。
        if self.mouse_on_pane_close() {
            return None;
        }
        // 分隔条命中区让位（F7③）：分隔条上的按下是调比例的开始，
        // 不算「在窗格上」（拖动由 egui 侧处理）。
        if self.mouse_on_pane_divider() {
            return None;
        }
        // 面板拖宽手柄让位（P10）：文件树右缘的手柄盖住终端区左缘
        // 数像素，按下是拖宽的开始，不算「在窗格上」。
        if self.mouse_on_panel_resize() {
            return None;
        }
        let (mx, my) = self.mouse_pos;
        let (sid, _) = self.pane_rects_px.iter().find(|(_, (x, y, w, h))| {
            mx >= *x as f64 && my >= *y as f64 && mx < (*x + *w) as f64 && my < (*y + *h) as f64
        })?;
        let ppp = self.egui_ctx.pixels_per_point();
        let pos = egui::pos2(mx as f32 / ppp, my as f32 / ppp);
        if self
            .egui_ctx
            .layer_id_at(pos)
            .is_some_and(|l| l.order != egui::Order::Background)
        {
            return None;
        }
        self.tabs[self.active_tab]
            .panes
            .iter()
            .position(|p| p.id == *sid)
    }

    fn ssh_modal_open(&self) -> bool {
        self.shell_state.settings.open
            || self.shell_state.login.open
            || self.shell_state.ssh_credentials.is_some()
            || self.shell_state.ssh_session_renaming.is_some()
            || self.shell_state.filetree.dialog_open()
            || self.shell_state.text_editor.is_visible()
            || self.ssh_runtime.active_blocks_input()
    }

    fn routes_input_to_ssh(&self) -> bool {
        ssh_runtime::should_route_terminal_input(
            self.settings.layout.view_mode,
            self.terminal_focused,
            self.ssh_modal_open(),
            self.ssh_runtime.active_accepts_input(),
        )
    }

    fn mouse_in_ssh_terminal(&self) -> bool {
        if !self.settings.layout.view_mode.is_ssh() || self.mouse_on_panel_resize() {
            return false;
        }
        let Some((x, y, width, height)) = self.ssh_rect_px else {
            return false;
        };
        let (mouse_x, mouse_y) = self.mouse_pos;
        if mouse_x < f64::from(x)
            || mouse_y < f64::from(y)
            || mouse_x >= f64::from(x + width)
            || mouse_y >= f64::from(y + height)
        {
            return false;
        }
        let points_per_pixel = self.egui_ctx.pixels_per_point();
        let position = egui::pos2(
            mouse_x as f32 / points_per_pixel,
            mouse_y as f32 / points_per_pixel,
        );
        self.egui_ctx
            .layer_id_at(position)
            .is_none_or(|layer| layer.order == egui::Order::Background)
    }

    /// 鼠标当前位置是否落在某个窗格关闭按钮上（上一帧布局的命中区，
    /// 与 pane_rects_px 同源同陈旧度）。
    fn mouse_on_pane_close(&self) -> bool {
        let (mx, my) = self.mouse_pos;
        self.pane_close_rects_px.iter().any(|(x, y, w, h)| {
            mx >= *x as f64 && my >= *y as f64 && mx < (*x + *w) as f64 && my < (*y + *h) as f64
        })
    }

    /// 鼠标当前位置是否落在某个分隔条命中区上（上一帧布局，F7③）。
    fn mouse_on_pane_divider(&self) -> bool {
        let (mx, my) = self.mouse_pos;
        self.divider_rects_px.iter().any(|(x, y, w, h)| {
            mx >= *x as f64 && my >= *y as f64 && mx < (*x + *w) as f64 && my < (*y + *h) as f64
        })
    }

    /// 鼠标当前位置是否落在侧栏/文件树栏的拖宽手柄上（上一帧布局，
    /// P10）。
    fn mouse_on_panel_resize(&self) -> bool {
        let (mx, my) = self.mouse_pos;
        self.panel_resize_rects_px.iter().any(|(x, y, w, h)| {
            mx >= *x as f64 && my >= *y as f64 && mx < (*x + *w) as f64 && my < (*y + *h) as f64
        })
    }

    /// 鼠标是否命中焦点 LLM 会话的 HUD（展开卡片或收起按钮）。
    ///
    /// HUD 属于终端内工具：点击关闭/展开不应把键盘焦点交给 egui，
    /// 否则后续普通字符落入无文本控件的 UI 层，Windows 会播放默认
    /// 提示音。Area 的 LayerId 与创建时的 Id 同源，可精确区分 HUD
    /// 和设置页、登录框等真正需要接管输入的前景层。
    fn mouse_on_llm_hud(&self) -> bool {
        if self.shell_state.hud.captures_pointer() {
            return true;
        }
        if self.settings.layout.view_mode.is_remote() || self.settings.layout.view_mode.is_ssh() {
            return false;
        }
        let session_id = self.focused_pane().id;
        let ppp = self.egui_ctx.pixels_per_point();
        let pos = egui::pos2(self.mouse_pos.0 as f32 / ppp, self.mouse_pos.1 as f32 / ppp);
        self.egui_ctx.layer_id_at(pos).is_some_and(|layer| {
            layer
                == egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new(("lumen_llm_hud", session_id)),
                )
                || layer
                    == egui::LayerId::new(
                        egui::Order::Foreground,
                        egui::Id::new(("lumen_llm_hud_collapsed", session_id)),
                    )
        })
    }

    /// 焦点窗格 footer 区域的物理像素矩形 (x, y, w, h)。
    ///
    /// 与 `sel_point_at_mouse` 使用相同几何源（同函数计算 footer_px），
    /// 确保命中判定与渲染几何一致、不漂移。
    /// Compose/Fallback（可见）态才返回 Some；AltScreen/Hidden 态返回 None。
    #[cfg(feature = "input-editor")]
    fn focused_footer_rect_px(&self) -> Option<(f32, f32, f32, f32)> {
        let (x, y, w, h) = self.focused_pane_rect_px()?;
        let pane = self.focused_pane();
        let mode = effective_session_mode(pane, self.force_fallback);
        let mut cv = composer::compose_view_for_mode(
            mode,
            pane.editor.view(),
            pane.preedit.clone(),
            pane.exit_badge.clone(),
            None,
        );
        cv.attachment_count = pane.attachments.len();
        if !cv.is_visible() {
            return None;
        }
        let (cell_w, cell_h) = self.renderer.cell_size();
        let fp = self.renderer.padding() * 0.4;
        cv.soft_wrap(lumen_renderer::composer_view::footer_wrap_columns(
            w, cell_w, fp,
        ));
        let max_h = h / 3.0;
        let footer_h =
            lumen_renderer::composer_view::footer_height_px(Some(&cv), cell_h, fp, max_h);
        if footer_h <= 0.0 {
            return None;
        }
        // footer 区域 = 窗格底部 footer_h 像素带
        Some((x, y + h - footer_h, w, footer_h))
    }

    #[cfg(feature = "input-editor")]
    fn attachment_overlay(&mut self) -> Option<AttachmentOverlay> {
        let (fx, fy, fw, footer_h) = self.focused_footer_rect_px()?;
        let session_id = self.focused_pane().id;
        let cli_name = self
            .focused_pane()
            .llm_cli
            .or_else(|| llm_cli::detect(None, &self.focused_pane().term))
            .map(llm_cli::LlmCliKind::display_name)
            .unwrap_or("LLM CLI");

        let live: HashSet<(SessionId, u64)> = self
            .tabs
            .iter()
            .flat_map(|tab| {
                tab.panes.iter().flat_map(|pane| {
                    pane.attachments
                        .images()
                        .iter()
                        .map(move |image| (pane.id, image.id))
                })
            })
            .collect();
        self.attachment_textures.retain(|key, _| live.contains(key));
        if self.focused_pane().attachments.is_empty() {
            return None;
        }

        let source: Vec<(u64, String, u32, u32, Vec<u8>)> = self
            .focused_pane()
            .attachments
            .images()
            .iter()
            .map(|image| {
                (
                    image.id,
                    image.label.clone(),
                    image.thumbnail_width,
                    image.thumbnail_height,
                    image.thumbnail_rgba.clone(),
                )
            })
            .collect();
        for (id, label, width, height, rgba) in source {
            self.attachment_textures
                .entry((session_id, id))
                .or_insert_with(|| {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [width as usize, height as usize],
                        &rgba,
                    );
                    self.egui_ctx.load_texture(
                        format!("llm-attachment-{session_id}-{id}-{label}"),
                        image,
                        egui::TextureOptions::LINEAR,
                    )
                });
        }

        let ppp = self.egui_ctx.pixels_per_point();
        let (_, cell_h) = self.renderer.cell_size();
        let fp = self.renderer.padding() * 0.4;
        let strip_h_px = (cell_h * lumen_renderer::composer_view::ATTACHMENT_STRIP_ROWS)
            .min((footer_h - cell_h - fp * 2.0).max(0.0));
        if strip_h_px <= 0.0 {
            return None;
        }
        let strip_h = strip_h_px / ppp;
        let items = self
            .focused_pane()
            .attachments
            .images()
            .iter()
            .filter_map(|image| {
                let texture = self.attachment_textures.get(&(session_id, image.id))?.id();
                // 明确保留底部的编号/删除按钮行及 item spacing，不能
                // 让图片吃满附件栏后把标签裁掉。
                let max_h = (strip_h - 34.0).max(1.0);
                let scale = (max_h / image.thumbnail_height.max(1) as f32).min(1.0);
                Some(AttachmentOverlayItem {
                    id: image.id,
                    label: image.label.clone(),
                    texture,
                    size: egui::vec2(
                        image.thumbnail_width as f32 * scale,
                        image.thumbnail_height as f32 * scale,
                    ),
                    original_size: (image.width, image.height),
                })
            })
            .collect();
        Some(AttachmentOverlay {
            session_id,
            cli_name,
            rect: egui::Rect::from_min_size(
                egui::pos2(fx / ppp, fy / ppp),
                egui::vec2(fw / ppp, strip_h),
            ),
            items,
        })
    }

    #[cfg(feature = "input-editor")]
    fn remove_llm_attachment(&mut self, session_id: SessionId, attachment_id: u64) {
        let Some((ti, pi)) = self.find_pane(session_id) else {
            return;
        };
        let Some(attachment) = self.tabs[ti].panes[pi].attachments.remove(attachment_id) else {
            return;
        };
        let text = self.tabs[ti].panes[pi]
            .editor
            .view()
            .text()
            .replace(&attachment.label, "");
        self.tabs[ti].panes[pi]
            .editor
            .apply(&lumen_editor::EditAction::SetText(text));
        self.attachment_textures
            .remove(&(session_id, attachment_id));
        if let Err(error) = std::fs::remove_file(&attachment.path) {
            log::warn!(
                "删除图片附件失败（文件可能已被外部移走）{}: {error}",
                attachment.path.display()
            );
        }
        self.sync_llm_slash_probe(ti, pi);
        self.window.request_redraw();
    }

    /// 当前鼠标位置是否落在焦点窗格的 footer 区域内（Compose/可见态）。
    #[cfg(feature = "input-editor")]
    fn mouse_on_footer(&self) -> bool {
        let Some((fx, fy, fw, fh)) = self.focused_footer_rect_px() else {
            return false;
        };
        let (mx, my) = self.mouse_pos;
        mx >= fx as f64 && my >= fy as f64 && mx < (fx + fw) as f64 && my < (fy + fh) as f64
    }

    /// 当前鼠标物理像素位置换算为 footer 内相对坐标（相对 footer 左上角）。
    /// 返回 (rel_x, rel_y, cell_w, cell_h, footer_padding, visual_lines)，
    /// 便于调用 `footer_mouse::pixel_to_position`。
    #[cfg(feature = "input-editor")]
    fn mouse_footer_relative(
        &self,
    ) -> Option<(
        f32,
        f32,
        f32,
        f32,
        f32,
        Vec<lumen_renderer::composer_view::WrappedFooterLine>,
    )> {
        let (fx, fy, fw, footer_h) = self.focused_footer_rect_px()?;
        let (mx, my) = self.mouse_pos;
        let rel_x = mx as f32 - fx;
        let mut rel_y = my as f32 - fy;
        let (cell_w, cell_h) = self.renderer.cell_size();
        let fp = self.renderer.padding() * 0.4;
        let pane = self.focused_pane();
        let attachment_h = if pane.attachments.is_empty() {
            0.0
        } else {
            (cell_h * lumen_renderer::composer_view::ATTACHMENT_STRIP_ROWS)
                .min((footer_h - cell_h - fp * 2.0).max(0.0))
        };
        if rel_y < attachment_h {
            return None;
        }
        rel_y -= attachment_h;
        let lines: Vec<String> = pane.editor.view().lines().map(|l| l.to_owned()).collect();
        let wrap_cols = lumen_renderer::composer_view::footer_wrap_columns(fw, cell_w, fp);
        let visual_lines = lumen_renderer::composer_view::soft_wrap_lines(&lines, wrap_cols);
        Some((rel_x, rel_y, cell_w, cell_h, fp, visual_lines))
    }

    /// 焦点窗格的物理像素矩形 (x, y, w, h)。首帧布局前/结构刚变更
    /// 时可能为 None。
    fn focused_pane_rect_px(&self) -> Option<(f32, f32, f32, f32)> {
        let fid = self.focused_pane().id;
        self.pane_rects_px
            .iter()
            .find(|(id, _)| *id == fid)
            .map(|(_, r)| *r)
    }

    /// 拖选边缘 auto-scroll 方向：鼠标在焦点窗格**内容区**（扣 footer）上边缘以上
    /// 返回 +1（向上滚露出更早内容）、下边缘以下返回 -1（向下滚回到更晚内容）、
    /// 区内返回 0。坐标几何与 `sel_point_at_mouse` 同源（`focused_pane_rect_px` +
    /// `pane_footer_px`，均物理像素）。
    fn autoscroll_dir_for_drag(&self) -> i8 {
        let Some((_, y, w, h)) = self.focused_pane_rect_px() else {
            return 0;
        };
        let footer = self.pane_footer_px(self.tabs[self.active_tab].focused, w, h);
        let top = f64::from(y);
        let bottom = f64::from(y + h - footer);
        let my = self.mouse_pos.1;
        if my < top {
            1
        } else if my > bottom {
            -1
        } else {
            0
        }
    }

    /// 拖选 auto-scroll 单步：按 `autoscroll_drag` 方向滚焦点窗格一行，再把选区端点
    /// 续到滚动后新视口下鼠标（夹在边缘）对应的绝对行列——`sel_point_at_mouse` 用
    /// `view_top_abs_line()+row`，滚动后自动反映新行，故端点随滚动扩展选区。
    fn tick_autoscroll_drag(&mut self) {
        let dir = self.autoscroll_drag;
        if dir == 0 {
            return;
        }
        let before = self.focused_pane().term.grid().view_top_abs_line();
        self.focused_pane_mut()
            .term
            .grid_mut()
            .scroll_display(isize::from(dir));
        // 已到 scrollback 顶 / 底（视口未动）→ 无可滚，停 tick，避免在边缘空转
        // （否则鼠标停边缘不动时按 tick 频率空跑 request_redraw）。
        if self.focused_pane().term.grid().view_top_abs_line() == before {
            self.autoscroll_drag = 0;
            self.autoscroll_at = None;
            return;
        }
        if let Some(head) = self.sel_point_at_mouse() {
            if let Some(sel) = self.focused_pane_mut().selection.as_mut() {
                sel.head = head;
            }
        }
    }

    /// 镜像（控制端远程视图）拖选边缘 auto-scroll 方向：鼠标在**正在拖选的镜像窗格**
    /// 矩形上边缘以上返回 +1（回看向上露更早）、下边缘以下返回 -1、区内返回 0。多窗格
    /// per-pane 矩形（生产路径）；镜像无 footer，整个窗格矩形即内容区。
    fn autoscroll_dir_for_mirror_drag(&self) -> i8 {
        let Some(sid) = self.remote_ws.mirror_pane_selecting_sid() else {
            return 0;
        };
        let Some((_, _x, y, _w, h)) = self
            .mirror_pane_rects_px
            .iter()
            .copied()
            .find(|(s, ..)| *s == sid)
        else {
            return 0;
        };
        let my = self.mouse_pos.1;
        if my < f64::from(y) {
            1
        } else if my > f64::from(y + h) {
            -1
        } else {
            0
        }
    }

    /// 镜像拖选 auto-scroll 单步：按 `autoscroll_drag` 方向回看滚一行（保留选区并按被控端
    /// 绝对行重投影 anchor——见 `scroll_mirror_pane_drag`），再把 head 续到新视口边缘行
    /// （`mirror_pane_sel_update` 按当前显示窗口 0 基）。到回看顶 / 底无可滚则停 tick。
    fn tick_autoscroll_mirror_drag(&mut self) {
        let dir = self.autoscroll_drag;
        if dir == 0 {
            return;
        }
        if !self.remote_ws.scroll_mirror_pane_drag(isize::from(dir)) {
            self.autoscroll_drag = 0;
            self.autoscroll_at = None;
            return;
        }
        if let Some(sid) = self.remote_ws.mirror_pane_selecting_sid() {
            if let Some((row, col)) = self.mirror_pane_cell_clamped(sid) {
                self.remote_ws.mirror_pane_sel_update(row, col);
            }
        }
    }

    /// 把 IME 候选框钉到焦点窗格光标所在格子（Compose 态跟 footer 编辑器
    /// 光标，其余态跟终端光标）。egui 会按自身文本焦点开关整窗 IME / 把
    /// 候选框挪到它的默认控件位，终端聚焦时必须强制复位回光标处。
    ///
    /// 调用点有二：① 每帧 `RedrawRequested` 末（`handle_platform_output`
    /// 之后，纠正 egui 本帧的挪动）；② **焦点失而复得后首个 `Ime::Enabled`
    /// 立即调**——否则「窗口/tab/窗格失焦再回来」时，首字组合串会赶在下一
    /// 帧复位前用 egui 残留的左上角位置画在最左，且该 OS 自绘组合串成孤儿
    /// 删不掉（Win10 焦点回来首字缩最左/删不掉的真因；WT/Warp 无此问题）。
    fn update_ime_cursor_area(&self, log_it: bool) {
        if !self.terminal_focused {
            return;
        }
        self.window.set_ime_allowed(true);
        if self.settings.layout.view_mode.is_ssh() {
            let Some((px, py, _, _)) = self.ssh_rect_px else {
                return;
            };
            let Some((row, column, _)) = self.ssh_runtime.active_cursor() else {
                return;
            };
            let (cell_width, cell_height) = self.renderer.cell_size();
            let (cursor_x, cursor_y) = self.renderer.cell_origin(row, column);
            self.window.set_ime_cursor_area(
                winit::dpi::PhysicalPosition::new(
                    f64::from(px + cursor_x),
                    f64::from(py + cursor_y),
                ),
                winit::dpi::PhysicalSize::new(f64::from(cell_width), f64::from(cell_height)),
            );
            return;
        }
        // M5.3 part4c：镜像态把候选框定位到**镜像光标**（被控端光标）在终端区的像素位置，
        // 使控制端打中文时候选框出现在远端光标处（跟随态有效；回看态 mirror_cursor=None
        // 则跳过本次定位，候选框留在上次位置）。
        if self.is_mirror_active() {
            if let (Some((rx, ry, _, _)), Some((crow, ccol))) =
                (self.mirror_rect_px, self.remote_ws.mirror_cursor())
            {
                let (cw, ch) = self.renderer.cell_size();
                let (cx, cy) = self.renderer.cell_origin(crow, ccol);
                self.window.set_ime_cursor_area(
                    winit::dpi::PhysicalPosition::new(
                        f64::from(rx) + f64::from(cx),
                        f64::from(ry) + f64::from(cy),
                    ),
                    winit::dpi::PhysicalSize::new(f64::from(cw), f64::from(ch)),
                );
            }
            return;
        }
        let Some((px, py, pw, ph)) = self.focused_pane_rect_px() else {
            if log_it {
                log::info!("[IME-ENABLED] 无焦点窗格矩形（首帧布局未完成），跳过本次定位");
            }
            return;
        };
        let s = self.focused_pane();
        let (cw, ch) = self.renderer.cell_size();
        #[cfg(feature = "input-editor")]
        let (ime_x, ime_y) = {
            let mode = effective_session_mode(s, self.force_fallback);
            if mode == crate::mode::InputMode::Compose {
                let cv_cursor = s.editor.view().cursor();
                let raw_attachment_h = if s.attachments.is_empty() {
                    0.0
                } else {
                    ch * lumen_renderer::composer_view::ATTACHMENT_STRIP_ROWS
                };
                let footer_h = (ch * s.editor.view().line_count().max(1) as f32
                    + self.renderer.padding() * 0.8
                    + raw_attachment_h)
                    .min(ph / 3.0);
                let fp = self.renderer.padding() * 0.4;
                let attachment_h = raw_attachment_h.min((footer_h - ch - fp * 2.0).max(0.0));
                let footer_top_y = py + ph - footer_h + attachment_h;
                let col_approx = cv_cursor.byte.min(200) as f32;
                let footer_x = px + col_approx * cw;
                let footer_y = footer_top_y + cv_cursor.line as f32 * ch;
                (footer_x, footer_y)
            } else {
                let g = s.term.grid();
                let view_row =
                    (g.display_offset() + s.cursor_displayed.0).min(g.rows().saturating_sub(1));
                let (cx, cy) = self.renderer.cell_origin(view_row, s.cursor_displayed.1);
                (px + cx, py + cy)
            }
        };
        #[cfg(not(feature = "input-editor"))]
        let (ime_x, ime_y) = {
            let g = s.term.grid();
            let view_row =
                (g.display_offset() + s.cursor_displayed.0).min(g.rows().saturating_sub(1));
            let (cx, cy) = self.renderer.cell_origin(view_row, s.cursor_displayed.1);
            (px + cx, py + cy)
        };
        let _ = pw; // pw 仅防未使用 warning（IME 候选框宽度用 cw）
        self.window.set_ime_cursor_area(
            winit::dpi::PhysicalPosition::new(ime_x as f64, ime_y as f64),
            winit::dpi::PhysicalSize::new(cw as f64, ch as f64),
        );
        if log_it {
            log::info!(
                "[IME-ENABLED] 候选框→({ime_x:.0},{ime_y:.0}) cursor_displayed=(行{},列{}) 窗格原点=({px:.0},{py:.0}) cell={cw:.0}x{ch:.0}",
                s.cursor_displayed.0,
                s.cursor_displayed.1
            );
        }
    }

    /// 「回到底部」浮动按钮的目标（每个上滚超过一整屏的可见窗格各
    /// 一个）。按钮置于窗格底部居中；焦点窗格扣除 footer 高度，避免压
    /// 在输入区上。目标同时记录视口所有者：主屏历史由 Lumen 操作，
    /// DECSET 1007 备用屏则须向应用发送 End。
    ///
    /// 几何取自上一帧的 `pane_rects_px`（物理像素，连续帧间稳定，滞后
    /// 一帧无感）；逻辑点 = 物理像素 / `pixels_per_point`。主屏采用真实
    /// `display_offset`；备用屏无本地 scrollback，采用成功转发给应用的
    /// Alternate Scroll 距离估算。二者都超过一整屏（`> rows`）才纳入。
    /// `&self` 借用整个 state，故须在 `run_ui` 可变借用
    /// `state.shell_state` 之前调用。
    fn scroll_to_bottom_overlays(&self) -> Vec<ScrollToBottomTarget> {
        // 按钮逻辑半径与距内容区下沿留白（点）。
        const RADIUS: f32 = 16.0;
        const GAP: f32 = 14.0;
        let ppp = self.egui_ctx.pixels_per_point();
        if ppp <= 0.0 {
            return Vec::new();
        }
        let focus_sid = self.focused_pane().id;
        // 焦点窗格 footer 高度（物理像素）；非 input-editor 构建无 footer。
        #[cfg(feature = "input-editor")]
        let footer_h_px = self.focused_footer_rect_px().map_or(0.0, |(_, _, _, h)| h);
        #[cfg(not(feature = "input-editor"))]
        let footer_h_px = 0.0_f32;

        let panes = &self.tabs[self.active_tab].panes;
        let mut out = Vec::new();
        for (sid, (x, y, w, h)) in &self.pane_rects_px {
            let Some(pane) = panes.iter().find(|p| p.id == *sid) else {
                continue;
            };
            let g = pane.term.grid();
            let Some(action) = scroll_to_bottom_action(
                g.display_offset(),
                g.rows(),
                pane.uses_application_alternate_scroll(),
                pane.alternate_scroll_distance_hint(),
            ) else {
                continue;
            };
            let footer = if *sid == focus_sid { footer_h_px } else { 0.0 };
            let center_x = (x + w / 2.0) / ppp;
            let bottom = (y + h - footer) / ppp;
            let center = egui::pos2(center_x, bottom - GAP - RADIUS);
            out.push(ScrollToBottomTarget {
                sid: *sid,
                rect: egui::Rect::from_center_size(center, egui::vec2(RADIUS * 2.0, RADIUS * 2.0)),
                action,
            });
        }
        out
    }

    /// 终端区滚动条的逐窗格几何（每个有 scrollback 历史、内容区够高的
    /// 可见窗格各一条）。返回轨道/滑块逻辑矩形与 scrollback 行数，run_ui
    /// 闭包内据此绘制 + 处理拖动，闭包后把目标 `display_offset` 落到 grid。
    ///
    /// 滑块模型：内容总行数 `total = scrollback + rows`，滑块高 = 可视占比
    /// `rows/total`（带最小高，行极多时仍可抓），位置按滚动进度——
    /// `display_offset` 以行计，0 = 底部（滑块贴底）、`scrollback` = 最旧
    /// （滑块贴顶）。几何取自上一帧 `pane_rects_px`（物理像素→逻辑点，
    /// 滞后一帧无感）；焦点窗格扣 footer，轨道不压输入区。`&self` 借整个
    /// state，须在 `run_ui` 可变借用 `state.shell_state` 之前调用。
    fn scrollbar_overlays(&self) -> Vec<ScrollbarGeom> {
        // 轨道逻辑宽、距窗格右缘留白、滑块最小高（逻辑点）。
        const WIDTH: f32 = 10.0;
        const MARGIN: f32 = 2.0;
        const MIN_THUMB: f32 = 28.0;
        let ppp = self.egui_ctx.pixels_per_point();
        if ppp <= 0.0 {
            return Vec::new();
        }
        let focus_sid = self.focused_pane().id;
        #[cfg(feature = "input-editor")]
        let footer_h_px = self.focused_footer_rect_px().map_or(0.0, |(_, _, _, h)| h);
        #[cfg(not(feature = "input-editor"))]
        let footer_h_px = 0.0_f32;

        let panes = &self.tabs[self.active_tab].panes;
        let mut out = Vec::new();
        for (sid, (x, y, w, h)) in &self.pane_rects_px {
            let Some(pane) = panes.iter().find(|p| p.id == *sid) else {
                continue;
            };
            let g = pane.term.grid();
            let sb = g.scrollback_len();
            // 无历史可滚则不画。
            if sb == 0 {
                continue;
            }
            let rows = g.rows().max(1);
            let total = sb + rows;
            let footer = if *sid == focus_sid { footer_h_px } else { 0.0 };
            // 轨道（逻辑点）：贴窗格右缘内侧，纵向占内容区（扣 footer）。
            let tx = (x + w) / ppp - MARGIN - WIDTH;
            let ty = y / ppp;
            let th = ((h - footer) / ppp).max(0.0);
            // 内容区太矮、塞不下最小滑块就不画（极小窗格防御）。
            if th <= MIN_THUMB {
                continue;
            }
            let track = egui::Rect::from_min_size(egui::pos2(tx, ty), egui::vec2(WIDTH, th));
            // 滑块高 = 可视占比 ×轨道高，夹到 [MIN_THUMB, th]。
            let thumb_h = (th * rows as f32 / total as f32).clamp(MIN_THUMB, th);
            // 进度：offset=0→贴底、offset=sb→贴顶。可移动范围 = th - thumb_h。
            let movable = (th - thumb_h).max(0.0);
            let offset = g.display_offset().min(sb);
            let thumb_top = ty + (1.0 - offset as f32 / sb as f32) * movable;
            let thumb =
                egui::Rect::from_min_size(egui::pos2(tx, thumb_top), egui::vec2(WIDTH, thumb_h));
            out.push(ScrollbarGeom {
                sid: *sid,
                track,
                thumb,
                scrollback: sb,
            });
        }
        out
    }

    /// 鼠标当前是否落在某窗格的滚动条轨道上（复用 `scrollbar_overlays`
    /// 几何，逻辑点判定）。滚动条是 `Order::Foreground` 层，会让
    /// `pane_under_mouse` 因命中非 Background 层而返回 None——MouseWheel
    /// 据此补一判：否则每个有历史窗格的右缘整列会变成「滚轮死区」（指针
    /// 悬在轨道上滚不动终端）。
    fn mouse_on_scrollbar_track(&self) -> bool {
        let ppp = self.egui_ctx.pixels_per_point();
        if ppp <= 0.0 {
            return false;
        }
        let pos = egui::pos2(self.mouse_pos.0 as f32 / ppp, self.mouse_pos.1 as f32 / ppp);
        self.scrollbar_overlays()
            .iter()
            .any(|g| g.track.contains(pos))
    }

    /// 把当前鼠标像素位置换算成**焦点窗格**的选区端点（绝对行号）。
    /// cell_at 接相对窗格原点的坐标并按窗格尺寸夹紧；焦点窗格矩形
    /// 未知（首帧布局前）时返回 None。
    ///
    /// M4.1 批C：footer 区域（底部 footer_px 像素）的点击夹紧到末行，
    /// 不映射进 footer（footer 有自己的点击处理，批D 实现）。
    fn sel_point_at_mouse(&self) -> Option<SelPoint> {
        let (x, y, w, h) = self.focused_pane_rect_px()?;
        // M4.1 批C：计算 footer 高度以排除 footer 区域的点击。
        #[cfg(feature = "input-editor")]
        let footer_px = {
            let pane = self.focused_pane();
            let mode = effective_session_mode(pane, self.force_fallback);
            let mut cv = composer::compose_view_for_mode(
                mode,
                pane.editor.view(),
                pane.preedit.clone(),
                pane.exit_badge.clone(),
                None, // ghost 仅用于渲染，高度计算不需要
            );
            cv.attachment_count = pane.attachments.len();
            let (cell_w, cell_h) = self.renderer.cell_size();
            let fp = self.renderer.padding() * 0.4;
            cv.soft_wrap(lumen_renderer::composer_view::footer_wrap_columns(
                w, cell_w, fp,
            ));
            let max_h = h / 3.0;
            lumen_renderer::composer_view::footer_height_px(Some(&cv), cell_h, fp, max_h)
        };
        #[cfg(not(feature = "input-editor"))]
        let footer_px: f32 = 0.0;
        let (row, col) = self.renderer.cell_at_with_footer(
            self.mouse_pos.0 - x as f64,
            self.mouse_pos.1 - y as f64,
            w.max(1.0) as u32,
            h.max(1.0) as u32,
            footer_px,
        );
        Some(SelPoint {
            line: self.focused_pane().term.grid().view_top_abs_line() + row as u64,
            col,
        })
    }

    /// M5.3 part4b：鼠标当前位置在镜像区内则返回相对镜像区原点的 `(row, col)`（控制端
    /// 镜像选区用）。非控制中 / 不在镜像区返回 None。镜像离屏按终端区像素定尺寸（含
    /// padding），故与窗格同款 `cell_at_with_footer`（footer=0）换算。
    fn mirror_cell_at_mouse(&self) -> Option<(usize, usize)> {
        let (rx, ry, rw, rh) = self.mirror_rect_px?;
        let (lx, ly) = (
            self.mouse_pos.0 - f64::from(rx),
            self.mouse_pos.1 - f64::from(ry),
        );
        if lx < 0.0 || ly < 0.0 || lx >= f64::from(rw) || ly >= f64::from(rh) {
            return None;
        }
        // 宽高与镜像离屏纹理同式 round（非截断），DPI 分数缩放下末行/末列不错格。
        Some(self.renderer.cell_at_with_footer(
            lx,
            ly,
            (rw.round() as u32).max(1),
            (rh.round() as u32).max(1),
            0.0,
        ))
    }

    /// M5.3 part4b：拖选用的镜像单元格换算——**不做边界拒绝**，越出镜像区由
    /// `cell_at_with_footer` 内部夹紧到边缘行列（拖出镜像区选区端点收在边缘，而非冻结
    /// 在最后一个区内格）。非控制中返回 None。
    fn mirror_cell_clamped(&self) -> Option<(usize, usize)> {
        let (rx, ry, rw, rh) = self.mirror_rect_px?;
        let (lx, ly) = (
            self.mouse_pos.0 - f64::from(rx),
            self.mouse_pos.1 - f64::from(ry),
        );
        Some(self.renderer.cell_at_with_footer(
            lx,
            ly,
            (rw.round() as u32).max(1),
            (rh.round() as u32).max(1),
            0.0,
        ))
    }

    /// Phase 4 多窗格镜像：鼠标所在镜像窗格的 `(session_id, x, y, w, h)`（内容矩形物理像素）；
    /// 不在任何窗格内返回 None。命中区用每帧填的 `mirror_pane_rects_px`。
    fn mirror_pane_at_mouse(&self) -> Option<(session::SessionId, f32, f32, f32, f32)> {
        let (mx, my) = self.mouse_pos;
        self.mirror_pane_rects_px
            .iter()
            .copied()
            .find(|(_, x, y, w, h)| {
                mx >= f64::from(*x)
                    && my >= f64::from(*y)
                    && mx < f64::from(x + w)
                    && my < f64::from(y + h)
            })
    }

    /// Phase 4 多窗格镜像：鼠标在某镜像窗格内则返回 `(session_id, row, col)`（相对该窗格内容矩形、
    /// 按该窗格离屏尺寸换算，用于点选焦点 + per-pane 拖选）；不在任何窗格返回 None。
    fn mirror_pane_cell_at_mouse(&self) -> Option<(session::SessionId, usize, usize)> {
        let (sid, x, y, w, h) = self.mirror_pane_at_mouse()?;
        let (lx, ly) = (
            self.mouse_pos.0 - f64::from(x),
            self.mouse_pos.1 - f64::from(y),
        );
        let (row, col) = self.renderer.cell_at_with_footer(
            lx,
            ly,
            (w.round() as u32).max(1),
            (h.round() as u32).max(1),
            0.0,
        );
        Some((sid, row, col))
    }

    /// Phase 4 多窗格镜像：把鼠标位置 clamp 到**指定窗格** `sid` 的内容矩形换算 `(row, col)`（拖选用，
    /// 拖出该窗格由 `cell_at_with_footer` 内部夹紧到边缘行列、不跳别格）。窗格矩形未知返回 None。
    fn mirror_pane_cell_clamped(&self, sid: session::SessionId) -> Option<(usize, usize)> {
        let (_, x, y, w, h) = self
            .mirror_pane_rects_px
            .iter()
            .copied()
            .find(|(s, ..)| *s == sid)?;
        let (lx, ly) = (
            self.mouse_pos.0 - f64::from(x),
            self.mouse_pos.1 - f64::from(y),
        );
        Some(self.renderer.cell_at_with_footer(
            lx,
            ly,
            (w.round() as u32).max(1),
            (h.round() as u32).max(1),
            0.0,
        ))
    }

    /// 某窗格底部 footer 物理像素高度：仅聚焦窗格显示 footer，其余为 0。
    /// 与渲染/resize 用同一 `footer_height_px` 算法，保证命中坐标一致。
    #[cfg(feature = "input-editor")]
    fn pane_footer_px(&self, pane_idx: usize, pane_w: f32, pane_h: f32) -> f32 {
        if self.tabs[self.active_tab].focused != pane_idx {
            return 0.0;
        }
        let pane = &self.tabs[self.active_tab].panes[pane_idx];
        let mode = effective_session_mode(pane, self.force_fallback);
        let mut cv = composer::compose_view_for_mode(
            mode,
            pane.editor.view(),
            pane.preedit.clone(),
            pane.exit_badge.clone(),
            None,
        );
        cv.attachment_count = pane.attachments.len();
        let (cell_w, cell_h) = self.renderer.cell_size();
        let fp = self.renderer.padding() * 0.4;
        cv.soft_wrap(lumen_renderer::composer_view::footer_wrap_columns(
            pane_w, cell_w, fp,
        ));
        let max_h = pane_h / 3.0;
        lumen_renderer::composer_view::footer_height_px(Some(&cv), cell_h, fp, max_h)
    }
    #[cfg(not(feature = "input-editor"))]
    fn pane_footer_px(&self, _pane_idx: usize, _pane_w: f32, _pane_h: f32) -> f32 {
        0.0
    }

    /// 鼠标当前所在的「窗格 + 单元格」：返回 (窗格下标, 窗格 id, 绝对行,
    /// 列)。鼠标不在任何窗格上（egui 面板/分隔条/首帧布局前）返回 None。
    /// 坐标换算与 [`Self::sel_point_at_mouse`] 同源（扣 footer、按窗格矩形
    /// 夹紧），但作用于**鼠标下的窗格**而非焦点窗格（F10 可悬停非焦点窗格）。
    fn cell_under_mouse(&self) -> Option<(usize, SessionId, u64, usize)> {
        let pane_idx = self.pane_under_mouse()?;
        let pane_id = self.tabs[self.active_tab].panes[pane_idx].id;
        let (x, y, w, h) = self
            .pane_rects_px
            .iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, r)| *r)?;
        let footer_px = self.pane_footer_px(pane_idx, w, h);
        let (row, col) = self.renderer.cell_at_with_footer(
            self.mouse_pos.0 - x as f64,
            self.mouse_pos.1 - y as f64,
            w.max(1.0) as u32,
            h.max(1.0) as u32,
            footer_px,
        );
        let abs = self.tabs[self.active_tab].panes[pane_idx]
            .term
            .grid()
            .view_top_abs_line()
            + row as u64;
        Some((pane_idx, pane_id, abs, col))
    }

    /// 鼠标当前所在窗格的**视口内** 0 基格子坐标：返回 (窗格下标, 窗格 id,
    /// 列, 行)。与 [`Self::cell_under_mouse`] 同源（按窗格矩形 + footer 夹紧），
    /// 但返回视口内行号而非绝对行——鼠标上报要的是相对屏幕左上角的列/行。
    fn viewport_cell_under_mouse(&self) -> Option<(usize, SessionId, usize, usize)> {
        let pane_idx = self.pane_under_mouse()?;
        let pane_id = self.tabs[self.active_tab].panes[pane_idx].id;
        let (x, y, w, h) = self
            .pane_rects_px
            .iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, r)| *r)?;
        let footer_px = self.pane_footer_px(pane_idx, w, h);
        let (row, col) = self.renderer.cell_at_with_footer(
            self.mouse_pos.0 - x as f64,
            self.mouse_pos.1 - y as f64,
            w.max(1.0) as u32,
            h.max(1.0) as u32,
            footer_px,
        );
        Some((pane_idx, pane_id, col, row))
    }

    /// 当前修饰键 → 鼠标协议修饰位。Shift 是本地选区逃生通道（上报前已被
    /// 拦截），这里仍如实带上，仅 ctrl/alt 实际进入编码。
    fn mouse_mods(&self) -> MouseMods {
        MouseMods {
            shift: self.modifiers.shift_key(),
            alt: self.modifiers.alt_key(),
            ctrl: self.modifiers.control_key(),
        }
    }

    /// 把 winit 按键映射为上报按钮（仅左/中/右参与上报）。
    fn map_report_button(button: MouseButton) -> Option<MouseReportBtn> {
        match button {
            MouseButton::Left => Some(MouseReportBtn::Left),
            MouseButton::Middle => Some(MouseReportBtn::Middle),
            MouseButton::Right => Some(MouseReportBtn::Right),
            _ => None,
        }
    }

    /// 终端选区复制成功后弹 toast「已复制：<预览>」，给用户明确反馈（老实现终端选区
    /// 复制成功/失败都无任何提示，是「不知道到底复制没复制成」的体验盲区）。
    fn show_copied_toast(&mut self, text: &str) {
        let mut preview: String = text.chars().take(40).collect();
        if text.chars().count() > 40 {
            preview.push('…');
        }
        // 多行选区把换行显示为空格，避免 toast 里断行。
        let preview = preview.replace('\n', " ");
        self.shell_state.toast.push(
            shell::toast::ToastKind::Info,
            i18n::fmt1(i18n::strings().toast_copied_fmt, preview),
        );
    }

    /// 鼠标按键上报：把按下/释放编码成鼠标事件写入对应窗格 PTY 并请求重绘，
    /// 返回 `true`（事件已被上报消费，调用方应跳过本地选区/块选中/复制粘贴）；
    /// 否则返回 `false`，调用方按原逻辑处理。
    ///
    /// 按下与释放严格配对：本次「点击」归本地还是归上报，在**按下**一刻由
    /// Shift（本地选区 / 复制逃生通道）latch 决定——`mouse_report_held` 记下
    /// 被上报的按键即作标记。释放时**不再看当前 Shift / 鼠标是否还在窗格内**，
    /// 当且仅当这枚键的按下当初被上报才上报释放，并无条件清掉 held。否则
    /// 中途切 Shift / 拖出窗格松手会留下「幻影按住」（程序以为键没抬）+ held
    /// 卡死（之后纯悬停被误报成拖动）。
    fn report_mouse_button(&mut self, button: MouseButton, pressed: bool) -> bool {
        let Some(btn) = Self::map_report_button(button) else {
            return false;
        };
        let idx = Self::held_index(btn);
        if !pressed {
            // 这枚键的按下当初走了本地（Shift 逃生 / 非上报窗格）：释放也
            // 走本地，不碰上报。
            if !self.mouse_report_held[idx] {
                return false;
            }
            // 解除该键的按住态（无条件，先于一切；仅清这一枚，不动其他键）。
            self.mouse_report_held[idx] = false;
            // 拖动全程钉在发起（焦点）窗格：释放也写焦点窗格、坐标按其矩形
            // 夹紧（鼠标可能已拖到别的窗格 / 窗外），保证 Press 的那个窗格
            // 一定收到配对的 Release、按钮不卡死。
            let (pane_idx, col, row) = self.drag_report_target();
            let pane = &self.tabs[self.active_tab].panes[pane_idx];
            let proto = pane.term.mouse_protocol();
            if !proto.is_on() {
                return true; // 上报刚被程序关掉：消费掉释放即可，无需写
            }
            let enc = pane.term.mouse_encoding();
            let ev = MouseEvent {
                kind: MouseEventKind::Release(btn),
                col,
                row,
                mods: self.mouse_mods(),
            };
            if let Some(bytes) = encode_mouse(proto, enc, ev) {
                if let Err(e) = self.tabs[self.active_tab].panes[pane_idx].write_user_input(&bytes)
                {
                    log::error!("鼠标上报写 PTY 失败: {e:#}");
                }
                self.window.request_redraw();
            }
            return true;
        }
        // —— 按下：Shift = 本地选区逃生通道（决策只在按下一刻 latch）。
        if self.modifiers.shift_key() {
            return false;
        }
        // footer（编辑器输入区）不上报：它不是终端内容。
        #[cfg(feature = "input-editor")]
        if self.mouse_on_footer() {
            return false;
        }
        let Some((pane_idx, _id, col, row)) = self.viewport_cell_under_mouse() else {
            return false;
        };
        let proto = self.tabs[self.active_tab].panes[pane_idx]
            .term
            .mouse_protocol();
        if !proto.is_on() {
            return false;
        }
        // 上报开启（Claude/vim/less 等全屏程序）：Ctrl+左键落在链接上 = 本地开链接
        // 逃生通道（对齐普通 shell 的 Ctrl+单击开链接）。命中链接的 Ctrl+左键让位
        // 本地、不上报（held 不置位，释放因 held==false 自然配对走本地），使释放时
        // 走到 F10「Ctrl+Click 开链接」；否则这枚点击被上报吞掉，全屏程序里链接永
        // 远点不开。非链接区的 Ctrl+左键仍照常上报，不破坏程序自身 Ctrl+Click 语义。
        if button == MouseButton::Left
            && self.modifiers.control_key()
            && self.link_at_mouse().is_some()
        {
            return false;
        }
        // 上报开且焦点窗格已有本地选区高亮时，普通左键（非 Shift——已在上面逃生）按下
        // → 清选区并消费该次按下（点击取消选择）。高亮靠 Shift 逃生选出、copy-on-select
        // 后刻意保留（供再复制），但上报态普通单击会被程序吞掉、清不掉高亮——这里拦下，
        // 对齐终端「有选区时点击先取消选择」惯例（海风哥反馈：Shift 选完不复制则高亮永久
        // 赖着，普通左键应能取消）。held 不置位、不上报给程序。
        if button == MouseButton::Left
            && self.focused_pane().selection.is_some_and(|s| !s.is_empty())
        {
            self.focused_pane_mut().selection = None;
            self.window.request_redraw();
            return true;
        }
        let enc = self.tabs[self.active_tab].panes[pane_idx]
            .term
            .mouse_encoding();
        let ev = MouseEvent {
            kind: MouseEventKind::Press(btn),
            col,
            row,
            mods: self.mouse_mods(),
        };
        let Some(bytes) = encode_mouse(proto, enc, ev) else {
            return false;
        };
        // latch：记下被上报的按键（集合，多键并发各自配对），供 motion 上报
        // 填 held、供释放配对。
        self.mouse_report_held[idx] = true;
        if let Err(e) = self.tabs[self.active_tab].panes[pane_idx].write_user_input(&bytes) {
            log::error!("鼠标上报写 PTY 失败: {e:#}");
        }
        self.window.request_redraw();
        true
    }

    /// 拖动 / 释放上报的目标：始终是按下时的焦点（发起）窗格——按住期间把
    /// 上报钉在起始窗格，模拟真实终端的指针捕获；坐标按该窗格矩形夹紧（鼠标
    /// 可能已拖到别的窗格 / 窗外）。返回 (窗格下标, 列, 行)。
    fn drag_report_target(&self) -> (usize, usize, usize) {
        let pane_idx = self.tabs[self.active_tab].focused;
        let (col, row) = self
            .viewport_cell_in_pane(pane_idx)
            .map(|(_, c, r)| (c, r))
            .unwrap_or((0, 0));
        (pane_idx, col, row)
    }

    /// 指定窗格的视口格（会话 id, 列, 行），坐标按该窗格矩形夹紧——鼠标在
    /// 窗格外时落到边缘格。窗格不存在 / 尚无矩形时返回 None。
    fn viewport_cell_in_pane(&self, pane_idx: usize) -> Option<(SessionId, usize, usize)> {
        let pane_id = self.tabs[self.active_tab].panes.get(pane_idx)?.id;
        let (x, y, w, h) = self
            .pane_rects_px
            .iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, r)| *r)?;
        let footer_px = self.pane_footer_px(pane_idx, w, h);
        let (row, col) = self.renderer.cell_at_with_footer(
            self.mouse_pos.0 - x as f64,
            self.mouse_pos.1 - y as f64,
            w.max(1.0) as u32,
            h.max(1.0) as u32,
            footer_px,
        );
        Some((pane_id, col, row))
    }

    /// 鼠标上报按住集合的索引（0=Left / 1=Middle / 2=Right）。
    fn held_index(btn: MouseReportBtn) -> usize {
        match btn {
            MouseReportBtn::Left => 0,
            MouseReportBtn::Middle => 1,
            MouseReportBtn::Right => 2,
        }
    }

    /// 是否有任意被上报的按键处于按住（拖动进行中）。
    fn any_button_held(&self) -> bool {
        self.mouse_report_held.iter().any(|&h| h)
    }

    /// 拖动 motion 上报填入的代表按键：按住集合里按 左 > 中 > 右 取一个。
    fn held_repr_button(&self) -> Option<MouseReportBtn> {
        if self.mouse_report_held[0] {
            Some(MouseReportBtn::Left)
        } else if self.mouse_report_held[1] {
            Some(MouseReportBtn::Middle)
        } else if self.mouse_report_held[2] {
            Some(MouseReportBtn::Right)
        } else {
            None
        }
    }

    /// 按住集合索引 → 按键（0=Left / 1=Middle / 2=Right）。
    fn button_from_index(i: usize) -> Option<MouseReportBtn> {
        match i {
            0 => Some(MouseReportBtn::Left),
            1 => Some(MouseReportBtn::Middle),
            2 => Some(MouseReportBtn::Right),
            _ => None,
        }
    }

    /// 鼠标上报的拖动被打断（离窗 / 失焦 / 切焦点窗格 / 切 Tab / 新建·关闭
    /// 窗格 / 最大化）时：对每个仍被上报按住的键，向**当前焦点窗格**补发一条
    /// Release，让程序收到配对的 button-up、不残留「幻影按住」，随后清空按住
    /// 集合。拖动期间焦点不变，故此刻焦点窗格即拖动发起窗格——**务必在改
    /// focused / active_tab / 移除发起窗格之前调用**，否则补发会落到错误窗格
    /// 或漏发。坐标按发起窗格矩形夹紧（对 Release 而言坐标无关紧要，程序只在
    /// 意「键已抬」）。
    ///
    /// 全程防御：取不到活的焦点窗格（close_tab 关末位激活 tab 时 active_tab
    /// 暂越界、或发起窗格已被移除）就只清按住态、不补发——绝不硬索引 panic。
    fn release_held_report_buttons(&mut self) {
        if !self.any_button_held() {
            return;
        }
        // 镜像态（控制端）拖动中断（失焦 / 离窗等）：把仍按住的键的 Release 转发给
        // 被控端、清状态，避免被控端程序残留「幻影按住」。`mirror_report_sid` 有值
        // 即当前按住是镜像转发的（与本地互斥）。
        if let Some(sid) = self.mirror_report_sid {
            if let (Some((row, col)), Some((proto, enc))) = (
                self.mirror_pane_cell_clamped(sid),
                self.mirror_pane_proto_enc(sid),
            ) {
                if proto.is_on() {
                    let mods = self.mouse_mods();
                    let held = self.mouse_report_held;
                    let mut buf = Vec::new();
                    for (i, &down) in held.iter().enumerate() {
                        if down {
                            if let Some(btn) = Self::button_from_index(i) {
                                if let Some(b) = encode_mouse(
                                    proto,
                                    enc,
                                    MouseEvent {
                                        kind: MouseEventKind::Release(btn),
                                        col,
                                        row,
                                        mods,
                                    },
                                ) {
                                    buf.extend_from_slice(&b);
                                }
                            }
                        }
                    }
                    if !buf.is_empty() {
                        self.remote_ws.send_input_to(sid, &buf);
                        self.window.request_redraw();
                    }
                }
            }
            self.mouse_report_held = [false; 3];
            self.mirror_report_sid = None;
            self.mouse_report_last_cell = None;
            return;
        }
        let pane_idx = match self.tabs.get(self.active_tab) {
            Some(tab) if tab.focused < tab.panes.len() => tab.focused,
            _ => {
                self.mouse_report_held = [false; 3];
                return;
            }
        };
        let (col, row) = self
            .viewport_cell_in_pane(pane_idx)
            .map(|(_, c, r)| (c, r))
            .unwrap_or((0, 0));
        let proto = self.tabs[self.active_tab].panes[pane_idx]
            .term
            .mouse_protocol();
        if proto.is_on() {
            let enc = self.tabs[self.active_tab].panes[pane_idx]
                .term
                .mouse_encoding();
            let mods = self.mouse_mods();
            let held = self.mouse_report_held; // [bool;3] 是 Copy
            let mut buf = Vec::new();
            for (i, &down) in held.iter().enumerate() {
                if down {
                    if let Some(btn) = Self::button_from_index(i) {
                        if let Some(b) = encode_mouse(
                            proto,
                            enc,
                            MouseEvent {
                                kind: MouseEventKind::Release(btn),
                                col,
                                row,
                                mods,
                            },
                        ) {
                            buf.extend_from_slice(&b);
                        }
                    }
                }
            }
            if !buf.is_empty() {
                if let Err(e) = self.tabs[self.active_tab].panes[pane_idx].write_user_input(&buf) {
                    log::error!("补发鼠标释放写 PTY 失败: {e:#}");
                }
                self.window.request_redraw();
            }
        }
        self.mouse_report_held = [false; 3];
    }

    /// 滚轮上报：上报开启时把滚轮编码成鼠标按钮 64(上)/65(下)，按档数发
    /// 给程序（每档一个事件），返回 `true`；否则返回 `false` 走本地滚动。
    /// Shift+滚轮强制本地滚动（逃生通道）。
    fn report_mouse_wheel(&mut self, up: bool, notches: usize) -> bool {
        if self.modifiers.shift_key() {
            return false;
        }
        #[cfg(feature = "input-editor")]
        if self.mouse_on_footer() {
            return false;
        }
        let Some((pane_idx, _id, col, row)) = self.viewport_cell_under_mouse() else {
            return false;
        };
        // 维持 F5 不变量：滚轮只作用于**焦点窗格**。悬停在别的窗格上时让位
        // 本地（本地回退滚的也是焦点窗格），否则同一手势的落点会随被悬停
        // 窗格是否开了鼠标上报而变，路由不一致。
        if pane_idx != self.tabs[self.active_tab].focused {
            return false;
        }
        let pane = &self.tabs[self.active_tab].panes[pane_idx];
        let proto = pane.term.mouse_protocol();
        if !proto.is_on() {
            return false;
        }
        let enc = pane.term.mouse_encoding();
        let kind = if up {
            MouseEventKind::WheelUp
        } else {
            MouseEventKind::WheelDown
        };
        let mods = self.mouse_mods();
        let mut buf = Vec::new();
        for _ in 0..notches.max(1) {
            if let Some(b) = encode_mouse(
                proto,
                enc,
                MouseEvent {
                    kind,
                    col,
                    row,
                    mods,
                },
            ) {
                buf.extend_from_slice(&b);
            }
        }
        if buf.is_empty() {
            return false;
        }
        if let Err(e) = self.tabs[self.active_tab].panes[pane_idx].write_user_input(&buf) {
            log::error!("滚轮上报写 PTY 失败: {e:#}");
        }
        self.window.request_redraw();
        true
    }

    /// DECSET 1007 Alternate Scroll：真实鼠标上报关闭、当前又在备用屏时，
    /// 把每档滚轮转换成一次普通光标上/下键写入 PTY。Codex 的 transcript /
    /// pager 全屏覆盖层使用这条路径。Shift 仍强制让位给 Lumen 本地回看。
    fn report_alternate_scroll_wheel(&mut self, up: bool, notches: usize) -> bool {
        if self.modifiers.shift_key() {
            return false;
        }
        #[cfg(feature = "input-editor")]
        if self.mouse_on_footer() {
            return false;
        }
        let Some((pane_idx, _id, _col, _row)) = self.viewport_cell_under_mouse() else {
            return false;
        };
        if pane_idx != self.tabs[self.active_tab].focused {
            return false;
        }
        let pane = &self.tabs[self.active_tab].panes[pane_idx];
        if pane.term.mouse_protocol().is_on()
            || !pane.term.is_alt_screen()
            || !pane.term.alternate_scroll()
        {
            return false;
        }
        let steps = notches.max(1);
        let buf = input::encode_alternate_scroll(up, steps);
        let pane = &mut self.tabs[self.active_tab].panes[pane_idx];
        match pane.write_user_input(&buf) {
            Ok(()) => pane.note_alternate_scroll_wheel(up, steps),
            Err(e) => log::error!("备用屏滚轮转方向键写 PTY 失败: {e:#}"),
        }
        self.window.request_redraw();
        true
    }

    /// 指针移动上报：协议为 Button（仅按住拖动时）或 Any（任意移动）时，把
    /// 移动编码成鼠标 motion 事件写 PTY，返回 `true`（调用方跳过本地拖选 /
    /// 链接 hover）；否则 `false` 走本地逻辑。Shift 按下时让位本地。
    /// 同一格内的抖动不重复上报（节流），避免 Any 模式刷爆 PTY。
    fn report_mouse_motion(&mut self) -> bool {
        if self.modifiers.shift_key() {
            return false;
        }
        #[cfg(feature = "input-editor")]
        if self.mouse_on_footer() {
            return false;
        }
        // 上报目标始终是焦点窗格（与按键 / 滚轮上报同源）。
        let focused_idx = self.tabs[self.active_tab].focused;
        let proto = self.tabs[self.active_tab].panes[focused_idx]
            .term
            .mouse_protocol();
        let dragging = self.any_button_held();
        // 仅 Any（任意移动）或 Button（拖动中）上报 motion；其余让位本地。
        let want = match proto {
            MouseProtocol::Any => true,
            MouseProtocol::Button => dragging,
            _ => false,
        };
        if !want {
            return false;
        }
        // 坐标解析：拖动中把上报钉在焦点窗格、坐标按其矩形夹紧（指针捕获，
        // 即便鼠标已拖到别的窗格 / 窗外）；纯 hover 则鼠标必须确实落在焦点
        // 窗格内容区，否则让位（不给焦点窗格发它管不到的移动）。
        let (pane_id, col, row) = if dragging {
            match self.viewport_cell_in_pane(focused_idx) {
                Some(t) => t,
                None => return false,
            }
        } else {
            match self.viewport_cell_under_mouse() {
                Some((pi, id, c, r)) if pi == focused_idx => (id, c, r),
                _ => return false,
            }
        };
        // 同格节流：未跨格（同窗格同格）不重复发，但仍视为「已被上报接管」
        // 返回 true。节流键带窗格身份，避免跨窗格落到同一视口格漏发。
        if self.mouse_report_last_cell == Some((pane_id, col, row)) {
            return true;
        }
        let enc = self.tabs[self.active_tab].panes[focused_idx]
            .term
            .mouse_encoding();
        let ev = MouseEvent {
            kind: MouseEventKind::Move(self.held_repr_button()),
            col,
            row,
            mods: self.mouse_mods(),
        };
        let Some(bytes) = encode_mouse(proto, enc, ev) else {
            return false;
        };
        self.mouse_report_last_cell = Some((pane_id, col, row));
        if let Err(e) = self.tabs[self.active_tab].panes[focused_idx].write_user_input(&bytes) {
            log::error!("鼠标移动上报写 PTY 失败: {e:#}");
        }
        self.window.request_redraw();
        true
    }

    /// 取指定镜像窗格的 (鼠标上报协议, 编码)；窗格不存在返回 None。镜像 `Terminal`
    /// 解析被控端 PTY 流（含 DECSET 鼠标模式），故与被控端同步。
    fn mirror_pane_proto_enc(&self, sid: SessionId) -> Option<(MouseProtocol, MouseEncoding)> {
        self.remote_ws
            .mirror_panes()
            .iter()
            .find(|p| p.session_id == sid)
            .map(|mp| (mp.term.mouse_protocol(), mp.term.mouse_encoding()))
    }

    /// 镜像态（控制端）鼠标**按钮**上报：被控端目标会话开了鼠标上报时，把
    /// 按下 / 抬起编码成鼠标上报、经 `send_input` 转发给被控端，返回 `true`（调用
    /// 方跳过本地镜像拖选 / 复制粘贴）；否则 `false`（走本地镜像逻辑）。与本地
    /// [`Self::report_mouse_button`] 对称：press 用鼠标所在镜像窗格、release 钉在
    /// 按下时记下的 `mirror_report_sid`（指针捕获，坐标按其矩形夹紧）。复用
    /// `mouse_report_held` 做多键并发配对（本地 / 镜像互斥，不会并发按住）。
    /// Shift 按下 = 本地选区逃生通道（仅按下一刻 latch）。
    fn report_mirror_mouse_button(&mut self, button: MouseButton, pressed: bool) -> bool {
        let Some(btn) = Self::map_report_button(button) else {
            return false;
        };
        let idx = Self::held_index(btn);
        if !pressed {
            // 这枚键按下当初没走上报（Shift 本地选区 / 上报未开）→ 释放也不碰上报。
            if !self.mouse_report_held[idx] {
                return false;
            }
            self.mouse_report_held[idx] = false;
            // 末键抬起即清拖动锚点 / 节流键——**无论下面转发成功与否**（早退路径也清，
            // 否则镜像窗格 mid-drag 消失 / 上报被关时残留陈旧 sid，违反「键抬即清」不变量）。
            let last = !self.any_button_held();
            // 钉在按下时的镜像窗格、坐标按其矩形夹紧（鼠标可能已拖到别处 / 窗外），
            // 保证发起窗格收到配对的 Release、被控端按钮不卡死；用 send_input_to 钉 sid。
            if let Some(sid) = self.mirror_report_sid {
                if let (Some((row, col)), Some((proto, enc))) = (
                    self.mirror_pane_cell_clamped(sid),
                    self.mirror_pane_proto_enc(sid),
                ) {
                    if proto.is_on() {
                        let ev = MouseEvent {
                            kind: MouseEventKind::Release(btn),
                            col,
                            row,
                            mods: self.mouse_mods(),
                        };
                        if let Some(bytes) = encode_mouse(proto, enc, ev) {
                            self.remote_ws.send_input_to(sid, &bytes);
                            self.window.request_redraw();
                        }
                    }
                }
            }
            if last {
                self.mirror_report_sid = None;
                self.mouse_report_last_cell = None;
            }
            return true;
        }
        // —— 按下：Shift = 本地选区逃生通道（仅按下一刻 latch）。
        if self.modifiers.shift_key() {
            return false;
        }
        // 本手势第一枚键：锚定鼠标所在镜像窗格做拖动目标 + 焦点；后续键（拖动中多键
        // 并发）复用已钉住的 sid、坐标按其矩形夹紧，不改锚点 / 不夺焦点（与本地「拖动
        // 钉发起窗格」语义一致，避免第二键把目标改投别格致先按住键失配对）。
        let first = !self.any_button_held();
        let (sid, row, col) = if first {
            let Some(cell) = self.mirror_pane_cell_at_mouse() else {
                return false;
            };
            cell
        } else {
            let Some(sid) = self.mirror_report_sid else {
                return false;
            };
            let Some((row, col)) = self.mirror_pane_cell_clamped(sid) else {
                return false;
            };
            (sid, row, col)
        };
        let Some((proto, enc)) = self.mirror_pane_proto_enc(sid) else {
            return false;
        };
        if !proto.is_on() {
            return false; // 上报未开 → 走本地镜像拖选
        }
        // F10：Ctrl+左键落在 URL 链接上 = 本地开链接逃生（对齐本地
        // report_mouse_button）。命中链接的 Ctrl+左键不转发被控端、held 不置位，
        // 释放因 held==false 自然配对走本地 Ctrl+Click 开链接；否则这枚点击被上报
        // 吞掉，镜像里 Claude 等全屏程序的链接永远点不开。非链接区 Ctrl+左键仍照常
        // 转发，不破坏被控端程序自身语义。
        if button == MouseButton::Left
            && self.modifiers.control_key()
            && self.mirror_link_at_mouse().is_some()
        {
            return false;
        }
        // 镜像焦点窗格已有本地选区高亮、且这次点的就是焦点窗格时，普通左键（非 Shift）
        // 按下 → 清选区并消费该次按下（点击取消选择）。镜像选区靠 Shift 逃生选出、
        // copy-on-select 后保留，普通单击本会被转发吞掉、清不掉——对齐本地
        // report_mouse_button 的「有选区时点击先取消」。`mirror_target_sid()==Some(sid)`
        // 守卫：只清被点焦点窗格自身的选区，点别的窗格不误清焦点窗格高亮、也不吞该点击
        // （与本地臂「focused_pane 一致、点哪清哪」对齐）。held 不置位、不转发被控端。
        if button == MouseButton::Left
            && self.remote_ws.mirror_target_sid() == Some(sid)
            && self.remote_ws.has_mirror_active_selection()
        {
            self.remote_ws.clear_mirror_active_selection();
            self.window.request_redraw();
            return true;
        }
        let ev = MouseEvent {
            kind: MouseEventKind::Press(btn),
            col,
            row,
            mods: self.mouse_mods(),
        };
        let Some(bytes) = encode_mouse(proto, enc, ev) else {
            return false;
        };
        if first {
            self.remote_ws.set_mirror_active_pane(sid);
            self.mirror_report_sid = Some(sid);
        }
        self.mouse_report_held[idx] = true;
        self.remote_ws.send_input_to(sid, &bytes);
        self.window.request_redraw();
        true
    }

    /// 镜像态（控制端）鼠标**移动 / 拖动**上报：被控端目标会话为 Any（任意移动）
    /// 或 Button（拖动中）时把移动编码、经 `send_input` 转发给被控端，返回 `true`
    /// （调用方跳过本地镜像拖选）；否则 `false`。与本地 [`Self::report_mouse_motion`]
    /// 对称：拖动中钉在 `mirror_report_sid`、坐标夹紧；纯 hover（Any）取鼠标所在
    /// 镜像窗格。同格节流（复用 `mouse_report_last_cell`）。Shift 让位本地。
    fn report_mirror_mouse_motion(&mut self) -> bool {
        if self.modifiers.shift_key() {
            return false;
        }
        let dragging = self.any_button_held();
        let (sid, row, col) = if dragging {
            // 拖动中：钉在按下窗格、坐标夹紧（指针捕获）。
            let Some(sid) = self.mirror_report_sid else {
                return false;
            };
            match self.mirror_pane_cell_clamped(sid) {
                Some((r, c)) => (sid, r, c),
                None => return false,
            }
        } else {
            // 纯 hover（Any）：仅对**当前焦点镜像窗格**上报——鼠标须确实落在它内容区，
            // 否则让位（不给非焦点镜像窗格发它管不到的移动，与本地 hover 同语义）。
            let Some((s, r, c)) = self.mirror_pane_cell_at_mouse() else {
                return false;
            };
            if self.remote_ws.mirror_target_sid() != Some(s) {
                return false;
            }
            (s, r, c)
        };
        let Some((proto, enc)) = self.mirror_pane_proto_enc(sid) else {
            return false;
        };
        let want = match proto {
            MouseProtocol::Any => true,
            MouseProtocol::Button => dragging,
            _ => false,
        };
        if !want {
            return false;
        }
        if self.mouse_report_last_cell == Some((sid, row, col)) {
            return true; // 同格节流：未跨格不重复转发，仍视为已接管
        }
        let ev = MouseEvent {
            kind: MouseEventKind::Move(self.held_repr_button()),
            col,
            row,
            mods: self.mouse_mods(),
        };
        let Some(bytes) = encode_mouse(proto, enc, ev) else {
            return false;
        };
        self.mouse_report_last_cell = Some((sid, row, col));
        self.remote_ws.send_input_to(sid, &bytes);
        self.window.request_redraw();
        true
    }

    /// 求鼠标当前位置命中的可点击链接（F10）：先查 OSC 8 显式超链接，
    /// 再按行文本扫描裸 URL / 文件路径（文件需在 cwd 下存在才算链接）。
    /// 焦点窗格的 footer（编辑器输入区）不参与终端链接识别。
    fn link_at_mouse(&self) -> Option<HoverLink> {
        #[cfg(feature = "input-editor")]
        if self.mouse_on_footer() {
            return None;
        }
        let (pane_idx, pane_id, abs, col) = self.cell_under_mouse()?;
        let pane = &self.tabs[self.active_tab].panes[pane_idx];
        // 备用屏幕（vim/less/Claude 等全屏程序）下也识别链接：alt 屏用独立 Grid
        // （`set_alt_screen` 建 `scrollback_limit=0` 的新网格），故 scrollback 恒空、
        // display_offset 夹在 0 滚不动 → `view_top_abs_line()==0`、`abs==row`，
        // 与 `line_by_abs(row)==screen.get(row)` 坐标自洽，取行安全。放开后 Claude
        // CLI 等上报态全屏程序里链接可 hover 高亮 + Ctrl+单击打开（配合
        // `report_mouse_button` 的 Ctrl+链接让位）。注：块选中另有 `is_alt_screen`
        // 守卫（释放分支）不受影响——alt 屏无 shell 命令块概念。
        // 1) OSC 8 显式超链接（区段与 URI 由终端侧直接给出）。
        if let Some((sc, ec, uri)) = pane.term.hyperlink_span_at(abs, col) {
            return Some(HoverLink {
                pane_id,
                line: abs,
                start_col: sc,
                end_col: ec,
                target: links::LinkTarget::Url(uri),
            });
        }
        // 2) 隐式 URL / 文件路径：扫描行文本（跳过宽字符占位格，建立
        //    显示列 ↔ 字符下标映射）。
        let row = pane.term.grid().line_by_abs(abs)?;
        let cells = row.cells();
        let mut cols: Vec<usize> = Vec::new();
        let mut text: Vec<char> = Vec::new();
        for (c, cell) in cells.iter().enumerate() {
            if cell.flags.contains(lumen_term::CellFlags::WIDE_SPACER) {
                continue;
            }
            cols.push(c);
            text.push(cell.ch);
        }
        // 命中字符下标：col 命中某显示列则取之；落在宽字符右半占位格时
        // 回退到其主格（最近的 ≤ col 的显示列）。
        let char_idx = cols
            .iter()
            .position(|&dc| dc == col)
            .or_else(|| cols.iter().rposition(|&dc| dc <= col))?;
        let (cs, ce, raw) = links::detect_link(&text, char_idx)?;
        let target = links::resolve(&raw, pane.term.cwd())?;
        // 字符下标区段 → 显示列区段（高亮用）。
        let start_col = *cols.get(cs)?;
        let end_col = cols.get(ce).copied().unwrap_or_else(|| {
            let last = ce.saturating_sub(1).min(cols.len().saturating_sub(1));
            let last_col = cols.get(last).copied().unwrap_or(start_col);
            let wide = cells
                .get(last_col)
                .is_some_and(|c| c.flags.contains(lumen_term::CellFlags::WIDE));
            last_col + if wide { 2 } else { 1 }
        });
        Some(HoverLink {
            pane_id,
            line: abs,
            start_col,
            end_col,
            target,
        })
    }

    /// 鼠标移动后更新链接 hover 态（F10）：单元格未变则跳过；变化时
    /// 重算 hover 链接并按需请求重绘（hover 下划线/手型光标）。
    fn update_link_hover(&mut self) {
        let probe = self
            .cell_under_mouse()
            .map(|(_, id, line, col)| (id, line, col));
        if probe == self.hover_probe_cell {
            return;
        }
        self.hover_probe_cell = probe;
        let new_link = self.link_at_mouse();
        let changed = match (&new_link, &self.hovered_link) {
            (Some(a), Some(b)) => {
                a.pane_id != b.pane_id
                    || a.line != b.line
                    || a.start_col != b.start_col
                    || a.end_col != b.end_col
            }
            (None, None) => false,
            _ => true,
        };
        self.hovered_link = new_link;
        if changed {
            self.window.request_redraw();
        }
    }

    /// 镜像态求鼠标命中的可点击链接（F10·**只放 URL**）：镜像显示的是**远端**
    /// 机器，而 [`links::open`] 在**本地**打开——URL 交本地浏览器语义正确，但
    /// 「本地文件路径」在镜像里其实是远端文件、本地会开错，故这里**只识别 URL**
    /// （OSC 8 超链接 + 隐式 http/https），丢弃文件路径候选、不碰 cwd / 文件系统。
    /// 回看态（渲染 hist_term）坐标系与 live 窗格不一致，MVP 不识别。
    fn mirror_link_at_mouse(&self) -> Option<HoverLink> {
        // 回看态渲染的是 hist_term scratch，坐标系与 live 窗格 term 不一致，且
        // hist 快照经 serialize_row_vt 不带 OSC 8——只在 live 跟随态识别。
        if self.remote_ws.mirror_pane_in_hist() {
            return None;
        }
        let (sid, row, col) = self.mirror_pane_cell_at_mouse()?;
        let mp = self
            .remote_ws
            .mirror_panes()
            .iter()
            .find(|p| p.session_id == sid)?;
        // 视口行 → 绝对行（镜像 term 有 scrollback，view_top 非 0；与本地
        // cell_under_mouse 同法）。
        let abs = mp.term.grid().view_top_abs_line() + row as u64;
        // 1) OSC 8 显式超链接（订阅后写入的内容才带 hyperlink_id；初始快照 /
        //    历史行经 serialize_row_vt 不含 OSC 8，靠下面隐式 URL 兜底）。
        if let Some((sc, ec, uri)) = mp.term.hyperlink_span_at(abs, col) {
            return Some(HoverLink {
                pane_id: sid,
                line: abs,
                start_col: sc,
                end_col: ec,
                target: links::LinkTarget::Url(uri),
            });
        }
        // 2) 隐式 http/https：扫行文本（跳过宽字符占位格，建显示列↔字符下标
        //    映射，与本地 link_at_mouse 同构），**只接受 RawLink::Url**。
        let grid_row = mp.term.grid().line_by_abs(abs)?;
        let cells = grid_row.cells();
        let mut cols: Vec<usize> = Vec::new();
        let mut text: Vec<char> = Vec::new();
        for (c, cell) in cells.iter().enumerate() {
            if cell.flags.contains(lumen_term::CellFlags::WIDE_SPACER) {
                continue;
            }
            cols.push(c);
            text.push(cell.ch);
        }
        let char_idx = cols
            .iter()
            .position(|&dc| dc == col)
            .or_else(|| cols.iter().rposition(|&dc| dc <= col))?;
        let (cs, ce, raw) = links::detect_link(&text, char_idx)?;
        // 只放 URL：文件路径候选丢弃（远端文件本地打不开 / 会开错）。
        let links::RawLink::Url(u) = raw else {
            return None;
        };
        // 字符下标区段 → 显示列区段（高亮用，与 link_at_mouse 同法）。
        let start_col = *cols.get(cs)?;
        let end_col = cols.get(ce).copied().unwrap_or_else(|| {
            let last = ce.saturating_sub(1).min(cols.len().saturating_sub(1));
            let last_col = cols.get(last).copied().unwrap_or(start_col);
            let wide = cells
                .get(last_col)
                .is_some_and(|c| c.flags.contains(lumen_term::CellFlags::WIDE));
            last_col + if wide { 2 } else { 1 }
        });
        Some(HoverLink {
            pane_id: sid,
            line: abs,
            start_col,
            end_col,
            target: links::LinkTarget::Url(u),
        })
    }

    /// 镜像态鼠标移动后更新链接 hover（F10）：与 [`Self::update_link_hover`]
    /// 对称，但探测**镜像**窗格、只认 URL。写同一 `hovered_link` /
    /// `hover_probe_cell` 字段——手型光标、提示浮层、渲染下划线全复用本地那套，
    /// 无需新增。
    fn update_mirror_link_hover(&mut self) {
        let probe = self
            .mirror_pane_cell_at_mouse()
            .and_then(|(sid, row, col)| {
                let abs = self
                    .remote_ws
                    .mirror_panes()
                    .iter()
                    .find(|p| p.session_id == sid)
                    .map(|mp| mp.term.grid().view_top_abs_line() + row as u64)?;
                Some((sid, abs, col))
            });
        if probe == self.hover_probe_cell {
            return;
        }
        self.hover_probe_cell = probe;
        let new_link = self.mirror_link_at_mouse();
        let changed = match (&new_link, &self.hovered_link) {
            (Some(a), Some(b)) => {
                a.pane_id != b.pane_id
                    || a.line != b.line
                    || a.start_col != b.start_col
                    || a.end_col != b.end_col
            }
            (None, None) => false,
            _ => true,
        };
        self.hovered_link = new_link;
        if changed {
            self.window.request_redraw();
        }
    }

    /// F3：后台线程检查更新。`manual=true` 时无更新/失败也回 toast 反馈；
    /// 自动检查无更新则静默（不唤醒主循环）。自动检查会更新节流时间戳。
    fn spawn_update_check(&mut self, manual: bool) {
        if !manual {
            self.settings.update.last_check_ms = Some(update::now_ms());
            if let Some(err) = self.settings.save() {
                log::warn!("F3：写盘检查时间戳失败: {err}");
            }
        }
        let tx = self.update_tx.clone();
        let proxy = self.proxy.clone();
        let net_proxy = self.settings.proxy.effective_url().map(str::to_owned);
        std::thread::spawn(move || {
            let msg = match update::check_for_update(net_proxy.as_deref()) {
                update::CheckResult::Newer(info) => update::UpdateMsg::Available(info),
                update::CheckResult::UpToDate if manual => update::UpdateMsg::UpToDate,
                update::CheckResult::Failed if manual => update::UpdateMsg::CheckFailed,
                // 自动检查无更新/失败：静默，不打扰、不唤醒主循环。
                _ => return,
            };
            let _ = tx.send(msg);
            let _ = proxy.send_event(PtyWake);
        });
    }

    /// F3：运行期定时检查更新——每 [`UPDATE_POLL_INTERVAL`] 自动查一次（不
    /// 只启动时），让长期开着 Lumen 不关的用户也能及时收到新版。
    ///
    /// `auto_check` 关闭时跳过本轮（共享原子镜像由设置页开关同步）。查到新版
    /// 经与 [`Self::spawn_update_check`] 同一 channel 回主循环，
    /// [`Self::drain_update_msgs`] 按 skip/下载态/同版本去重，不重复打扰。
    /// 在 init 内调用一次，起一个长驻守护线程（不单独管理生命周期，随进程
    /// 退出回收；主循环退出后 channel/proxy 发送失败即自行结束）。
    fn spawn_periodic_update_check(&self) {
        let tx = self.update_tx.clone();
        let proxy = self.proxy.clone();
        let enabled = self.update_auto_check.clone();
        let net_proxy = self.update_proxy.clone();
        let _ = std::thread::Builder::new()
            .name("lumen-update-poll".into())
            .spawn(move || loop {
                std::thread::sleep(UPDATE_POLL_INTERVAL);
                if !enabled.load(Ordering::Relaxed) {
                    continue; // 用户已关自动检查：本轮不打网络
                }
                // 读最新生效代理镜像（设置页改动会同步刷新；锁中毒则退化直连）。
                let p = net_proxy.lock().ok().and_then(|g| g.clone());
                if let update::CheckResult::Newer(info) = update::check_for_update(p.as_deref()) {
                    if tx.send(update::UpdateMsg::Available(info)).is_err() {
                        break; // 主循环已退出、通道关闭
                    }
                    let _ = proxy.send_event(PtyWake);
                }
            });
    }

    /// F3：后台下载安装包，完成/失败经 channel 回主循环。
    fn spawn_update_download(&mut self, info: &update::UpdateInfo) {
        // 防重入：已在下载则不再起第二个线程——否则两线程写同一
        // installer_dest（File::create 截断）会并发写坏安装包。
        if self.update_downloading {
            return;
        }
        let tx = self.update_tx.clone();
        let proxy = self.proxy.clone();
        let net_proxy = self.settings.proxy.effective_url().map(str::to_owned);
        let url = info.download_url.clone();
        let dest = update::installer_dest(&info.tag);
        self.update_downloading = true;
        std::thread::spawn(move || {
            let msg =
                match update::download_installer(&url, &dest, net_proxy.as_deref(), |_d, _t| {}) {
                    Ok(()) => update::UpdateMsg::DownloadDone(dest),
                    Err(e) => update::UpdateMsg::DownloadFailed(e),
                };
            let _ = tx.send(msg);
            let _ = proxy.send_event(PtyWake);
        });
    }

    /// F3：drain 更新消息（user_event 内调用）。返回 true = 请求优雅退出。
    ///
    /// 注：静默预下载模型下，下载完成只标记就绪+弹窗（不退出）；真正拉起
    /// 安装器+退出在用户点「立即更新」时由 window_event 的 Install 动作处理，
    /// 故本函数恒返回 false（保留返回值与退出请求接口供未来用）。
    fn drain_update_msgs(&mut self) -> bool {
        let want_exit = false;
        while let Ok(msg) = self.update_rx.try_recv() {
            match msg {
                update::UpdateMsg::Available(info) => {
                    // 用户已「跳过」该版本则不打扰。
                    if self.settings.update.skip_version.as_deref() == Some(info.tag.as_str()) {
                        continue;
                    }
                    // 同一版本已记录：一律不重复下载/toast（无论下载中/已就绪/
                    // 已失败）。失败重试交手动「检查更新」（它清 available 后重新
                    // 触发），避免下载持续失败时定时复查每 30 分钟反复下载+toast。
                    if self.update_available.as_ref().map(|u| u.tag.as_str())
                        == Some(info.tag.as_str())
                    {
                        continue;
                    }
                    // 新版本：Windows 记录并**后台静默下载**（Warp 式）——下载完成
                    // （DownloadDone）才弹窗安装；启动时给一条轻提示让用户知道
                    // 在后台下载（每个新版本仅一次，定时复查被上面的去重挡住）。
                    // 非 Windows 不自动安装（安装链路是 Inno Setup 专属）：直接
                    // toast 发现新版本并即刻允许弹窗，引导前往下载页手动更新。
                    if cfg!(windows) {
                        self.shell_state.toast.push(
                            shell::toast::ToastKind::Info,
                            i18n::strings().update_toast_downloading.to_owned(),
                        );
                        self.update_ready = None;
                        self.update_dismissed = false;
                        self.spawn_update_download(&info);
                    } else {
                        self.shell_state.toast.push(
                            shell::toast::ToastKind::Info,
                            i18n::fmt1(i18n::strings().update_toast_available_fmt, info.version),
                        );
                        self.update_dismissed = false;
                    }
                    self.update_available = Some(info);
                }
                update::UpdateMsg::UpToDate => self.shell_state.toast.push(
                    shell::toast::ToastKind::Info,
                    i18n::strings().update_toast_up_to_date.to_owned(),
                ),
                update::UpdateMsg::CheckFailed => self.shell_state.toast.push(
                    shell::toast::ToastKind::Warn,
                    i18n::strings().update_toast_check_failed.to_owned(),
                ),
                update::UpdateMsg::DownloadDone(path) => {
                    // 静默下载完成：标记就绪并弹窗（点「立即更新」直接拉起已
                    // 下好的安装器，无需再等下载）。
                    self.update_downloading = false;
                    self.update_ready = Some(path);
                    self.update_dismissed = false;
                    if let Some(v) = self.update_available.as_ref().map(|u| u.version) {
                        self.shell_state.toast.push(
                            shell::toast::ToastKind::Info,
                            i18n::fmt1(i18n::strings().update_toast_available_fmt, v),
                        );
                    }
                    self.window.request_redraw();
                }
                update::UpdateMsg::DownloadFailed(e) => {
                    // 静默自动下载失败：debug 记录、清状态，下次检查再试。
                    // 不弹窗/不 toast——自动下载失败不打扰用户（手动「检查更
                    // 新」会重试）。
                    log::debug!("F3：后台下载失败 {e}");
                    self.update_downloading = false;
                    self.update_ready = None;
                }
            }
        }
        want_exit
    }

    /// IME 事件是否应路由给 composer 编辑器，即便 `terminal_focused` 因切
    /// tab / 点击 composer 时的焦点翻转**时序**短暂为 `false`（composer Win10
    /// 中文首字 bug 的激进修复，H1）。
    ///
    /// 条件：**无任何 egui 覆盖层/模态打开**（否则 IME 应归 egui 输入框，
    /// 放行会双投/劫持）+ 焦点窗格处于 [`mode::InputMode::Compose`]（提示符
    /// 等待输入，composer 可用）。满足时，焦点翻转窗口期到达的首个
    /// `Ime::Preedit` 不再漏给 egui（画在默认控件位 ≈ 最左）、也不再被
    /// Lumen 的 `!terminal_focused` 闸丢弃，而是直达 composer。
    ///
    /// Win11 常态下打字时 `terminal_focused` 已为 `true`，根本不进此分支
    /// （`bypass_egui` 的 `terminal_focused && Ime` 项已覆盖），故对 Win11
    /// 正常路径零影响；仅焦点翻转窗口期生效。
    fn ime_should_route_to_composer(&self) -> bool {
        if self.app_lock.is_locked() || self.settings.layout.view_mode.is_ssh() {
            return false;
        }
        #[cfg(feature = "input-editor")]
        {
            let overlay = self.shell_state.settings.open
                || self.shell_state.login.open
                || self.shell_state.history_search.open
                || (self.shell_state.completion.open && !self.shell_state.completion.passive)
                || self.shell_state.renaming.is_some()
                || self.shell_state.pane_renaming.is_some()
                || self.shell_state.ssh_session_renaming.is_some()
                || self.shell_state.filetree.dialog_open()
                || self.shell_state.text_editor.is_visible()
                // 文件树搜索框（egui TextEdit）聚焦时能收 IME，须视作覆盖层、
                // 把 IME 交还 egui，否则激进路由会把往搜索框打的中文劫持进
                // 终端 composer（对抗审查 IME 项）。
                || self.shell_state.filetree.search_open();
            if overlay {
                return false;
            }
            let (ti, pi) = (self.active_tab, self.tabs[self.active_tab].focused);
            effective_session_mode(&self.tabs[ti].panes[pi], self.force_fallback)
                == mode::InputMode::Compose
        }
        #[cfg(not(feature = "input-editor"))]
        {
            false
        }
    }

    /// 切换激活 tab：清掉目标 tab **全部窗格**的冻结计时与渲染计划
    /// （属于「上次激活期间」的旧时间轴，带过来会借用过期的调度），
    /// 清未读点，同步窗口标题并立即重绘。无覆盖层/重命名时终端拿
    /// 键盘/IME 焦点。
    fn activate(&mut self, idx: usize) {
        // 切 Tab 同样打断鼠标上报拖动：先向旧 tab 焦点（=发起）窗格补发
        // Release（须在改 active_tab 前），否则原窗格程序留下幻影按住。
        self.release_held_report_buttons();
        // 换出 tab 的拖选手势随切换结束：按住左键 Ctrl+Tab 切走后，
        // Released 只检查新焦点窗格的 selecting，旧窗格的标志会永久
        // 残留——切回时不按键鼠标一动就「幽灵拖选」，且 Ctrl+C 被
        // 选区复制分支吞掉。close_tab 路径下旧下标可能已越界
        // （删的是末位激活 tab），用 get_mut 防御。
        if let Some(prev) = self.tabs.get_mut(self.active_tab) {
            for p in &mut prev.panes {
                p.selecting = false;
            }
        }
        self.active_tab = idx;
        for s in &mut self.tabs[idx].panes {
            s.cursor_frozen_at = None;
            s.redraw_at = None;
            s.redraw_hard_at = None;
            s.redraw_abs_at = None;
            // 离屏纹理里还是后台期间的旧画面：下一帧必须渲染本窗格，
            // 即使它正处于 DEC 2026 同步区间（画半成品也好过画旧帧）。
            // 欠帧起点回拨 REDRAW_ABS_CAP 让它直接「超龄」：若新数据
            // 赶在重绘执行前重新武装了渲染计划，门控也不许把旧画面多
            // 留哪怕一帧（checked_sub 仅防进程启动极早期的理论下溢）。
            s.term_frame_due_since = Some(
                Instant::now()
                    .checked_sub(REDRAW_ABS_CAP)
                    .unwrap_or_else(Instant::now),
            );
            s.has_unseen_output = false;
        }
        // 焦点归属按覆盖层/重命名状态计算，不无条件抢回：后台 shell
        // 自行退出触发的 activate 可能发生在用户正往设置页/登录表单/
        // 重命名框打字时，无脑置 true 会让在途按键直写邻位会话的 PTY
        // （bypass_egui 即刻生效，等不到下一帧的纠偏）。
        self.terminal_focused = self.terminal_focus_allowed();
        // 防御（composer Win10 IME）：切 tab 复位焦点窗格的 IME 预编辑残留，
        // 防止切回时上一 tab 半成品组合串串入。激进修复（见
        // ime_should_route_to_composer）负责焦点翻转期首字直达 composer。
        #[cfg(feature = "input-editor")]
        {
            let pi = self.tabs[idx].focused;
            if let Some(p) = self.tabs[idx].panes.get_mut(pi) {
                p.preedit = None;
            }
        }
        log::info!(
            "[IME-ACTIVATE] active_tab={} terminal_focused={}",
            self.active_tab,
            self.terminal_focused
        );
        self.update_window_title();
        self.window.request_redraw();
        // 激活下标是持久化状态的一部分：切换即落盘（F4）。
        self.persist_sessions();
    }

    /// 切换激活 tab 内的焦点窗格（点击窗格 / F5 焦点路由）。窗口
    /// 标题、文件树 cwd、键盘/IME/滚轮路由随之跟随新焦点窗格。
    fn focus_pane(&mut self, idx: usize) {
        {
            let tab = &self.tabs[self.active_tab];
            if idx >= tab.panes.len() || idx == tab.focused {
                return;
            }
            // 最大化期间焦点强制为最大化格（P14）：隐藏窗格的矩形不在
            // 本帧布局里、正常路径点不到，纯防御（陈旧矩形/竞态）。
            if tab.maximized.is_some_and(|m| m != idx) {
                return;
            }
        }
        // 焦点真的要换走：若鼠标上报拖动正进行，先向旧焦点（=拖动发起）
        // 窗格补发 Release，避免原窗格里的程序留下幻影按住（与下面清
        // selecting 同源，须在改 focused 前）。
        self.release_held_report_buttons();
        let tab = &mut self.tabs[self.active_tab];
        // 旧焦点窗格的拖选手势随切焦点结束（与 activate 同理：标志
        // 残留会在切回时产生幽灵拖选）。窗格本身保持可见、渲染计划
        // 与冻结计时是「正在上屏」的活状态，不清。
        tab.panes[tab.focused].selecting = false;
        tab.focused = idx;
        // accent 边框移动 + 标题跟随需要一帧重绘。
        self.update_window_title();
        self.window.request_redraw();
        // 焦点窗格下标是持久化状态的一部分（F5）。
        self.persist_sessions();
    }

    /// 把路径文本插入指定窗格的命令行（需求3）：文件树拖放与外部文件
    /// 拖入终端共用。下标 pi 对应本帧布局，结构若已变（增删窗格）则越界
    /// 防御跳过。先聚焦落点窗格（插入后接着编辑的就是它）。Compose 态
    /// 进 footer 编辑器（dispatch InsertText），其余态直写 PTY。转义/同形
    /// 引号/控制字符防御见 filetree::path_insert_text（空串 = 路径被拒）。
    fn insert_path_into_pane(&mut self, pi: usize, path: &std::path::Path) {
        // 覆盖层/重命名打开时吞掉拖入（与 activate 的 open 标志门控对齐）：
        // 否则 OS 级 WindowEvent::DroppedFile 会越过可见的设置/登录/历史/
        // 补全模态，把路径文本注入背后 shell 并抢回 terminal_focused。文件
        // 树拖放本被模态 backdrop 遮挡不触发，这里加闸是纵深防御 + 专门
        // 覆盖新增的外部 DroppedFile 路径。
        if self.shell_state.settings.open
            || self.shell_state.login.open
            || self.shell_state.history_search.open
            || (self.shell_state.completion.open && !self.shell_state.completion.passive)
            || self.shell_state.renaming.is_some()
            || self.shell_state.pane_renaming.is_some()
            || self.shell_state.ssh_session_renaming.is_some()
            || self.shell_state.filetree.dialog_open()
            || self.shell_state.text_editor.is_visible()
        {
            return;
        }
        // 下标对应布局快照；本帧结构若已被改变则跳过本次插入（防御，
        // 拖放与增删同帧发生的概率可忽略）。
        if pi >= self.tabs[self.active_tab].panes.len() {
            return;
        }
        // 先聚焦落点窗格（落点若非焦点窗格，先切焦点再插入，与原行为一致）。
        self.focus_pane(pi);
        let (ti, pi_focused) = (self.active_tab, self.tabs[self.active_tab].focused);

        // Compose 态分流：进编辑器；其余态直写 PTY。
        #[cfg(feature = "input-editor")]
        {
            let mode =
                effective_session_mode(&self.tabs[ti].panes[pi_focused], self.force_fallback);
            if mode == mode::InputMode::Compose {
                // path_insert_text_str 与 path_insert_text 同一引号规则；
                // 控制字符路径返回 None，静默跳过（纵深防御）。
                if let Some(text) = shell::filetree::path_insert_text_str(path) {
                    // 尾随空格：路径后方便光标继续编辑（与 PTY 路径行为对称）。
                    let text_with_space = format!("{text} ");
                    self.dispatch(
                        action::Action::Edit(action::EditAction::InsertText(text_with_space)),
                        ti,
                        pi_focused,
                    );
                    self.terminal_focused = true;
                }
                // Compose 路径处理完毕，不走下方 PTY 路径
                // （下方 PTY 块受 feature-gate else 保护）。
            } else {
                // 非 Compose 态：原写 PTY 路径。
                let bytes = shell::filetree::path_insert_text(path);
                if !bytes.is_empty() {
                    let s = self.focused_pane_mut();
                    s.term.grid_mut().scroll_to_bottom();
                    if let Err(e) = s.write_user_input(&bytes) {
                        error!("写入 PTY 失败: {e:#}");
                    }
                    self.terminal_focused = true;
                }
            }
        }
        // feature = "input-editor" 未开启时：全量走原 PTY 路径。
        #[cfg(not(feature = "input-editor"))]
        {
            let bytes = shell::filetree::path_insert_text(path);
            if !bytes.is_empty() {
                let s = self.focused_pane_mut();
                s.term.grid_mut().scroll_to_bottom();
                if let Err(e) = s.write_user_input(&bytes) {
                    error!("写入 PTY 失败: {e:#}");
                }
                self.terminal_focused = true;
            }
        }
    }

    /// 释放窗格的渲染资源：离屏纹理 + 行排版缓存即刻释放；egui 侧
    /// 的纹理注册推迟到帧呈现后注销（关闭动作可能发生在 run_ui 之
    /// 后，本帧 shape 仍引用该纹理；离屏视图被 egui 注册表持有引用
    /// 计数，先行 drop 不影响本帧采样）。
    fn release_pane_resources(&mut self, sid: SessionId) {
        self.renderer.drop_offscreen(sid);
        if let Some(tex) = self.pane_textures.remove(&sid) {
            self.pending_tex_free.push(tex);
        }
    }

    /// 关闭整个 tab：窗格全部移除即随 `PtySession` Drop 杀掉子进程；
    /// 各窗格通道的接收端同时销毁，转发线程 send 失败自然退出（残留
    /// 事件随通道一并丢弃，无需清理）。
    /// 返回是否已无 tab（调用方应退出应用）。
    fn close_tab(&mut self, idx: usize) -> bool {
        // 关的是当前激活（=拖动发起）tab 时，移除前先补发鼠标上报的 Release
        // 给它的焦点窗格（移除后该 tab 连同窗格 drop、无处可补；且 activate
        // 在移除后才补发会落到左移顶上来的另一个 tab，误投）。关非激活 tab
        // 只是下标平移、激活 tab 身份不变，不打断拖动。close_pane→close_tab
        // 链里 close_pane 已先 flush，这里即空操作（held 已清，不重复补发）。
        if idx == self.active_tab {
            self.release_held_report_buttons();
        }
        let removed = self.tabs.remove(idx);
        info!(
            "关闭 tab id={}（{} 个窗格）",
            removed.id,
            removed.panes.len()
        );
        let sids: Vec<SessionId> = removed.panes.iter().map(|p| p.id).collect();
        drop(removed);
        for sid in sids {
            self.release_pane_resources(sid);
        }
        if self.tabs.is_empty() {
            // 最后一个 tab 关闭即退出：以退出瞬间的（空）列表落盘，
            // 下次启动回到单默认会话（F4）。
            self.persist_sessions();
            return true;
        }
        // tab 列表变化必须立即反映到侧栏：后台 shell 自行退出关 tab
        // 不经过 activate()，没有这句时已死条目会一直挂在侧栏，直到
        // 下一个无关事件碰巧触发重绘。激活路径里 activate() 也会
        // request_redraw，重复请求由 winit 合并，无害。
        self.window.request_redraw();
        if idx < self.active_tab {
            // 移除位在激活位之前：激活 tab 整体左移一位，无需切换。
            self.active_tab -= 1;
        } else if idx == self.active_tab {
            // 关闭激活 tab：切到邻位（右邻顶上原位；无右邻取末位）。
            self.activate(idx.min(self.tabs.len() - 1));
        }
        // 关 tab 是结构性变更：落盘（activate 路径已写过时快照一致，
        // 自动跳过）。
        self.persist_sessions();
        false
    }

    /// 关闭单个窗格（shell 退出 / Ctrl+Shift+W）：最后一个窗格时 =
    /// 关整个 tab。返回是否已无 tab（调用方应退出应用）。
    fn close_pane(&mut self, ti: usize, pi: usize) -> bool {
        // 关的是当前焦点（=拖动发起）窗格时，移除前先补发鼠标上报的 Release
        // （移除后该窗格 drop、无处可补）。关非焦点窗格只是索引平移、焦点窗格
        // 身份不变，不打断拖动，无需补发。
        if ti == self.active_tab && pi == self.tabs[ti].focused {
            self.release_held_report_buttons();
        }
        if self.tabs[ti].panes.len() <= 1 {
            return self.close_tab(ti);
        }
        let removed = self.tabs[ti].panes.remove(pi);
        let sid = removed.id;
        info!("关闭窗格 id={sid}（tab id={}）", self.tabs[ti].id);
        drop(removed);
        self.release_pane_resources(sid);
        // 焦点下标调整（与关 tab 的激活下标同款规则：移除位之前的
        // 整体左移；关焦点窗格时右邻顶上原位、无右邻取末位）。
        let tab = &mut self.tabs[ti];
        if pi < tab.focused {
            tab.focused -= 1;
        } else if pi == tab.focused {
            tab.focused = pi.min(tab.panes.len() - 1);
        }
        // 最大化下标随移除调整（P14）：关最大化格自动退出（其余格
        // 还原可见）；关它之前的隐藏格（shell 自行退出）下标左移。
        tab.maximized = match tab.maximized {
            Some(m) if pi == m => None,
            Some(m) if pi < m => Some(m - 1),
            other => other,
        };
        // 剩单格无最大化语义（不变量：Some 时必有多格）。
        if tab.panes.len() == 1 {
            tab.maximized = None;
        }
        // 删窗格重置比例为均分（与增窗格同理，F7 拍板）。
        tab.layout = PaneLayout::uniform(tab.panes.len());
        if ti == self.active_tab {
            // 可见窗格布局变化 + 标题可能跟随新焦点窗格；后台 tab 关
            // 窗格也要重绘侧栏（未读点可能随窗格消失），统一请求。
            self.update_window_title();
        }
        self.window.request_redraw();
        // 关窗格是结构性变更：落盘（F5）。
        self.persist_sessions();
        false
    }

    /// 单窗格 shell 退出后在原位重启一个新 shell（海风哥 2026-06-13 体验
    /// 优化：单窗口 `exit` 不关应用，而是换一个干净的 PowerShell 继续用；
    /// 多窗格场景仍走 [`Self::close_pane`] 关掉退出的那格）。
    ///
    /// 沿用旧窗格的网格行列与 cwd（最后上报的 OSC 9;9 目录，失效回初始/
    /// 默认目录），id 新分配、旧窗格渲染资源释放。返回 `true` 表示重启失败
    /// 且退回关闭后已无 tab（调用方应退出应用）。
    fn respawn_pane(&mut self, ti: usize, pi: usize) -> bool {
        let (rows, cols, cwd, old_id) = {
            let old = &self.tabs[ti].panes[pi];
            let g = old.term.grid();
            let cwd = old
                .term
                .cwd()
                .map(std::path::Path::to_path_buf)
                .or_else(|| old.initial_cwd.clone())
                // spawn 约定由调用方先验证目录仍存在。
                .filter(|p| p.is_dir());
            (g.rows(), g.cols(), cwd, old.id)
        };
        let id = self.next_session_id;
        self.next_session_id += 1;
        match Session::spawn(
            id,
            rows,
            cols,
            SCROLLBACK,
            self.wake_pending.clone(),
            self.proxy.clone(),
            cwd.as_deref(),
            self.settings.proxy.effective_url(),
        ) {
            Ok(s) => {
                // 原位替换：旧 Session 随赋值 Drop（杀旧进程——已退出）。
                self.tabs[ti].panes[pi] = s;
                self.release_pane_resources(old_id);
                info!("单窗格 shell 退出，已在原位重启新 shell（id {old_id}→{id}）");
                self.update_window_title();
                self.window.request_redraw();
                // 会话内容变更（id 换绑）：落盘。
                self.persist_sessions();
                false
            }
            Err(e) => {
                // 重启失败（系统起不了进程）：退回关闭，避免卡死无响应窗格。
                error!("单窗格 shell 重启失败: {e:#}，退回关闭窗格");
                self.close_pane(ti, pi)
            }
        }
    }

    /// 新建 tab（单窗格，继承当前 shell 配置）并切换为激活。
    /// 行列数先取焦点窗格网格，下一帧按实际窗格矩形校正。
    fn new_tab(&mut self) {
        if self.new_tab_unfocused().is_some() {
            // activate 内部会落盘会话快照（新建是结构性变更）。
            self.activate(self.tabs.len() - 1);
        }
    }

    /// 新建一个会话(tab)但**不切换焦点 / 不 activate**，返回新 tab id。供 part3d 远程
    /// [`RemoteFrame::NewTab`]：控制端远程建会话**不得移动被控端焦点**（需求 e「互不同步」）。
    /// 不在此落盘——调用方按需 [`Self::persist_sessions`]（本地 [`Self::new_tab`] 走 activate 落盘）。
    fn new_tab_unfocused(&mut self) -> Option<TabId> {
        let g = self.focused_pane().term.grid();
        let (rows, cols) = (g.rows(), g.cols());
        let id = self.next_session_id;
        self.next_session_id += 1;
        match Session::spawn(
            id,
            rows,
            cols,
            SCROLLBACK,
            self.wake_pending.clone(),
            self.proxy.clone(),
            None,
            self.settings.proxy.effective_url(),
        ) {
            Ok(s) => {
                let tab_id = self.next_tab_id;
                self.next_tab_id += 1;
                self.tabs.push(Tab {
                    id: tab_id,
                    custom_title: None,
                    panes: vec![s],
                    focused: 0,
                    layout: PaneLayout::uniform(1),
                    maximized: None,
                });
                Some(tab_id)
            }
            Err(e) => {
                error!("新建会话失败: {e:#}");
                None
            }
        }
    }

    /// 激活 tab 内新增一个窗格（F5：Ctrl+Shift+D；+ 按钮归批次2）。
    /// 满 [`MAX_PANES`] 时 toast 提示。新窗格继承焦点窗格的 cwd
    /// （Warp/Windows Terminal 分屏惯例；OSC 9;9 未上报时退恢复时的
    /// 初始目录，目录已失效则回默认），并自动成为焦点。
    ///
    /// # B3 根治：spawn 前预计算新窗格真实尺寸
    ///
    /// 旧实现用「焦点窗格当前行列」spawn，但新格加入后布局从 N 变
    /// N+1 格均分，各格真实尺寸完全不同。时序：
    ///   shell 按错误宽度打印首个提示符
    ///   → 下一帧 egui 出真实矩形 → resize
    ///   → ConPTY 按新列宽 reflow，PSReadLine 坐标簿仍按旧假设
    ///   → 该行后续编辑持续列错位（截图混叠形态）
    ///   → 回车开新行 PSReadLine 重新定位 → 正常
    ///
    /// 修复：spawn 前按「加入后布局」预计算新格矩形，复用
    /// [`estimate_restored_pane_px`] 同源逻辑（n+1 均分、扣标题栏、
    /// 换算行列），spawn 即终态尺寸，首帧零 resize。
    fn new_pane(&mut self) {
        if self.tabs[self.active_tab].panes.len() >= MAX_PANES {
            self.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                i18n::fmt1(i18n::strings().toast_max_panes_fmt, MAX_PANES),
            );
            // push 不在 egui 帧内：请求一帧立即显示。
            self.window.request_redraw();
            return;
        }
        // 新建窗格会把焦点移到新格：先给旧焦点（=拖动发起）窗格补发鼠标
        // 上报的 Release，避免它残留幻影按住（held 是全局态、不随焦点自愈）。
        self.release_held_report_buttons();

        // —— 预计算新窗格真实尺寸（B3 根治）——
        // 新格加入后共 n+1 格，布局均分，新格是最后一个（index=n）。
        // area 估算与 estimate_restored_pane_px 同源：扣侧栏/顶栏/文件树。
        let n = self.tabs[self.active_tab].panes.len();
        let scale = self.egui_ctx.pixels_per_point();
        let inner = self.window.inner_size();
        let sidebar_px = (self.settings.layout.sidebar_width * scale).round();
        // 顶部双栏合计占高：标题栏 + 应用工具栏（F12 批1 工具栏入栏后
        // 工作区 y 起点下移，估算须同步扣除）。
        let topbar_px = ((shell::topbar::HEIGHT + shell::toolbar::HEIGHT) * scale).round();
        let ft_width = self
            .shell_state
            .filetree
            .effective_width(self.settings.layout.filetree_width);
        let ft_px = (ft_width * scale).round();
        let term_w_px = (inner.width as f32 - sidebar_px).max(1.0);
        let term_h_px = (inner.height as f32 - topbar_px).max(1.0);
        // est_area 为逻辑点（与 estimate_restored_pane_px 入参单位一致）。
        let est_area = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2((term_w_px - ft_px).max(1.0) / scale, term_h_px / scale),
        );
        // n+1 格均分，取第 n 个矩形（新格）。
        let new_pane_layout = PaneLayout::uniform(n + 1);
        let est_px = estimate_restored_pane_px(est_area, &new_pane_layout, n + 1, None, scale);
        // 估算不可用时兜底焦点窗格当前尺寸（防御，不应发生）。
        // M4.1 批C 注：新窗格 spawn 时 term 尚无 block 数据 → Fallback 态 →
        // footer_px=0，此处用 grid_size_for（等价 footer_px=0）正确。
        // 首帧实际布局后 RedrawRequested 会按真实 footer 高度做精确 resize。
        let (rows, cols) = est_px
            .get(n)
            .map(|&(w, h)| self.renderer.grid_size_for(w, h))
            .unwrap_or_else(|| {
                let g = self.focused_pane().term.grid();
                (g.rows(), g.cols())
            });
        info!(
            "new_pane 预计算：n+1={} 格，新格 rows={rows} cols={cols}（est_area={:?}）",
            n + 1,
            est_area,
        );

        let cwd = self
            .focused_pane()
            .term
            .cwd()
            .map(std::path::Path::to_path_buf)
            .or_else(|| self.focused_pane().initial_cwd.clone())
            // spawn 约定由调用方先验证目录仍存在。
            .filter(|p| p.is_dir());
        let id = self.next_session_id;
        self.next_session_id += 1;
        match Session::spawn(
            id,
            rows,
            cols,
            SCROLLBACK,
            self.wake_pending.clone(),
            self.proxy.clone(),
            cwd.as_deref(),
            self.settings.proxy.effective_url(),
        ) {
            Ok(s) => {
                let tab = &mut self.tabs[self.active_tab];
                // 最大化态先自动退出再加格（P14：新格要可见，隐藏着
                // 加格没有意义）。
                tab.maximized = None;
                tab.panes.push(s);
                tab.focused = tab.panes.len() - 1;
                // 增窗格重置比例为均分（F7 拍板：简单正确优先——网格
                // 结构随数量变化，旧权重的形状已不适用）。
                tab.layout = PaneLayout::uniform(tab.panes.len());
                // 布局变化：下一帧 egui 产出新窗格矩形并触发逐窗格
                // 离屏重建 + term/pty resize。
                self.update_window_title();
                self.window.request_redraw();
                // 增窗格是结构性变更：落盘（F5）。
                self.persist_sessions();
            }
            Err(e) => {
                error!("新建窗格失败: {e:#}");
                self.shell_state.toast.push(
                    shell::toast::ToastKind::Error,
                    i18n::fmt1(i18n::strings().toast_new_pane_failed_fmt, &e),
                );
                self.window.request_redraw();
            }
        }
    }

    /// 被控端：远程新建窗格到 tab `ti`（控制端 `PaneOp::New`，修①）。尺寸取该 tab 焦点窗格当前网格
    /// （后续 SubViewport / 布局同步会按控制端镜像精确 resize）；满 `MAX_PANES` 则忽略（fire-and-forget，
    /// 无 toast——控制端不该弹被控端的提示）。**不抢被控端自身焦点**（需求 c/e）：新格加在末尾、`focused`
    /// 不动。窗格集变化经 `SubscriptionStarted` 重发同步回控制端。
    fn new_remote_pane_in(&mut self, ti: usize) {
        if self.tabs[ti].panes.len() >= MAX_PANES {
            log::warn!(
                "远程新建窗格忽略：tab id={} 已达 MAX_PANES",
                self.tabs[ti].id
            );
            return;
        }
        let (rows, cols) = {
            let g = self.tabs[ti].focused_pane().term.grid();
            (g.rows(), g.cols())
        };
        let cwd = self.tabs[ti]
            .focused_pane()
            .term
            .cwd()
            .map(std::path::Path::to_path_buf)
            .or_else(|| self.tabs[ti].focused_pane().initial_cwd.clone())
            .filter(|p| p.is_dir());
        let id = self.next_session_id;
        self.next_session_id += 1;
        match Session::spawn(
            id,
            rows,
            cols,
            SCROLLBACK,
            self.wake_pending.clone(),
            self.proxy.clone(),
            cwd.as_deref(),
            self.settings.proxy.effective_url(),
        ) {
            Ok(s) => {
                let tab = &mut self.tabs[ti];
                tab.maximized = None; // 加格前退最大化（新格要可见）。
                tab.panes.push(s); // 末尾追加；不改 focused（被控端焦点不动）。
                tab.layout = PaneLayout::uniform(tab.panes.len());
                self.persist_sessions();
                self.window.request_redraw();
            }
            Err(e) => error!("远程新建窗格失败（tab {ti}）: {e:#}"),
        }
    }

    /// 最大化/还原激活 tab 的窗格 `pi`（P14：标题栏按钮 /
    /// Ctrl+Shift+Enter）。已处于最大化态时还原（无论 `pi` 是哪格
    /// ——可见的只有最大化格，按钮/快捷键都落在它身上）；普通态把
    /// `pi` 最大化并强制聚焦。布局权重不动：还原即回原比例。
    fn toggle_maximize_pane(&mut self, ti: usize, pi: usize) {
        // 仅当操作的是激活 tab、且「进入最大化」会改焦点时，先补发鼠标上报
        // 拖动的 Release（避免旧焦点窗格幻影按住）。ti 可能是后台 tab（远程
        // 最大化非激活 tab 的窗格），那种情况无本地拖动、无需补发。
        let will_change_focus = {
            let tab = &self.tabs[ti];
            if pi >= tab.panes.len() {
                return; // 防御：结构刚变更的过渡帧
            }
            ti == self.active_tab && tab.maximized.is_none() && tab.panes.len() > 1
        };
        if will_change_focus {
            self.release_held_report_buttons();
        }
        let tab = &mut self.tabs[ti];
        if pi >= tab.panes.len() {
            return; // 防御：结构刚变更的过渡帧（与上面同判，借用分隔）
        }
        if tab.maximized.is_some() {
            tab.maximized = None;
            // 还原后其余窗格的离屏纹理还是隐藏前的旧画面：强制下一帧
            // 渲染（同 activate 的「超龄欠帧」手法——即使正处同步
            // 区间也不许把旧帧多留一帧）。
            //
            // 【补帧三件套·勿拆】「帧内清 maximized」必须配齐三件事：①此处
            // 清 maximized ②给**所有**窗格设超龄 term_frame_due_since
            // ③本 fn 末尾 request_redraw（见 2344 附近）。缘由：main 的
            // resize 循环（6029-6055）以「矩形退化」为唯一判据跳过隐藏窗格
            // ——本帧 run_ui 已按改前的 maximized=Some 算出隐藏窗格的
            // NOTHING 占位矩形，故它们这帧被退化 guard 跳过、不被 resize
            // （这正是防「还原帧把隐藏窗格 resize 成 1 列、每行截断丢内容」
            // 的修复）；隐藏窗格改靠这里的超龄欠帧在**下一帧**按正确分屏
            // 矩形补绘上屏。reset_pane_layout 的还原分支是同根因第二入口，
            // 三件套须与此处保持一致；将来任何新增「帧内清 maximized」的
            // 动作都得复制这套，否则隐藏窗格会被跳过却拿不到下一帧补绘。
            for s in &mut tab.panes {
                s.term_frame_due_since = Some(
                    Instant::now()
                        .checked_sub(REDRAW_ABS_CAP)
                        .unwrap_or_else(Instant::now),
                );
            }
        } else {
            if tab.panes.len() <= 1 {
                return; // 单窗格本就满屏，无最大化语义
            }
            // 焦点强制为最大化格（旧焦点窗格的拖选手势随切焦点结束，
            // 与 focus_pane 同理）。
            tab.panes[tab.focused].selecting = false;
            tab.focused = pi;
            tab.maximized = Some(pi);
            // 隐藏窗格清残留渲染计划（后台消化不再武装新计划，残留
            // 计划会让 about_to_wait 空打一帧）。
            for (i, s) in tab.panes.iter_mut().enumerate() {
                if i != pi {
                    s.redraw_at = None;
                    s.redraw_hard_at = None;
                    s.redraw_abs_at = None;
                }
            }
        }
        // 布局变化：下一帧 egui 产出新矩形并触发离屏重建 + resize；
        // 最大化态是持久化状态的一部分（P14，重启保持）。
        self.update_window_title();
        self.window.request_redraw();
        self.persist_sessions();
    }

    /// 一键恢复默认布局（P15：顶栏「▦」）：激活 tab 的行/列权重全部
    /// 恢复均分；处于最大化态先退出（其余窗格还原可见并强制补帧）。
    /// 复位后落盘。单窗格/已均分且非最大化时无事可做。
    fn reset_pane_layout(&mut self) {
        let tab = &mut self.tabs[self.active_tab];
        if tab.panes.len() <= 1 {
            return; // 顶栏按钮单窗格时已禁用，纯防御
        }
        let uniform = PaneLayout::uniform(tab.panes.len());
        if tab.maximized.is_some() {
            tab.maximized = None;
            // 与 toggle_maximize_pane 的还原分支同款（同根因第二入口）：
            // 隐藏窗格的纹理还是旧画面，强制下一帧渲染。补帧三件套（清
            // maximized + 给所有窗格设超龄 term_frame_due_since + 末尾
            // request_redraw，见 2372 附近）须与 toggle_maximize_pane 保持
            // 一致——缘由见那里的「补帧三件套·勿拆」注释。
            for s in &mut tab.panes {
                s.term_frame_due_since = Some(
                    Instant::now()
                        .checked_sub(REDRAW_ABS_CAP)
                        .unwrap_or_else(Instant::now),
                );
            }
        } else if tab.layout == uniform {
            return; // 已是均分且非最大化：无变化不写盘
        }
        tab.layout = uniform;
        self.window.request_redraw();
        // 复位后写盘（P15；与拖动结束/双击复位同语义）。
        self.persist_sessions();
    }

    /// 循环切换激活 tab：dir 为 1（下一个）或 -1（上一个）。
    fn cycle_tab(&mut self, dir: isize) {
        let n = self.tabs.len() as isize;
        if n <= 1 {
            return;
        }
        let idx = (self.active_tab as isize + dir).rem_euclid(n) as usize;
        self.activate(idx);
    }

    /// 窗口标题跟随当前模式的激活会话。本地模式与 tab 的
    /// `display_title` 同源；SSH 模式与会话栏同源（自定义名优先，
    /// 否则当前 Linux cwd 尾目录名）。
    fn update_window_title(&self) {
        if self.app_lock.is_locked() {
            self.window
                .set_title(&format!("Lumen — {}", i18n::strings().lock_screen_title));
            return;
        }
        if self.settings.layout.view_mode.is_ssh() {
            let title = self
                .ssh_runtime
                .session_views()
                .into_iter()
                .find(|session| session.active)
                .map_or_else(
                    || i18n::strings().topbar_tab_ssh.to_owned(),
                    |session| session.display_name,
                );
            self.window.set_title(&format!("Lumen [ime-r4] — {title}"));
            return;
        }
        let title = self.tabs[self.active_tab].display_title();
        // [BUILD-MARKER r4]（composer-IME 取证临时）：标题栏带版本标记，海风哥
        // 一眼确认跑的是不是带修复的新版，不用翻日志。坐实后连同诊断一并移除。
        self.window.set_title(&format!("Lumen [ime-r4] — {title}"));
    }

    /// 识别每个窗格的前台 LLM CLI。只识别程序类型，不判断回答/空闲状态。
    fn probe_llm_clis(&mut self, now: Instant) {
        if self
            .last_llm_cli_probe
            .is_some_and(|last| now.duration_since(last) < LLM_CLI_PROBE_INTERVAL)
        {
            return;
        }
        self.last_llm_cli_probe = Some(now);
        let mut cleared_slash_probe = false;
        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                let shell_pid = pane.pty.shell_pid();
                let foreground_pid = shell_pid.map(proc_icon::foreground_pid);
                let exe = shell_pid.and_then(proc_icon::foreground_exe);
                let detected = llm_cli::detect(exe.as_deref(), &pane.term);
                if detected != pane.llm_cli {
                    pane.llm_started_at = detected.map(|_| now);
                }
                pane.llm_cli = detected;
                pane.llm_foreground_pid = detected.and(foreground_pid);
                if pane.llm_cli.is_none() && !pane.slash_probe.shadow.is_empty() {
                    // CLI 已退出时清掉尚未提交的探测输入，避免候选串到 shell。
                    let _ = pane.write_user_input(b"\x15");
                    pane.slash_probe.clear();
                    cleared_slash_probe = true;
                }
            }
        }
        #[cfg(feature = "input-editor")]
        if cleared_slash_probe {
            self.close_passive_completion();
        }
    }

    /// F7② 节流轮询各 tab 焦点窗格的前台运行程序 exe（进程快照较重，
    /// `ICON_PROBE_INTERVAL` 限频）。结果写入 `session_icon_exe`，纹理在
    /// [`Self::ensure_session_icon_textures`] 按需懒加载。侧栏隐藏时跳过。
    fn probe_session_icons(&mut self, now: Instant) {
        if !self.settings.layout.sidebar_visible {
            return;
        }
        if self
            .last_icon_probe
            .is_some_and(|t| now.duration_since(t) < ICON_PROBE_INTERVAL)
        {
            return;
        }
        self.last_icon_probe = Some(now);
        // 先收集（不可变借 tabs），再写缓存（可变借自身），避免借用冲突。
        let probed: Vec<(TabId, Option<std::path::PathBuf>)> = self
            .tabs
            .iter()
            .map(|t| {
                let exe = t
                    .focused_pane()
                    .pty
                    .shell_pid()
                    .and_then(proc_icon::foreground_exe);
                (t.id, exe)
            })
            .collect();
        for (id, exe) in probed {
            self.session_icon_exe.insert(id, exe);
        }
        // 清理已关闭 tab 的条目（exe 缓存按路径保活，无需随 tab 清）。
        let live: std::collections::HashSet<TabId> = self.tabs.iter().map(|t| t.id).collect();
        self.session_icon_exe.retain(|k, _| live.contains(k));
    }

    /// F7② 把 `session_icon_exe` 里出现的 exe 图标懒加载为 egui 纹理。首抽后隔
    /// [`ICON_REFRESH_DELAY`] 重抽一次覆盖（治首抽踩到进程刚起时的系统占位图标、
    /// 被永久冻结不自愈）；抽取失败缓存 `tex: None`、回退自绘字形。
    fn ensure_session_icon_textures(&mut self, now: Instant) {
        // 侧栏隐藏时不显示 tab 图标、无需建本地纹理（被控端侧栏常隐藏——它靠
        // session_icon_rgba 位图上线给控制端，不依赖此本地纹理，避免白建永不显示
        // 的 GPU 纹理）。与 probe_session_icons 的 gate 一致。
        if !self.settings.layout.sidebar_visible {
            return;
        }
        // 需抽取：① 未缓存（首抽）② 已缓存但未重抽且过了延迟窗口（进程此时已
        //   稳定，重抽覆盖首抽可能踩到的占位图标）。
        let needed: Vec<std::path::PathBuf> = self
            .session_icon_exe
            .values()
            .flatten()
            .filter(|p| match self.session_icon_tex.get(*p) {
                None => true,
                Some(e) => !e.refreshed && now.duration_since(e.born) >= ICON_REFRESH_DELAY,
            })
            .cloned()
            .collect();
        for path in needed {
            let tex = self.load_session_icon_texture(&path);
            // 已存在=这是延迟重抽（定型 refreshed）；否则首抽（留一次重抽机会）。
            let refreshed = self.session_icon_tex.contains_key(&path);
            self.session_icon_tex.insert(
                path,
                SessionIcon {
                    tex,
                    born: now,
                    refreshed,
                },
            );
        }
    }

    /// F7② 抽取单个 exe 的关联图标并上传为 egui 纹理（失败 None）。
    fn load_session_icon_texture(&self, path: &std::path::Path) -> Option<egui::TextureHandle> {
        proc_icon::load_icon_rgba(path).map(|ic| {
            let img = egui::ColorImage::from_rgba_unmultiplied(
                [ic.width as usize, ic.height as usize],
                &ic.rgba,
            );
            self.egui_ctx.load_texture(
                format!("sess-icon:{}", path.display()),
                img,
                egui::TextureOptions::LINEAR,
            )
        })
    }

    /// F7② 取某 tab 当前应显示的会话图标纹理（前台程序 exe 图标）；
    /// 查不到 / 抽取失败时 None（上层回退自绘终端字形）。
    fn session_icon_for(&self, tab_id: TabId) -> Option<egui::TextureId> {
        self.session_icon_exe
            .get(&tab_id)
            .and_then(|o| o.as_ref())
            .and_then(|p| self.session_icon_tex.get(p))
            .and_then(|e| e.tex.as_ref())
            .map(egui::TextureHandle::id)
    }

    /// F7②-remote 被控端：为上线刷新前台 exe（不受本地侧栏 gate——被控端侧栏常
    /// 隐藏）+ 抽会话图标位图缓存（`session_icon_rgba`）。probe 进程快照重、按
    /// [`ICON_PROBE_INTERVAL`] 节流；位图只对新出现的 exe 抽（稳定后无操作）。
    fn refresh_remote_tab_icons(&mut self) {
        let now = Instant::now();
        // 到节流点才 probe 前台 exe（复用 last_icon_probe；被控端侧栏隐藏时本地
        // probe_session_icons 直接 return，不与此争节流）。
        if self
            .last_icon_probe
            .is_none_or(|t| now.duration_since(t) >= ICON_PROBE_INTERVAL)
        {
            self.last_icon_probe = Some(now);
            let probed: Vec<(TabId, Option<std::path::PathBuf>)> = self
                .tabs
                .iter()
                .map(|t| {
                    let exe = t
                        .focused_pane()
                        .pty
                        .shell_pid()
                        .and_then(proc_icon::foreground_exe);
                    (t.id, exe)
                })
                .collect();
            for (id, exe) in probed {
                self.session_icon_exe.insert(id, exe);
            }
            let live: std::collections::HashSet<TabId> = self.tabs.iter().map(|t| t.id).collect();
            self.session_icon_exe.retain(|k, _| live.contains(k));
        }
        // 抽新出现 exe 的图标位图（top-down RGBA8 上线）；已缓存的跳过（廉价）。
        let needed: Vec<std::path::PathBuf> = self
            .session_icon_exe
            .values()
            .flatten()
            .filter(|p| !self.session_icon_rgba.contains_key(*p))
            .cloned()
            .collect();
        for path in needed {
            let bm = proc_icon::load_icon_rgba(&path).map(|ic| {
                std::sync::Arc::new(lumen_protocol::remote::IconBitmap {
                    w: ic.width as u16,
                    h: ic.height as u16,
                    rgba: ic.rgba,
                })
            });
            self.session_icon_rgba.insert(path, bm);
        }
    }

    /// F7②-remote 被控端：取某 tab 焦点前台程序图标位图（上线用），无则 None。
    fn remote_tab_icon(&self, tab_id: TabId) -> Option<lumen_protocol::remote::IconBitmap> {
        self.session_icon_exe
            .get(&tab_id)
            .and_then(|o| o.as_ref())
            .and_then(|p| self.session_icon_rgba.get(p))
            .and_then(|o| o.as_ref())
            .map(|arc| (**arc).clone())
    }

    /// F7②-remote 控制端：把远程 tab 的图标位图 ensure 成本地 egui 纹理（内容
    /// 寻址、同图标多 tab 共享），并清掉不再被引用的纹理（防累积泄漏）。
    fn ensure_remote_icon_textures(&mut self) {
        // 先收集（结束对 remote_ws 的借用），再建纹理（借 egui_ctx + 写缓存）。
        let icons: Vec<(u64, lumen_protocol::remote::IconBitmap)> = self
            .remote_ws
            .remote_tabs()
            .iter()
            .filter_map(|t| t.icon.as_ref().map(|bm| (remote_icon_hash(bm), bm.clone())))
            .collect();
        let live: std::collections::HashSet<u64> = icons.iter().map(|(k, _)| *k).collect();
        for (key, bm) in icons {
            if self.remote_icon_tex.contains_key(&key) {
                continue;
            }
            // 尺寸防御（对端字节不可信）：① 长度自洽挡 from_rgba_unmultiplied 的
            //   length-mismatch panic；② w/h 上界挡畸形超大尺寸（如 w=65535 长度仍
            //   自洽、却超 GPU max_texture_dimension → wgpu 上传报错/掉设备），对齐
            //   本地 proc_icon 的 512 上界。畸形则跳过（不建纹理 → 取纹理落 None →
            //   回退自绘字形，不 panic）。
            let (w, h) = (bm.w as usize, bm.h as usize);
            if w == 0 || h == 0 || w > 512 || h > 512 || bm.rgba.len() != w * h * 4 {
                continue;
            }
            let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &bm.rgba);
            let tex = self.egui_ctx.load_texture(
                format!("remote-icon:{key:016x}"),
                img,
                egui::TextureOptions::LINEAR,
            );
            self.remote_icon_tex.insert(key, tex);
        }
        self.remote_icon_tex.retain(|k, _| live.contains(k));
    }

    /// 构造当前 tab 列表的持久化快照（F4/F5 嵌套结构：每 tab 的
    /// 自定义名 + 各窗格 cwd + 焦点下标）。窗格 cwd 取 OSC 9;9 上报
    /// 值，尚未上报（恢复后首个提示符还没到）时回退该窗格启动时的
    /// 初始目录——防止恢复后立即触发的写盘把保存的 cwd 冲成 None。
    fn sessions_snapshot(&self) -> sessions_store::SessionsFile {
        sessions_store::SessionsFile::new(
            self.tabs
                .iter()
                .map(|t| sessions_store::TabEntry {
                    custom_title: t.custom_title.clone(),
                    panes: t
                        .panes
                        .iter()
                        .map(|p| sessions_store::PaneEntry {
                            cwd: p
                                .term
                                .cwd()
                                .map(std::path::Path::to_path_buf)
                                .or_else(|| p.initial_cwd.clone()),
                            // 窗格自定义名（需求2）：持久化镜像运行时 Session。
                            custom_title: p.custom_title.clone(),
                        })
                        .collect(),
                    focused: t.focused,
                    // 布局比例（F7③）：构造路径保证归一化权重，写盘
                    // 原值（拖动结束/双击复位时由调用时机触发落盘）。
                    row_weights: t.layout.row_weights().to_vec(),
                    col_weights: t.layout.col_weights().to_vec(),
                    // 最大化态（P14）：toggle 即落盘，重启保持。
                    maximized: t.maximized,
                })
                .collect(),
            self.active_tab,
        )
    }

    /// 会话列表持久化（F4）：结构性变更（新建/关闭/重命名/切换激活）
    /// 与 cwd 上报变化时调用；快照与上次写盘一致则跳过，实际写频
    /// ≈ 用户开关 tab / cd 的频率。失败只记日志（save 内部），不
    /// 打扰终端使用。
    fn persist_sessions(&mut self) {
        let snap = self.sessions_snapshot();
        if self.last_sessions_snapshot.as_ref() == Some(&snap) {
            return;
        }
        log::debug!(
            "会话快照落盘：{} 个 tab，权重 {:?}",
            snap.tabs.len(),
            snap.tabs
                .iter()
                .map(|t| (&t.row_weights, &t.col_weights))
                .collect::<Vec<_>>()
        );
        snap.save();
        self.last_sessions_snapshot = Some(snap);
    }

    /// 粘贴编排（右键「粘贴到此目录」与 Ctrl+V 共用）：按目标侧定方向——
    /// - 本地目录：Lumen 内部有远程项 → 下载；否则系统剪贴板文件 → 本机复制。
    /// - 远程目录（上传）：系统剪贴板本地文件 → 上传到被控端。
    fn do_file_paste(&mut self, target_side: remote_ws::ClipSide, dir: String) {
        let remote_items = self
            .remote_ws
            .file_clipboard()
            .filter(|c| matches!(c.side, remote_ws::ClipSide::Remote))
            .map(|c| c.items.clone());
        log::info!(
            "[文件剪贴板] 粘贴请求: target={target_side:?} dir={dir} 远程剪贴板项={} 系统剪贴板有文件={}",
            remote_items.as_ref().map_or(0, Vec::len),
            clipboard_files::has_files(),
        );
        match target_side {
            remote_ws::ClipSide::Local => {
                if let Some(items) = remote_items {
                    // 下载：远程剪贴板 → 本地目录（撞名弹覆盖模态）。
                    self.paste_refresh = Some((false, dir.clone()));
                    let dest_root = std::path::Path::new(&dir);
                    let conflicts = items
                        .iter()
                        .filter(|it| dest_root.join(&it.name).exists())
                        .count();
                    if conflicts > 0 {
                        self.pending_paste = Some(PendingPaste {
                            items,
                            dest_dir: dir,
                            conflict_count: conflicts,
                            local: false,
                        });
                    } else {
                        self.remote_ws.start_download(items, dir, true);
                    }
                } else if let Some(item) = self.ssh_file_clipboard.clone() {
                    let destination =
                        std::path::Path::new(&dir).join(safe_staging_file_name(&item.name));
                    if destination.exists() {
                        self.pending_ssh_download = Some(PendingSshDownload { item, destination });
                    } else {
                        self.start_ssh_download_to_local(item, destination, false);
                    }
                } else {
                    // 本机复制：系统剪贴板文件 → 本地目录。
                    let paths = clipboard_files::paste_files();
                    if paths.is_empty() {
                        log::info!("[本机复制] 系统剪贴板无文件，忽略粘贴");
                    } else {
                        self.paste_refresh = Some((false, dir.clone()));
                        let items = paths_to_clipitems(&paths);
                        self.paste_local_files(items, dir);
                    }
                }
            }
            remote_ws::ClipSide::Remote => {
                // 上传：系统剪贴板本地文件 → 被控端目录（递归编排，撞名由被控端 Probe 决议）。
                let paths = clipboard_files::paste_files();
                if paths.is_empty() {
                    log::info!("[上传] 系统剪贴板无文件，忽略粘贴");
                } else {
                    self.paste_refresh = Some((true, dir.clone()));
                    let items = paths_to_clipitems(&paths);
                    self.remote_ws.start_upload(items, dir);
                }
            }
        }
    }

    fn start_ssh_download_to_local(
        &mut self,
        item: SshClipboardItem,
        destination: std::path::PathBuf,
        overwrite: bool,
    ) {
        match self
            .ssh_runtime
            .download_file(item.session_id, item.path, destination, overwrite)
        {
            Ok(()) => self.shell_state.toast.push(
                shell::toast::ToastKind::Info,
                i18n::strings().remote_download_started,
            ),
            Err(error) => self
                .shell_state
                .toast
                .push(shell::toast::ToastKind::Error, error),
        }
        self.window.request_redraw();
    }

    fn paste_into_ssh(
        &mut self,
        target_session_id: ssh_runtime::SshSessionId,
        target_directory: String,
    ) {
        let system_has_files = clipboard_files::has_files();
        let local_paths = clipboard_files::paste_files();
        let system_paths_are_current_ssh_export = self.ssh_file_clipboard.is_some()
            && self
                .ssh_clipboard_ready_path
                .as_ref()
                .is_some_and(|ready| local_paths.len() == 1 && local_paths.first() == Some(ready));
        if !local_paths.is_empty() && !system_paths_are_current_ssh_export {
            self.ssh_file_clipboard = None;
            for path in local_paths {
                if let Err(error) = self.ssh_runtime.upload_file(
                    target_session_id,
                    path,
                    target_directory.clone(),
                    false,
                ) {
                    self.shell_state
                        .toast
                        .push(shell::toast::ToastKind::Error, error);
                }
            }
            self.window.request_redraw();
            return;
        }
        if system_has_files && local_paths.is_empty() {
            self.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                i18n::strings().file_clipboard_read_failed,
            );
            self.window.request_redraw();
            return;
        }

        let Some(item) = self.ssh_file_clipboard.clone() else {
            return;
        };
        let staging_root = std::env::temp_dir().join("lumen_ssh_paste");
        if let Err(error) = std::fs::create_dir_all(&staging_root) {
            self.shell_state.toast.push(
                shell::toast::ToastKind::Error,
                format!("无法创建 SSH 粘贴暂存目录：{error}"),
            );
            return;
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let staging_path = staging_root.join(format!(
            "{nonce}-{}-{}",
            item.size,
            safe_staging_file_name(&item.name)
        ));
        self.ssh_paste_chains.insert(
            staging_path.clone(),
            SshPasteChain {
                target_session_id,
                target_directory,
                destination_name: item.name.clone(),
            },
        );
        if let Err(error) =
            self.ssh_runtime
                .download_file(item.session_id, item.path, staging_path.clone(), false)
        {
            self.ssh_paste_chains.remove(&staging_path);
            self.shell_state
                .toast
                .push(shell::toast::ToastKind::Error, error);
        }
        self.window.request_redraw();
    }

    /// 粘贴传输完成后刷新目标目录（消费 `paste_refresh`）：本地 → 刷新本地树该目录；远程 → 刷新
    /// 远程树该目录。作废其缓存重拉，使刚粘贴进来的文件立即显示。
    fn apply_paste_refresh(&mut self) {
        if let Some((is_remote, dir)) = self.paste_refresh.take() {
            if is_remote {
                self.remote_ws.refresh_remote_path(&dir);
            } else {
                self.shell_state
                    .filetree
                    .refresh_dir(std::path::Path::new(&dir));
            }
            self.window.request_redraw();
        }
    }

    /// Ctrl+C 触发的文件树复制（winit 层拦截，与 Ctrl+V 统一门控）。远程视图 → 复制选中远程项到
    /// Lumen 内部剪贴板（下载源）；本地视图 → 复制选中本地项到系统剪贴板（CF_HDROP，与资源管理器
    /// 互通）。无选中则忽略。
    fn filetree_ctrl_c(&mut self) {
        if self.settings.layout.view_mode.is_ssh() {
            let Some((session_id, path, name, _is_directory, size)) =
                self.ssh_runtime.active_selected_file()
            else {
                return;
            };
            self.copy_ssh_file_to_clipboards(SshClipboardItem {
                session_id,
                path,
                name,
                size,
            });
        } else if self.settings.layout.view_mode.is_remote() {
            self.ssh_file_clipboard = None;
            // 远程视图：复制选中的被控端项 → Lumen 内部剪贴板（远程路径进不了系统剪贴板，仅供下载）。
            if let Some((path, name, is_dir, size)) = self
                .remote_ws
                .remote_filetree()
                .and_then(crate::remote_ws::RemoteFileTree::selected_item)
            {
                // 片6/8：单文件 → 即时系统剪贴板虚拟文件；目录 → 先 clear 关竞态、起递归枚举，
                // 枚举完成（ClipDirReady）才 set 多文件 descriptor。
                if let Some(svc) = self.clipboard_svc.as_ref() {
                    svc.clear();
                }
                if is_dir {
                    self.remote_ws.start_clip_dir(path.clone(), name.clone());
                } else {
                    self.remote_ws.cancel_clip_dir(); // 作废可能在途的目录枚举（M1）
                    if let Some(svc) = self.clipboard_svc.as_ref() {
                        svc.set_remote_file(path.clone(), name.clone(), size);
                    }
                }
                self.remote_ws.set_file_clipboard(
                    remote_ws::ClipSide::Remote,
                    vec![remote_ws::ClipItem { path, name, is_dir }],
                );
                let msg = if is_dir {
                    i18n::strings().remote_clip_dir_preparing.to_string()
                } else {
                    i18n::fmt1(i18n::strings().remote_copied_fmt, 1)
                };
                self.shell_state
                    .toast
                    .push(shell::toast::ToastKind::Info, msg);
            }
        } else if self.settings.layout.view_mode.is_local() {
            self.ssh_file_clipboard = None;
            let Some((path, _is_dir)) = self.shell_state.filetree.selected_item() else {
                return;
            };
            // 本地视图：复制选中项 → 系统剪贴板（CF_HDROP），清 Lumen 内部远程剪贴板。
            let ok = clipboard_files::copy_files(&[path]);
            self.remote_ws.clear_file_clipboard();
            self.remote_ws.cancel_clip_dir(); // 作废可能在途的远程目录枚举（M1）
                                              // 片6：清掉系统剪贴板可能残留的我方远程虚拟文件（本地复制改走 CF_HDROP）。
            if let Some(svc) = self.clipboard_svc.as_ref() {
                svc.clear();
            }
            self.shell_state.toast.push(
                if ok {
                    shell::toast::ToastKind::Info
                } else {
                    shell::toast::ToastKind::Error
                },
                if ok {
                    i18n::fmt1(i18n::strings().remote_copied_fmt, 1)
                } else {
                    i18n::strings().local_copy_clipboard_failed.to_string()
                },
            );
        }
    }

    /// Ctrl+V 触发的文件树粘贴（winit 层拦截，因 egui 把 Ctrl+V 吞成 Paste/consume V，文件剪贴板
    /// 无文本时连信号都没）。按当前视图定目标目录：远程视图 → 选中目录 / 树根（上传）；本地视图 →
    /// 选中目录 / 树根（下载或本机复制）。无目标目录则忽略。
    fn filetree_ctrl_v(&mut self) {
        if self.settings.layout.view_mode.is_ssh() {
            if let Some((session_id, directory)) = self.ssh_runtime.active_paste_target() {
                self.paste_into_ssh(session_id, directory);
            }
        } else if self.settings.layout.view_mode.is_remote() {
            // 远程视图：上传到选中的远程目录（或树根）。
            let dir = self
                .remote_ws
                .remote_filetree()
                .and_then(remote_ws::RemoteFileTree::paste_target_dir);
            if let Some(dir) = dir {
                self.do_file_paste(remote_ws::ClipSide::Remote, dir);
            }
        } else if self.settings.layout.view_mode.is_local() {
            let Some(dir) = self.shell_state.filetree.paste_target_dir() else {
                return;
            };
            // 本地视图：下载 / 本机复制到选中目录（或树根）。
            self.do_file_paste(remote_ws::ClipSide::Local, dir.display().to_string());
        }
    }

    /// 本机复制粘贴落地（系统剪贴板文件 → 本地目录）：同目录粘贴自动副本 → 撞名探测 → 撞名弹
    /// 覆盖模态 / 否则直接 [`Self::start_local_copy`]。粘贴到本地目录且无远程剪贴板项时调用。
    fn paste_local_files(&mut self, items: Vec<remote_ws::ClipItem>, dir: String) {
        if self.local_copy_rx.is_some() {
            log::info!("[本机复制] 已有复制在途，忽略本次粘贴");
            self.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                i18n::strings().local_copy_busy.to_string(),
            );
            return;
        }
        let dest_root = std::path::Path::new(&dir);
        // 同目录粘贴（源父目录 == 目标目录）→ 自动副本名，避免落地名 = 源自身被撞名跳过
        // （文件管理器标准：粘贴到原地产生「X (1)」）。
        let items: Vec<remote_ws::ClipItem> = items
            .into_iter()
            .map(|mut it| {
                if std::path::Path::new(&it.path).parent() == Some(dest_root) {
                    let nn = unique_copy_name(dest_root, &it.name, it.is_dir);
                    log::info!("[本机复制] 同目录粘贴 → 副本名 {} ⇒ {nn}", it.name);
                    it.name = nn;
                }
                it
            })
            .collect();
        let conflicts = items
            .iter()
            .filter(|it| dest_root.join(&it.name).exists())
            .count();
        log::info!(
            "[本机复制] 撞名数={conflicts} 待落地=[{}]",
            items
                .iter()
                .map(|i| dest_root.join(&i.name).display().to_string())
                .collect::<Vec<_>>()
                .join("; ")
        );
        if conflicts > 0 {
            self.pending_paste = Some(PendingPaste {
                items,
                dest_dir: dir,
                conflict_count: conflicts,
                local: true,
            });
        } else {
            self.start_local_copy(items, dir, true);
        }
    }

    /// 本机复制粘贴（local→local，海风哥本轮新增）：后台线程把剪贴板里的本地项递归 fs 复制到
    /// `dest_dir`，完成回主线程弹 toast。`overwrite`：撞名覆盖（否则跳过已存在），由覆盖模态
    /// 一次性决定、套用整次递归。盘 IO 绝不在 UI 线程做（大目录 / 网络盘会冻结）；完成经 mpsc
    /// 回包 + PtyWake 唤醒主循环（[`Self::local_copy_rx`] drain 处弹 toast）。已有本机复制在途
    /// 则忽略（`local_copy_rx` 充当并发闸，防回包错配）。
    fn start_local_copy(
        &mut self,
        items: Vec<remote_ws::ClipItem>,
        dest_dir: String,
        overwrite: bool,
    ) {
        if items.is_empty() || self.local_copy_rx.is_some() {
            log::info!(
                "[本机复制] 跳过: empty={} busy={}",
                items.is_empty(),
                self.local_copy_rx.is_some()
            );
            return;
        }
        log::info!(
            "[本机复制] 开始: {} 项 → {dest_dir}（overwrite={overwrite}）",
            items.len()
        );
        let (tx, rx) = std::sync::mpsc::channel();
        self.local_copy_rx = Some(rx);
        self.shell_state.toast.push(
            shell::toast::ToastKind::Info,
            i18n::strings().local_copy_started.to_string(),
        );
        let proxy = self.proxy.clone();
        let wake = self.wake_pending.clone();
        std::thread::spawn(move || {
            // catch_unwind 保证：即便 local_copy_item 意外 panic（如 OOM），也必发回包解开
            // local_copy_rx 并发闸——否则 rx 永停在 Some、整个会话内再不能发起本机复制。
            let stats = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut stats = CopyStats::default();
                let dest_root = std::path::PathBuf::from(&dest_dir);
                for item in &items {
                    local_copy_item(
                        &dest_root.join(&item.name),
                        std::path::Path::new(&item.path),
                        item.is_dir,
                        overwrite,
                        0,
                        &mut stats,
                    );
                }
                stats
            }))
            .unwrap_or_else(|_| {
                log::error!("本机复制后台线程 panic，回错误计数以解开并发闸");
                CopyStats {
                    done: 0,
                    skipped: 0,
                    errors: 1,
                }
            });
            let _ = tx.send((stats.done, stats.skipped, stats.errors));
            // 唤醒主循环收包弹 toast（与 PTY 转发同一套 wake 去重，避免空闲不重绘收不到）。
            if !wake.swap(true, Ordering::AcqRel) {
                let _ = proxy.send_event(PtyWake);
            }
        });
    }
}

/// 远程控制一次性通知 → toast（分级 + 本地化文案，按机器可读原因细分）。M5.3 part2b。
/// part3c-2 #7：粘贴检测到同名后、等覆盖模态拍板的待决下载。
struct PendingPaste {
    /// 要落地的项（剪贴板快照；下载=远程项 / 本机复制=本地项）。
    items: Vec<remote_ws::ClipItem>,
    /// 本地落地目录。
    dest_dir: String,
    /// 冲突项数（驱动模态文案）。
    conflict_count: usize,
    /// 本轮粘贴方向：`true` = 本机复制（local→local，fs 递归）；`false` = 下载（远程→本地，
    /// WS Fetch）。覆盖模态拍板后据此路由 `start_local_copy` / `start_download`。
    local: bool,
}

#[derive(Clone)]
struct SshClipboardItem {
    session_id: ssh_runtime::SshSessionId,
    path: String,
    name: String,
    size: u64,
}

struct PendingSshDownload {
    item: SshClipboardItem,
    destination: std::path::PathBuf,
}

struct SshPasteChain {
    target_session_id: ssh_runtime::SshSessionId,
    target_directory: String,
    destination_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SshClipboardExport {
    generation: u64,
    session_id: ssh_runtime::SshSessionId,
    /// 开始准备时的 Windows 剪贴板代次；用户随后在资源管理器复制了
    /// 其他内容时，完成回包不得覆盖用户更新后的剪贴板。
    clipboard_sequence: Option<u32>,
}

/// 把系统剪贴板取出的本地路径转成 `ClipItem`（统一交给本机复制 / 上传编排）。`is_dir` 现读盘
/// （系统剪贴板只给路径），失败按文件处理。
fn paths_to_clipitems(paths: &[std::path::PathBuf]) -> Vec<remote_ws::ClipItem> {
    paths
        .iter()
        .map(|p| {
            let name = p.file_name().map_or_else(
                || p.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            remote_ws::ClipItem {
                path: p.display().to_string(),
                name,
                is_dir: p.is_dir(),
            }
        })
        .collect()
}

fn safe_staging_file_name(name: &str) -> String {
    let mut result = name
        .chars()
        .take(180)
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    while result.ends_with(' ') || result.ends_with('.') {
        result.pop();
    }
    if result.is_empty() || result == "." || result == ".." {
        "remote-file".to_owned()
    } else {
        result
    }
}

fn next_ssh_clipboard_generation(current: u64) -> u64 {
    let next = current.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}

fn ssh_clipboard_export_is_current(export_generation: u64, current_generation: u64) -> bool {
    export_generation != 0 && export_generation == current_generation
}

fn clipboard_changed_since(started: Option<u32>, current: Option<u32>) -> bool {
    started != current
}

#[cfg(windows)]
fn system_clipboard_sequence_number() -> Option<u32> {
    // SAFETY: 只读取系统维护的全局剪贴板序号，不持有句柄或指针。
    Some(unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() })
}

#[cfg(not(windows))]
fn system_clipboard_sequence_number() -> Option<u32> {
    None
}

fn ssh_clipboard_staging_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("lumen_ssh_clipboard-{}", std::process::id()))
}

fn ssh_clipboard_batch_directory(
    root: &std::path::Path,
    generation: u64,
    session_id: ssh_runtime::SshSessionId,
    nonce: u128,
) -> std::path::PathBuf {
    root.join(format!("{generation}-{session_id}-{nonce}"))
}

fn create_ssh_clipboard_staging_path(
    generation: u64,
    item: &SshClipboardItem,
) -> std::io::Result<std::path::PathBuf> {
    let root = ssh_clipboard_staging_root();
    std::fs::create_dir_all(&root)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let batch = ssh_clipboard_batch_directory(&root, generation, item.session_id, nonce);
    std::fs::create_dir(&batch)?;
    Ok(batch.join(safe_staging_file_name(&item.name)))
}

/// 只删除本进程创建的 `%TEMP%\lumen_ssh_clipboard-<pid>\<batch>`。
///
/// 下载目标可能是目录，也可能在失败时残留 SFTP 临时文件，因此以已验证的
/// 唯一 batch 目录为最小清理单元；任何不符合两级布局的路径一律拒绝删除。
fn remove_ssh_clipboard_staging_path(path: &std::path::Path) {
    let root = ssh_clipboard_staging_root();
    let Some(batch) = path.parent() else {
        return;
    };
    if !root.is_absolute() || batch.parent() != Some(root.as_path()) {
        log::warn!(
            "拒绝清理不属于 SSH 文件剪贴板暂存区的路径：{}",
            path.display()
        );
        return;
    }
    let _ = std::fs::remove_dir_all(batch);
}

fn remove_staged_path(path: &std::path::Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// 在 `dir` 下为 `name` 找一个不冲突的副本名（文件管理器式「X (1)」「X (2)」…）。同目录粘贴
/// （粘贴到源文件自己的目录）时用——此时落地名若不变就会撞到源自身，故自动改名产生副本。
fn unique_copy_name(dir: &std::path::Path, name: &str, is_dir: bool) -> String {
    if !dir.join(name).exists() {
        return name.to_string();
    }
    // 文件保留扩展名（`a.txt` → `a (1).txt`）；目录或无扩展名整体加后缀。
    let (stem, ext) = if is_dir {
        (name.to_string(), String::new())
    } else {
        match name.rfind('.') {
            Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
            _ => (name.to_string(), String::new()),
        }
    };
    for n in 1..10_000 {
        let candidate = format!("{stem} ({n}){ext}");
        if !dir.join(&candidate).exists() {
            return candidate;
        }
    }
    name.to_string() // 兜底：上万个同名副本，几乎不可能。
}

/// 本机复制粘贴（local→local）的统计计数：完成 / 跳过（撞名或复制到原地）/ 出错文件数。
#[derive(Default)]
struct CopyStats {
    done: usize,
    skipped: usize,
    errors: usize,
}

/// 本机递归复制一项（[`AppState::start_local_copy`] 后台线程的工作函数，纯 std::fs）：
///
/// - 文件：撞名按 `overwrite` 覆盖 / 跳过，`fs::copy` 落地（源与目标同一文件则跳过，防 truncate 毁源）。
/// - 目录：建目标目录后递归子项。
///
/// 防御：深度上限 64（防 symlink / junction 成环）；拒绝把目录复制进自身子树（否则无限递归）。
/// 统计累计进 `stats`，回主线程汇总 toast。
fn local_copy_item(
    dest: &std::path::Path,
    src: &std::path::Path,
    is_dir: bool,
    overwrite: bool,
    depth: usize,
    stats: &mut CopyStats,
) {
    const MAX_DEPTH: usize = 64;
    if depth > MAX_DEPTH {
        stats.errors += 1;
        return;
    }
    if is_dir {
        // 防御：目标落在源子树内（复制目录到自身 / 子目录）→ 无限递归，拒绝。dest 首次未必存在，
        // 故用「dest 父目录 canonical + 末段」与「src canonical」比较，不依赖 dest 自身可解析。
        if let Ok(src_canon) = src.canonicalize() {
            let dest_canon = dest
                .parent()
                .and_then(|p| p.canonicalize().ok())
                .and_then(|p| dest.file_name().map(|n| p.join(n)));
            if dest_canon.is_some_and(|d| d == src_canon || d.starts_with(&src_canon)) {
                log::warn!(
                    "本机复制：目标在源子树内，拒绝（防无限递归）: {}",
                    dest.display()
                );
                stats.errors += 1;
                return;
            }
        }
        if let Err(e) = std::fs::create_dir_all(dest) {
            log::warn!("本机复制建目录失败 {}: {e}", dest.display());
            stats.errors += 1;
            return;
        }
        let rd = match std::fs::read_dir(src) {
            Ok(rd) => rd,
            Err(e) => {
                log::warn!("本机复制读源目录失败 {}: {e}", src.display());
                stats.errors += 1;
                return;
            }
        };
        for entry in rd.flatten() {
            let child_src = entry.path();
            let child_dest = dest.join(entry.file_name());
            let child_is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            local_copy_item(
                &child_dest,
                &child_src,
                child_is_dir,
                overwrite,
                depth + 1,
                stats,
            );
        }
    } else {
        if !overwrite && dest.exists() {
            stats.skipped += 1;
            return;
        }
        // 防御：源与目标解析为同一文件 → fs::copy 会先 truncate 毁掉源，跳过（复制到原地无意义）。
        if let (Ok(s), Ok(d)) = (src.canonicalize(), dest.canonicalize()) {
            if s == d {
                stats.skipped += 1;
                return;
            }
        }
        match std::fs::copy(src, dest) {
            Ok(_) => stats.done += 1,
            Err(e) => {
                log::warn!(
                    "本机复制文件失败 {} → {}: {e}",
                    src.display(),
                    dest.display()
                );
                stats.errors += 1;
            }
        }
    }
}

/// part3d Phase 3c：据多窗格镜像 Terminal 取窗格标题（cwd 尾目录名 > OSC 标题 > 「窗格 N」，
/// 与本地窗格标题同源逻辑；镜像无 app 级 custom_title，故不取）。
fn mirror_pane_title(term: &lumen_term::Terminal, idx: usize) -> String {
    term.cwd()
        .map(|c| {
            c.file_name().map_or_else(
                || c.display().to_string(),
                |t| t.to_string_lossy().into_owned(),
            )
        })
        .or_else(|| {
            let t = term.title();
            (!t.is_empty()).then(|| t.to_owned())
        })
        .unwrap_or_else(|| i18n::fmt1(i18n::strings().pane_default_name_fmt, idx + 1))
}

fn remote_notice_toast(n: &remote_ws::Notice) -> (shell::toast::ToastKind, String) {
    use lumen_protocol::remote::{DenyReason, EndReason, FsErr, PairingFailReason, Role};
    use remote_ws::Notice;
    use shell::toast::ToastKind;
    let s = i18n::strings();
    match n {
        Notice::ControlDenied(reason) => {
            let text = match reason {
                DenyReason::Offline => s.remote_denied_offline,
                DenyReason::AlreadyControlled | DenyReason::TargetPairing => s.remote_denied_busy,
                DenyReason::RejectedByUser => s.remote_denied_rejected,
                _ => s.remote_denied_generic,
            };
            (ToastKind::Warn, text.to_string())
        }
        Notice::PairingCancelled(reason) => {
            let text = match reason {
                DenyReason::Expired => s.remote_toast_pairing_expired,
                _ => s.remote_toast_pairing_cancelled,
            };
            (ToastKind::Warn, text.to_string())
        }
        Notice::PairingFailed(reason) => {
            let text = match reason {
                PairingFailReason::Expired => s.remote_toast_pairing_expired,
                _ => s.remote_pairing_failed,
            };
            (ToastKind::Warn, text.to_string())
        }
        Notice::SessionStarted { role, peer } => {
            let tpl = match role {
                Role::Controller => s.remote_toast_controlling_fmt,
                Role::Controlled => s.remote_toast_controlled_fmt,
            };
            (ToastKind::Info, i18n::fmt1(tpl, peer))
        }
        Notice::SessionEnded(reason) => {
            let text = match reason {
                EndReason::PeerDisconnected => s.remote_toast_peer_offline,
                EndReason::Replaced => s.remote_toast_replaced,
                EndReason::PeerLeft => s.remote_toast_session_ended,
            };
            (ToastKind::Info, text.to_string())
        }
        Notice::FetchStarted => (ToastKind::Info, s.remote_fetch_started.to_string()),
        Notice::FetchFailed(err) => {
            let text = match err {
                FsErr::TooLarge => s.remote_fetch_too_large,
                _ => s.remote_fetch_failed,
            };
            (ToastKind::Warn, text.to_string())
        }
        Notice::DownloadStarted => (ToastKind::Info, s.remote_download_started.to_string()),
        Notice::DownloadDone {
            done,
            skipped,
            errors,
        } => {
            let kind = if *errors > 0 {
                ToastKind::Warn
            } else {
                ToastKind::Info
            };
            (
                kind,
                i18n::fmt3(s.remote_download_done_fmt, done, skipped, errors),
            )
        }
        Notice::UploadStarted => (ToastKind::Info, s.remote_upload_started.to_string()),
        Notice::UploadDone {
            done,
            skipped,
            errors,
        } => {
            let kind = if *errors > 0 {
                ToastKind::Warn
            } else {
                ToastKind::Info
            };
            (
                kind,
                i18n::fmt3(s.remote_upload_done_fmt, done, skipped, errors),
            )
        }
        Notice::ClipDirReady { count, truncated } => {
            if *truncated {
                (
                    ToastKind::Warn,
                    i18n::fmt1(s.remote_clip_dir_truncated_fmt, *count),
                )
            } else {
                (
                    ToastKind::Info,
                    i18n::fmt1(s.remote_clip_dir_ready_fmt, *count),
                )
            }
        }
        Notice::ClipDirFailed => (ToastKind::Warn, s.remote_clip_dir_failed.to_string()),
        // part3d Phase 2：远程增删会话失败提示（需求 d #11）。
        Notice::RemoteNewTabFailed(err) => {
            let text = match err {
                lumen_protocol::remote::RemoteOpErr::LimitReached => s.remote_session_limit,
                _ => s.remote_op_failed,
            };
            (ToastKind::Warn, text.to_string())
        }
        Notice::RemoteCloseTabFailed(err) => {
            let text = match err {
                // 被控端用 Io 表示「拒关最后一个会话」（见 main 关闭分支）。
                lumen_protocol::remote::RemoteOpErr::Io => s.remote_close_last,
                _ => s.remote_op_failed,
            };
            (ToastKind::Warn, text.to_string())
        }
        // M6 Phase 3：数据面直连/回退状态（无声降级禁令——切换对用户可见）。
        Notice::P2pDirect => (ToastKind::Info, s.remote_toast_p2p_direct.to_string()),
        Notice::P2pRelay => (ToastKind::Info, s.remote_toast_p2p_relay.to_string()),
        // 断线宽限重挂：中断提示（黄）+ 自动恢复成功（蓝）。
        Notice::SessionReconnecting => (ToastKind::Warn, s.remote_toast_reconnecting.to_string()),
        Notice::SessionRestored => (ToastKind::Info, s.remote_toast_restored.to_string()),
        // M7 片 4：对端不支持 LLM 面（Hello 5 秒无 HelloAck）。**Warn 而非 Info**——
        // 它是一次功能不可用，不是状态播报；但终端镜像与文件传输照常，文案里点明了。
        Notice::LlmPeerTooOld => (
            ToastKind::Warn,
            s.remote_toast_llm_peer_too_old.to_string(),
        ),
    }
}

/// 无边框窗口外缘 resize 命中检测（左/右/下及下方两角）：鼠标物理坐标
/// `mouse_pos` 落在客户区外缘约 6 逻辑像素带内时返回对应 [`ResizeDirection`]。
/// 无边框窗口客户区铺满整窗、系统不再对边缘做原生 NCHITTEST/边框 resize，
/// 故手动命中 + drag_resize_window 补回拖边 resize。顶边让位自绘标题栏拖动
/// 移动与右上角窗控按钮，不做 resize。最大化态返回 None。
///
/// 三平台通用：客户端各自绘标题栏 + 无边框（Windows WS_THICKFRAME 铺满、
/// Linux/macOS with_decorations(false)），边缘 resize 均由此逻辑接管，经
/// winit drag_resize_window（Windows WM_NCLBUTTONDOWN / X11 _NET_WM_MOVERESIZE /
/// Wayland xdg_toplevel::resize）启动系统 resize。
fn resize_edge_dir(
    window: &winit::window::Window,
    mouse_pos: (f64, f64),
    ppp: f32,
) -> Option<winit::window::ResizeDirection> {
    use winit::window::ResizeDirection;
    if window.is_maximized() {
        return None;
    }
    let size = window.inner_size();
    if size.width == 0 || size.height == 0 {
        return None;
    }
    let (w, h) = (size.width as f64, size.height as f64);
    let (mx, my) = mouse_pos;
    // 命中带：约 6 逻辑像素（随 DPI 缩放），最低 4 物理像素。
    let b = (6.0 * ppp as f64).max(4.0);
    let left = mx < b;
    let right = mx > w - b;
    let bottom = my > h - b;
    let dir = if bottom && left {
        ResizeDirection::SouthWest
    } else if bottom && right {
        ResizeDirection::SouthEast
    } else if left {
        ResizeDirection::West
    } else if right {
        ResizeDirection::East
    } else if bottom {
        ResizeDirection::South
    } else {
        return None;
    };
    Some(dir)
}

impl App {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<AppState> {
        // M3.8 自绘标题栏：无边框窗口 + DWM 阴影/Win11 圆角。
        // with_decorations(false) 在 Windows 上保留 WS_THICKFRAME（拖边 resize
        // 可用），WM_NCCALCSIZE 铺满客户区（无系统标题栏）。
        // with_undecorated_shadow(true) 启用 DWM 阴影并允许 Win11 圆角识别；
        // 副作用：顶部 1px 黑线（顶栏背景色覆盖消除）。
        // 非 Windows 平台降级保留系统装饰（with_decorations 有 #[cfg(windows)] 处理）。
        // 第二十二轮：运行时窗口图标（窗口左上角/Alt-Tab/任务栏运行态）。
        // with_window_icon 设 32px 图标（符合 Windows ICON 推荐小图标尺寸）；
        // with_taskbar_icon (Windows 专属扩展) 设 64px 大图标（任务栏/Alt-Tab 高 DPI）。
        // 解码失败降级为 None（warn 已在 load_icon 内打印）。
        let window_icon = load_icon(include_bytes!("../../../icons/lumen-icon-32.png"));
        #[cfg(target_os = "windows")]
        let taskbar_icon = load_icon(include_bytes!("../../../icons/lumen-icon-64.png"));
        #[cfg(target_os = "windows")]
        let attrs = {
            Window::default_attributes()
                .with_title("Lumen")
                .with_inner_size(winit::dpi::LogicalSize::new(1000.0, 640.0))
                .with_maximized(true)
                .with_decorations(false)
                .with_undecorated_shadow(true)
                // 隐藏创建：初始化全程不可见，避免 DWM 显示空白表面的白闪。
                // init 末尾铺一帧主题底色后再 set_visible(true)（同步、无条件，
                // 见 init 收尾），窗口一露面即深色 + 已最大化、无尺寸跳变。
                .with_visible(false)
                .with_window_icon(window_icon)
                .with_taskbar_icon(taskbar_icon)
        };
        // macOS：保留原生装饰（红黄绿交通灯）。自绘 × 在 mac 上点击命中不可靠（真机
        // 实测关不掉），且原生窗口管理更符合 mac 习惯；topbar 在 mac 上不再画自绘窗控
        // 按钮（避免与交通灯双套）。不强制最大化、不无边框——排除「无边框+maximized」
        // 在 Metal 上的 surface 尺寸/渲染怪象（残影排查）。
        #[cfg(target_os = "macos")]
        let attrs = Window::default_attributes()
            .with_title("Lumen")
            .with_inner_size(winit::dpi::LogicalSize::new(1000.0, 640.0))
            .with_visible(false)
            .with_window_icon(window_icon);
        // Linux/其它 unix：无边框 + 自绘顶栏作唯一标题栏（消除双标题栏，已真机验证）。
        // 移动走顶栏 drag_window、缩放走 resize_edge_dir + drag_resize_window（winit 跨平台）。
        #[cfg(all(unix, not(target_os = "macos")))]
        let attrs = Window::default_attributes()
            .with_title("Lumen")
            .with_inner_size(winit::dpi::LogicalSize::new(1000.0, 640.0))
            .with_maximized(true)
            .with_decorations(false)
            // 隐藏创建，init 末尾铺底色帧后再显示（消白闪，见 init 收尾）。
            .with_visible(false)
            .with_window_icon(window_icon);
        // 启动默认最大化（P17）：inner_size 保留为「取消最大化」后的还原尺寸。
        let window = Arc::new(event_loop.create_window(attrs).context("创建窗口失败")?);
        // workaround winit #4186：with_decorations(false) + with_resizable(true) 下
        // 拖边 resize 可能失效（WS_THICKFRAME 添加时序 bug，PR #4188 修复未合入 0.30.9）。
        // init 后显式调 set_resizable(true) 可触发 WS_THICKFRAME 重新施加，绕过该 bug。
        window.set_resizable(true);
        window.set_ime_allowed(true);
        // 告知输入法处于终端语境（egui-winit 内部有同等映射）。
        window.set_ime_purpose(winit::window::ImePurpose::Terminal);

        let size = window.inner_size();
        let scale = window.scale_factor() as f32;
        let mut renderer = Renderer::new(window.clone(), size.width, size.height, scale)
            .context("初始化渲染器失败")?;

        // —— 设置加载与应用（settings.json；缺失/损坏降级默认值）——
        let app_settings = settings::Settings::load();
        // M5.2：把持久化的服务端地址应用到 cloud 全局（非空才覆盖，空则回退
        // 环境变量/默认）。供登录/心跳/设备列表读取。
        if !app_settings.server_url.trim().is_empty() {
            cloud::set_server_url(&app_settings.server_url);
        }
        // F6 多语言：启动后立即将全局语言设为设置中存储的语言。
        i18n::set_language(app_settings.language);
        // 系统深浅模式（P12 Sync with OS）：winit 报不出来（None）按
        // 深色处理——默认主题即深色；后续变化经 ThemeChanged 事件维护。
        let os_dark = !matches!(window.theme(), Some(winit::window::Theme::Light));
        let ap = &app_settings.appearance;
        let actual_family = renderer.reconfigure_font(&ap.font_family, ap.font_size);
        let theme_info = settings::theme_info(app_settings.effective_theme_id(os_dark));
        renderer.set_theme(theme_info.theme());
        // 问题5：启动时同步 panel_outline 描边色到 renderer。
        {
            let pal = shell::theme::shell_palette(theme_info);
            let [r, g, b, _] = pal.panel_outline.to_array();
            renderer.set_footer_border_color(r, g, b);
        }
        info!(
            "设置加载：主题 {}（id {}，sync_with_os={}）字号 {} 侧栏宽 {}/{} 字体「{}」→ 实际生效「{actual_family}」",
            theme_info.name,
            theme_info.id,
            ap.sync_with_os,
            ap.font_size,
            app_settings.layout.sidebar_width,
            app_settings.layout.filetree_width,
            if ap.font_family.is_empty() {
                "自动"
            } else {
                &ap.font_family
            }
        );
        // 字体回退提示（设置页 Appearance 展示）。
        // F6：启动时语言已由第 1069 行 i18n::set_language 设置完毕，
        // 此处必须走 i18n 表而非硬编码简体中文。
        let font_hint = (!ap.font_family.is_empty()
            && !actual_family.eq_ignore_ascii_case(&ap.font_family))
        .then(|| {
            i18n::fmt2(
                i18n::strings().toast_font_fallback_fmt,
                &ap.font_family,
                &actual_family,
            )
        });
        // 首次启动（无设置文件）落盘默认值，方便用户直接手改；
        // 文件存在但损坏时不在此覆盖（保留现场，变更时才写）。
        // 此刻 UI 尚未建立，失败只记日志（save 内部已记）不弹 toast。
        if settings::Settings::path().is_some_and(|p| !p.exists()) {
            let _ = app_settings.save();
        }

        // —— 登录态加载（profile.json；缺失=未登录、损坏=未登录+警告）——
        let current_server_url = cloud::server_url();
        let mut user_profile = profile::Profile::load();
        // v1.0.15 及更早版本只有一个全局服务端，profile 尚无 auth_origin。
        // 升级时把旧登录态一次性绑定到当时已配置且通过传输校验的 origin；
        // 不能直接删除，否则远程心跳与控制 WS 都不会启动。
        let profile_origin_migrated = user_profile
            .as_mut()
            .is_some_and(|profile| profile.migrate_legacy_auth_origin(&current_server_url));
        // 已有来源与当前设置确实不一致时仍 fail-closed，避免把一个服务端
        // 签发的 bearer token 发送给另一个服务端。
        let profile_origin_reauth_required = user_profile
            .as_ref()
            .is_some_and(|profile| profile_origin_requires_reauth(profile, &current_server_url));
        if profile_origin_reauth_required {
            log::warn!("登录档案缺少有效服务端来源绑定，已安全退出并要求重新登录");
            profile::Profile::delete();
            user_profile = None;
        }
        match &user_profile {
            Some(p) => info!("登录态加载：{} <{}>", p.display_name, p.email),
            None => info!("登录态：未登录"),
        }
        // 设备 id 也按签发 origin 分区；只允许同源 profile 修复对应分区，
        // 旧版全局 device_id 不再参与联网，避免跨自建服务关联或误认设备。
        if let (Ok(origin), Some(profile)) = (
            cloud::canonical_server_origin(&current_server_url),
            user_profile.as_ref(),
        ) {
            if profile_origin_migrated {
                cloud::claim_legacy_device_id_for_origin(&origin, profile.device_id.as_deref());
            }
            cloud::reconcile_device_id_for_origin(
                &origin,
                profile.auth_origin.as_deref(),
                profile.device_id.as_deref(),
            );
        }
        if profile_origin_migrated {
            if let Some(profile) = user_profile.as_ref() {
                // 先完成旧 device_id 的单次认领，再提交 profile schema 迁移；
                // 中途崩溃时下次启动仍会重试，不留下“已迁移但未标记旧身份”的窗口。
                profile.save();
            }
            log::info!("已为旧版登录档案补齐服务端来源绑定");
        }

        // SSH 库存按 Lumen 账号隔离；账号首次使用时先以不可逆认领标记
        // 导入未登录清单。无账号/账号 ID 非规范时仍只读写 Local，绝不
        // 把一份未确认归属的缓存交给同步 worker。
        let (ssh_store, ssh_store_load_error) = match paths::data_dir() {
            Some(data_root) => {
                match load_ssh_store_for_profile(
                    &data_root,
                    user_profile.as_ref(),
                    &current_server_url,
                ) {
                    Ok(store) => (Some(store), None),
                    Err(error) => {
                        error!("加载 SSH 库存失败（保留原文件）: {error}");
                        (None, Some(error.to_string()))
                    }
                }
            }
            None => (
                None,
                Some("无法解析 Lumen 数据目录，SSH 配置本次不可写".to_owned()),
            ),
        };
        let initial_auth_token = profile_auth_token(user_profile.as_ref(), &current_server_url);

        // —— egui 三件套 ——
        let egui_ctx = egui::Context::default();
        shell::theme::apply_style(&egui_ctx, &shell::theme::shell_palette(theme_info));
        shell::theme::install_cjk_fonts(&egui_ctx);
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(scale),
            None,
            Some(renderer.device().limits().max_texture_dimension_2d as usize),
        );
        let egui_renderer = egui_wgpu::Renderer::new(
            renderer.device(),
            renderer.surface_format(),
            egui_wgpu::RendererOptions::default(),
        );

        // 终端区初值：窗口减去侧栏宽度与顶部双栏（标题栏 + 应用工具栏，
        // F12 批1）合计高度（首帧 egui 布局后按实际窗格矩形校正，文件树
        // 栏宽度即在首帧补扣）。窗格离屏纹理在首帧 RedrawRequested 懒创建
        // （布局前不知道各窗格尺寸）。
        let sidebar_px = (app_settings.layout.sidebar_width * scale).round();
        let topbar_px = ((shell::topbar::HEIGHT + shell::toolbar::HEIGHT) * scale).round();
        let term_w = ((size.width as f32 - sidebar_px).max(1.0)) as u32;
        let term_h = ((size.height as f32 - topbar_px).max(1.0)) as u32;

        // 单会话兜底的行列数按整个终端区估算；多窗格恢复时**逐窗格**
        // 按还原布局预切矩形估算（见 estimate_restored_pane_px——B2
        // 修复：旧实现全员按整区 spawn，首帧腰斩级缩行 resize 与首个
        // 提示符打印撞车，是症状①②的共同触发器）。
        // M4.1 批C 注：初始化时 term 尚未 spawn（无 block 数据）→ Fallback →
        // footer_px=0；此处 grid_size_for 等价 footer_px=0，正确。
        // 首帧 RedrawRequested 按真实 footer 高度精确校正。
        let (rows, cols) = renderer.grid_size_for(term_w, term_h);
        info!("终端尺寸: {rows} 行 x {cols} 列（初始化估算，footer 待首帧校正）");
        // 估算用终端工作区（逻辑点）：再扣文件树栏（启动时默认展开，
        // 宽度来自设置）。与首帧实际布局的残差只剩面板边距像素级出入。
        let filetree_px = (app_settings.layout.filetree_width * scale).round();
        let est_area = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(
                (term_w as f32 - filetree_px).max(1.0) / scale,
                term_h as f32 / scale,
            ),
        );

        // PTY 事件走 per-session 有界通道（Session 自持接收端），唤醒
        // 走全局去重的 PtyWake（见 session.rs 模块文档）。
        let wake_pending = Arc::new(AtomicBool::new(false));

        // —— 会话恢复（F4/F5）：sessions.json 有效时按嵌套结构逐 tab
        // 逐窗格重开 shell（初始目录用保存的 cwd，失效回退默认并提示；
        // 屏幕内容不恢复，是新 shell）；缺失/损坏/全部 spawn 失败回退
        // 单默认会话。旧平铺格式由 sessions_store 读侧自动迁移。 ——
        let stored = sessions_store::SessionsFile::load();
        let mut tabs: Vec<Tab> = Vec::new();
        let mut next_session_id: SessionId = 0;
        let mut next_tab_id: TabId = 0;
        let mut active_idx = 0usize;
        // 保存的 cwd 已失效（目录被删/网络盘离线）的窗格数（toast 一次）。
        let mut stale_cwd = 0usize;
        // 成功还原布局比例的 tab 数（F7 持久化；恢复日志用）。
        let mut restored_layouts = 0usize;
        if let Some(stored) = &stored {
            for tab_entry in &stored.tabs {
                // 逐窗格估算 spawn 尺寸（B2 修复）：布局/最大化的取值
                // 规则与下方实际还原同源。spawn 失败跳窗格时实际布局
                // 会回退均分、估算随之偏差，但那是罕见降级路径，只
                // 影响首帧 resize 幅度，不影响正确性。
                let n = tab_entry.panes.len();
                let est_layout =
                    PaneLayout::from_weights(n, &tab_entry.row_weights, &tab_entry.col_weights)
                        .unwrap_or_else(|| PaneLayout::uniform(n));
                let est_max = tab_entry.maximized.filter(|&m| m < n && n > 1);
                let est_px = estimate_restored_pane_px(est_area, &est_layout, n, est_max, scale);
                let mut panes: Vec<Session> = Vec::new();
                for (pi, pane_entry) in tab_entry.panes.iter().enumerate() {
                    let cwd = pane_entry.usable_cwd();
                    if let Some(saved) = pane_entry.cwd.as_deref() {
                        if cwd.is_none() {
                            stale_cwd += 1;
                            log::warn!(
                                "会话恢复：保存的工作目录已失效，回退默认目录: {}",
                                saved.display()
                            );
                        }
                    }
                    // 估算不可用（防御，不应发生）回退整区行列。
                    // M4.1 批C 注：恢复路径 term 尚未 spawn → Fallback → footer_px=0，
                    // grid_size_for 等价 footer_px=0。首帧 RedrawRequested 校正。
                    let (est_rows, est_cols) = est_px
                        .get(pi)
                        .map(|&(w, h)| renderer.grid_size_for(w, h))
                        .unwrap_or((rows, cols));
                    match Session::spawn(
                        next_session_id,
                        est_rows,
                        est_cols,
                        SCROLLBACK,
                        wake_pending.clone(),
                        self.proxy.clone(),
                        cwd,
                        app_settings.proxy.effective_url(),
                    ) {
                        Ok(mut s) => {
                            // 恢复窗格自定义名（F4 持久化）。
                            s.custom_title = pane_entry.custom_title.clone();
                            next_session_id += 1;
                            panes.push(s);
                        }
                        // 单窗格 spawn 失败（shell 缺失等极端情况）跳过
                        // 该窗格，不连坐其余。
                        Err(e) => error!("恢复窗格失败（跳过该窗格）: {e:#}"),
                    }
                }
                if panes.is_empty() {
                    // 整个 tab 的窗格都没起来：跳过该 tab。
                    continue;
                }
                // 最大化态还原（P14）：读侧已夹紧，这里再按实际起来的
                // 窗格数防御（spawn 失败跳窗格会改变数量）；单格无最大
                // 化语义。最大化期间焦点强制为最大化格。
                let maximized = tab_entry
                    .maximized
                    .filter(|&m| m < panes.len() && panes.len() > 1);
                let focused = maximized.unwrap_or(tab_entry.focused.min(panes.len() - 1));
                // 布局比例还原（F7 持久化）：保存的权重形状须与实际
                // 起来的窗格数一致（spawn 失败跳窗格会改变数量）且
                // 数值合法，否则回退均分（旧 v2 无字段也走这条路）。
                let layout = match PaneLayout::from_weights(
                    panes.len(),
                    &tab_entry.row_weights,
                    &tab_entry.col_weights,
                ) {
                    Some(l) => {
                        restored_layouts += 1;
                        l
                    }
                    None => PaneLayout::uniform(panes.len()),
                };
                tabs.push(Tab {
                    id: next_tab_id,
                    custom_title: tab_entry.custom_title.clone(),
                    panes,
                    focused,
                    layout,
                    maximized,
                });
                next_tab_id += 1;
            }
            if !tabs.is_empty() {
                active_idx = stored.active_tab.min(tabs.len() - 1);
                let pane_total: usize = tabs.iter().map(|t| t.panes.len()).sum();
                info!(
                    "会话恢复：{} 个 tab / {pane_total} 个窗格，激活 #{active_idx}（cwd 失效 {stale_cwd} 个，布局比例还原 {restored_layouts} 个 tab）",
                    tabs.len()
                );
            }
        }
        if tabs.is_empty() {
            tabs.push(Tab {
                id: next_tab_id,
                custom_title: None,
                panes: vec![Session::spawn(
                    next_session_id,
                    rows,
                    cols,
                    SCROLLBACK,
                    wake_pending.clone(),
                    self.proxy.clone(),
                    None,
                    app_settings.proxy.effective_url(),
                )?],
                focused: 0,
                layout: PaneLayout::uniform(1),
                maximized: None,
            });
            next_session_id += 1;
            next_tab_id += 1;
        }

        let clipboard = match arboard::Clipboard::new() {
            Ok(c) => Some(c),
            Err(e) => {
                error!("剪贴板不可用: {e}");
                None
            }
        };

        let perf = std::env::var("LUMEN_PERF")
            .ok()
            .and_then(|p| std::fs::File::create(p).ok());

        // —— 命令历史库（M4.1 批D2）——
        // 启动时加载磁盘历史，顺序：磁盘 JSONL → PSReadLine 种子（首次）。
        // 加载失败降级空库，记 warn 日志，不阻断启动。
        #[cfg(feature = "input-editor")]
        let history_store = history::HistoryStore::load();

        // 在 app_settings 被 move 进 AppState 之前读出 classic_mode（第十八轮）。
        let init_force_fallback = app_settings.classic_mode;
        // SSH 模式启动时先停在服务器选择层，绝不能让后台本地 PTY
        // 取得键盘/IME 焦点。
        let init_terminal_focused = !app_settings.layout.view_mode.is_ssh();
        // auto_check 初值（app_settings 随后 move 进 settings 字段，先取出）。
        let init_auto_check = app_settings.update.auto_check;
        // 生效代理初值（同上，先取出 owned）。
        let init_proxy = app_settings.proxy.effective_url().map(str::to_owned);

        // F3：后台更新线程 → 主循环的消息通道。
        let (update_tx, update_rx) = crossbeam_channel::unbounded();
        // P0 应用锁：配置独立于 settings.json；密码作业在后台线程，
        // 结果沿 bounded(1) 通道回主循环。
        let app_lock = app_lock::AppLock::load();
        let (lock_crypto_tx, lock_crypto_rx) = crossbeam_channel::bounded(1);
        let now = Instant::now();

        let mut state = AppState {
            perf,
            perf_t0: Instant::now(),
            last_render_at: None,
            last_term_render_at: None,
            window,
            renderer,
            tabs,
            active_tab: active_idx,
            next_session_id,
            next_tab_id,
            // M7：**必须写在 `wake_pending` 之前** —— 结构体字面量按书写顺序求值，
            // 写在后面就会用到已经被 move 进字段的 `wake_pending`。
            // 传的是 `AppState` 那个**全局**唤醒标志（不能另起一个，否则会丢唤醒，
            // 理由见 `llm_runner::LlmRunnerManager::new`）。
            llm_runners: llm_runner::LlmRunnerManager::new(llm_runner::Waker::new(
                self.proxy.clone(),
                wake_pending.clone(),
            )),
            wake_pending,
            proxy: self.proxy.clone(),
            settings: app_settings,
            app_lock,
            lock_crypto_tx,
            lock_crypto_rx,
            lock_ui: shell::lock_ui::LockUiState::default(),
            last_local_input: now,
            next_lock_poll: now,
            os_dark,
            last_sessions_snapshot: None,
            layout_dirty: false,
            layout_apply_logged: false,
            profile: user_profile,
            ssh_store,
            ssh_sync: None,
            ssh_empty_inventory: ssh::SshInventory::default(),
            ssh_runtime: ssh_runtime::SshRuntime::default(),
            ssh_force_credential_prompt: HashSet::new(),
            ssh_texture: None,
            ssh_rect_px: None,
            modifiers: ModifiersState::default(),
            clipboard,
            last_key_at: None,
            mouse_pos: (0.0, 0.0),
            hovered_link: None,
            hover_probe_cell: None,
            scrollbar_drag: None,
            mouse_report_held: [false; 3],
            mouse_report_last_cell: None,
            mirror_report_sid: None,
            autoscroll_drag: 0,
            autoscroll_at: None,
            last_left_click: None,
            update_tx,
            update_rx,
            update_available: None,
            update_downloading: false,
            update_ready: None,
            update_dismissed: false,
            update_auto_check: Arc::new(AtomicBool::new(init_auto_check)),
            update_proxy: Arc::new(std::sync::Mutex::new(init_proxy)),
            egui_ctx,
            auth_token: initial_auth_token,
            remote: remote::RemoteState::default(),
            remote_ws: remote_ws::RemoteWs::default(),
            remote_restore_attempted: false,
            mirror_src: None,
            mirror_bounds_sent: None,
            mirror_rect_px: None,
            mirror_pane_rects_px: Vec::new(),
            pending_paste: None,
            pending_ssh_download: None,
            ssh_file_clipboard: None,
            ssh_paste_chains: HashMap::new(),
            ssh_staged_uploads: HashSet::new(),
            ssh_clipboard_export_generation: 0,
            ssh_clipboard_exports: HashMap::new(),
            ssh_clipboard_ready_path: None,
            local_copy_rx: None,
            clip_fetch_rx: None,
            clipboard_svc: None,
            paste_refresh: None,
            filetree_hovered: false,
            filetree_focused: false,
            remote_viewport: None,
            remote_pane_viewports: None,
            mirror_texture: None,
            mirror_pane_textures: Vec::new(),
            egui_state,
            egui_renderer,
            pane_textures: HashMap::new(),
            pending_tex_free: Vec::new(),
            session_icon_exe: HashMap::new(),
            session_icon_tex: HashMap::new(),
            session_icon_rgba: HashMap::new(),
            remote_icon_tex: HashMap::new(),
            #[cfg(feature = "input-editor")]
            attachment_textures: HashMap::new(),
            last_icon_probe: None,
            last_llm_cli_probe: None,
            pane_rects_px: Vec::new(),
            pending_resize_dir: None,
            pane_close_rects_px: Vec::new(),
            divider_rects_px: Vec::new(),
            panel_resize_rects_px: Vec::new(),
            terminal_focused: init_terminal_focused,
            egui_repaint_at: None,
            was_popup_open: false,
            shell_state: shell::ShellState::default(),
            window_just_resized: false,
            bg_texture: None,
            // 从持久化设置恢复经典直通状态（第十八轮）。
            // settings.classic_mode 在 ToggleFallback 路径同步写盘，重启后还原。
            force_fallback: init_force_fallback,
            #[cfg(feature = "input-editor")]
            history: history_store,
            #[cfg(feature = "input-editor")]
            ghost_cache: (0, None),
            #[cfg(feature = "input-editor")]
            completion_candidates: Vec::new(),
            #[cfg(feature = "input-editor")]
            completion_sidecar: completion_sidecar::CompletionSidecar::new(self.proxy.clone()),
            #[cfg(feature = "input-editor")]
            completion_req_id: 0,
            #[cfg(feature = "input-editor")]
            footer_dragging: false,
            #[cfg(feature = "input-editor")]
            footer_drag_anchor: lumen_editor::Position::default(),
            #[cfg(feature = "input-editor")]
            footer_click_state: footer_mouse::ClickState::default(),
            #[cfg(feature = "input-editor")]
            footer_context_menu_at: None,
        };
        state.shell_state.settings.font_hint = font_hint;
        // 第十九轮：从持久化设置恢复文件树可见性初值。
        // sidebar_visible 直接驱动 if app_settings.layout.sidebar_visible { } 渲染分支，
        // 无需额外映射；filetree.visible 存于 ShellState（Default 硬编码 true），
        // 必须在此显式从 settings 读出。两入口（顶栏②按钮 + Ctrl+B）切换时均同步
        // 写盘（见 shell_out 处理段与 ToggleFiletree 分支），重启即可还原。
        state.shell_state.filetree.visible = state.settings.layout.filetree_visible;
        // SSH 监控面板/卡片折叠状态：从持久化设置恢复（四入口切换时经
        // shell_out.ssh_monitor_prefs_changed 写盘，见下方处理段）。
        state.shell_state.ssh_ui.load_monitor_prefs(
            state.settings.layout.ssh_monitor_collapsed,
            &state.settings.layout.ssh_monitor_cards_collapsed,
        );
        if let Some(error) = ssh_store_load_error {
            state
                .shell_state
                .toast
                .push(shell::toast::ToastKind::Error, error);
        }
        if profile_origin_reauth_required {
            state.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                "为保护账号数据，升级后请在当前 Lumen 服务器重新登录",
            );
        }
        state.ensure_ssh_sync_worker();
        // 恢复条目中保存的 cwd 已失效：回退默认目录并提示一次（F4）。
        if stale_cwd > 0 {
            state.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                i18n::fmt1(i18n::strings().toast_stale_cwd_fmt, stale_cwd),
            );
        }
        // 启动时加载背景图（P13）：enabled 且有 path 时解码上传 GPU。
        if state.settings.appearance.background.enabled
            && state.settings.appearance.background.path.is_some()
        {
            state.apply_background_image();
        }
        // 窗口标题对齐激活会话（恢复多会话时 active 可能非 0）。
        state.update_window_title();
        if state.app_lock.is_locked() {
            state.prepare_locked_ui();
        }

        // 片6 虚拟文件剪贴板：启动专用 STA OLE 线程。OLE 线程经 clip_fetch_tx 请主线程把远程
        // 文件下到临时文件（资源管理器粘贴远程虚拟文件时触发）；rx 在 user_event 排空。非 Windows
        // 为空桩。failure（OleInitialize 失败）时服务静默无效，不影响主程序。
        let (clip_fetch_tx, clip_fetch_rx) = std::sync::mpsc::channel();
        state.clip_fetch_rx = Some(clip_fetch_rx);
        state.clipboard_svc = Some(virtual_files::ClipboardService::start(
            self.proxy.clone(),
            state.wake_pending.clone(),
            clip_fetch_tx,
        ));

        // M3.8 批2 Snap Layouts 子类化：窗口创建后安装子类过程。
        // 失败时记 warn 日志并继续（Snap 是增强功能，不影响应用主体逻辑）。
        // 取 HWND：winit 使用 rwh_06，HasWindowHandle trait 提供 window_handle()。
        // raw-window-handle 0.6 中 Win32WindowHandle.hwnd 字段类型为 NonZeroIsize，
        // 调用 .get() 取出 isize 值传入 install。
        #[cfg(target_os = "windows")]
        {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match state.window.window_handle() {
                Ok(handle) => {
                    if let RawWindowHandle::Win32(wh) = handle.as_raw() {
                        // SAFETY: hwnd 来自 winit 刚创建的有效窗口（本函数内），
                        // 在 init 返回前窗口不会被销毁，时序成立。
                        let hwnd = wh.hwnd.get(); // NonZeroIsize::get() → isize，即 Win32 HWND 值
                        if unsafe { snap_layouts::install(hwnd) } {
                            log::info!("Snap Layouts 子类过程安装成功（hwnd={hwnd:#x}）");
                        } else {
                            log::warn!(
                                "Snap Layouts 子类过程安装失败，SetWindowSubclass 返回 FALSE"
                            );
                        }
                    } else {
                        log::warn!("Snap Layouts 子类化跳过：非 Win32 窗口句柄");
                    }
                }
                Err(e) => {
                    log::warn!("Snap Layouts 子类化跳过：获取 window_handle 失败（{e}）");
                }
            }
        }

        // F3：启动自动检查更新（节流——距上次检查不足 AUTO_CHECK_INTERVAL_MS
        // 则跳过；无更新静默；有更新经 channel 回主循环弹提示）。
        if state.settings.update.auto_check
            && update::should_auto_check(
                state.settings.update.last_check_ms,
                update::now_ms(),
                AUTO_CHECK_INTERVAL_MS,
            )
        {
            state.spawn_update_check(false);
        }
        // F3：运行期定时检查（每 UPDATE_POLL_INTERVAL 查一次，不只启动时）——
        // 长期开着 Lumen 不关也能收到新版。auto_check 关闭时线程内跳过。
        state.spawn_periodic_update_check();

        // —— 启动消白块：揭幕窗口 ——
        // 窗口以 with_visible(false) 隐藏创建，至此渲染器/字体/主题/egui 全
        // 部就绪、全程不可见（无白闪）。顺序固定、缺一不可：
        //   ① present_clear：同步把一帧主题底色塞进交换链（acquire 失败则
        //      静默跳过，退化为原白闪、不影响后续）。**用 catch_unwind 包裹**：
        //      wgpu 29 默认错误处理器对未捕获错误（OOM/校验类，受控单次取帧
        //      下几近不可能）会 panic，若不吞会经 init→resumed 上抛令进程退出、
        //      窗口一次都不显示——违背「显示绝不依赖渲染成功」的承诺。吞掉后
        //      照常显示，最坏退化为白闪。（对抗审查 finding#1。）
        //   ② set_visible(true)：**无条件**显示——winit 在事件循环线程同步
        //      执行 ShowWindow(SW_SHOW+SW_MAXIMIZE)（execute_in_thread 直跑），
        //      窗口必定露面且直接最大化（无尺寸跳变）。决不把「显示」依赖在
        //      任何隐藏窗口收不到的事件上（上一版误用 RedrawRequested 致窗口
        //      卡隐藏态的根错，此处堵死）；
        //   ③ request_redraw：随后正常渲染真实 UI（窗口已可见，事件可靠）。
        let present_panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.renderer.present_clear();
        }))
        .is_err();
        if present_panicked {
            log::warn!("启动铺底色帧 present_clear 触发 wgpu panic，已吞并照常显示窗口");
        }
        state.window.set_visible(true);
        state.window.request_redraw();

        Ok(state)
    }
}

/// F3 启动自动检查的最小间隔（30 分钟）：避免频繁开关应用每次都打网络，
/// 同时不至于太久收不到新版（海风哥 2026-06-14：6 小时太保守，半小时即可）。
const AUTO_CHECK_INTERVAL_MS: u64 = 30 * 60 * 1000;

/// F3 运行期定时检查更新的间隔（30 分钟）：长期开着 Lumen 不关也能定时
/// 收到新版（不只启动时查）。auto_check 关闭时跳过本轮。
const UPDATE_POLL_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// F7② 会话前台进程轮询间隔：进程快照较重，限频到 ~0.8s（命令起止的
/// 图标切换感知足够灵敏，开销可忽略）。
const ICON_PROBE_INTERVAL: Duration = Duration::from_millis(800);
/// LLM CLI 识别需独立于侧栏图标工作；同样节流进程快照查询。
const LLM_CLI_PROBE_INTERVAL: Duration = Duration::from_millis(500);
/// LLM CLI 的原生候选可能异步生成；在截止时间内允许任意数量的局部
/// PTY 重绘，不能用“无结果帧数”提前判定失败。
const LLM_SLASH_PROBE_TIMEOUT: Duration = Duration::from_millis(800);
/// 原生菜单每次移动一项。间隔需覆盖常见 TUI 的局部重绘；Kimi 有精确
/// 总数，其他 CLI 在选中项回环或连续多次无新增时结束。
const LLM_SLASH_SCAN_INTERVAL: Duration = Duration::from_millis(40);
const LLM_SLASH_SCAN_TIMEOUT: Duration = Duration::from_secs(90);
const LLM_SLASH_SCAN_MAX_STEPS: u16 = 2048;
const LLM_SLASH_SCAN_MAX_STAGNANT_STEPS: u16 = 256;
/// Ctrl+U 与 Kimi 的 Esc 必须分成两个 ConPTY 写入；每阶段留出一个
/// 很短的处理窗口，同时由 about_to_wait 定时唤醒，绝不等待 CLI 回显。
const LLM_SLASH_CLEAR_STAGE_DELAY: Duration = Duration::from_millis(60);

/// F7② 会话图标首抽后延迟重抽一次的间隔：首抽可能撞上前台进程刚 spawn、系统
/// 图标未就绪而抽到通用占位图标；隔此时间进程已稳定，重抽覆盖（> [`ICON_PROBE_INTERVAL`]，
/// 确保跨过至少一轮 probe）。
const ICON_REFRESH_DELAY: Duration = Duration::from_secs(3);

impl ApplicationHandler<PtyWake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_none() {
            match self.init(event_loop) {
                Ok(state) => self.state = Some(state),
                Err(e) => {
                    error!("初始化失败: {e:#}");
                    event_loop.exit();
                }
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: PtyWake) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        // F8 前台化：第二实例被单实例锁拒掉前发来的请求——恢复最小
        // 化并请求焦点（Windows 限制跨进程抢前台，focus_window 可能
        // 只闪任务栏，request_user_attention 兜底向用户示意）。
        if single_instance::take_foreground_request() {
            info!("处理前台化请求：set_minimized(false) + focus_window + request_user_attention");
            state.window.set_minimized(false);
            state.window.focus_window();
            state
                .window
                .request_user_attention(Some(winit::window::UserAttentionType::Informational));
        }
        if state.tabs.is_empty() {
            return; // 退出流程中（exit 后仍可能有滞后事件）
        }
        // 应用锁密码线程复用 PtyWake 唤醒主循环；先提交结果，再处理
        // PTY/远程事件，解锁首帧即可与最新终端状态对齐。
        state.drain_lock_crypto();
        state.drain_ssh_runtime();
        state.drain_ssh_sync();
        // F3：drain 更新消息（发现新版/检查结果/下载完成）。下载完成会拉起
        // 安装器并请求优雅退出——走与 CloseRequested 同路径（落盘 + flush
        // 历史 + exit），让安装器替换 exe。
        if state.drain_update_msgs() {
            state.persist_sessions();
            #[cfg(feature = "input-editor")]
            state.history.flush_on_exit();
            event_loop.exit();
            return;
        }
        // 先清挂起标志再 drain：drain 期间新到的数据会触发下一个 wake，不丢。
        // 用 SeqCst（非 Release）：full barrier 阻止下面的 drain/pump_remote 被重排到
        // 清标志之前——否则存在丢唤醒窗口（后台线程 swap(true) 见旧值跳过发 PtyWake，
        // 而数据已入队却无人唤醒处理）。后台线程的 swap RMW 恒读最新值，故 store 侧的
        // 顺序是关键。
        state.wake_pending.store(false, Ordering::SeqCst);

        // M5.3：先处理远程控制 WS（失焦也经此路径，bug2）——收帧 + 应用远程输入 +
        // 整屏快照转发。置于 PTY drain 之前，保证「快照先于实时增量」。
        state.pump_remote();

        // M7：headless LLM runner 泵。紧跟 pump_remote，且**在 `remote_ws.sub_target()` 的
        // 任何嵌套之外**——headless 会话与手机订阅哪个 tab 无关（见 `pump_llm_runners` 文档）。
        // 有事件即重绘：桌面图标的两个计数（在线手机数 · 运行中任务数）挂在这上面，
        // ControlFlow::Wait 下不重绘就永远停在旧数字。
        if state.pump_llm_runners() {
            state.window.request_redraw();
        }

        // M5.2 设备列表：后台 worker 拉到新列表后经 PtyWake 唤醒到此（ControlFlow::Wait 下
        // request_repaint 单独叫不醒空闲循环）。**必须在此排空 + 重绘**——否则该 PtyWake 不带 PTY
        // 数据，下方按需重绘不触发，设备在线/上下线永不刷新（要切 tab 才更新，海风哥实测踩坑）。
        if state.remote.poll() {
            state.window.request_redraw();
        }

        // 片6 虚拟文件剪贴板：OLE 线程请求把远程文件下到临时文件（资源管理器粘贴远程虚拟文件
        // 触发）。先收齐释放 rx 借用，再逐个起 Clipboard Fetch（传完经 reply 回临时文件路径）。
        let clip_cmds: Vec<remote_ws::ClipFetchCmd> = state
            .clip_fetch_rx
            .as_ref()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();
        for cmd in clip_cmds {
            state.remote_ws.start_clip_fetch(cmd.path, cmd.data_tx);
        }

        // 本机复制粘贴（local→local）完成回包 → toast。后台 fs 递归复制完会 send PtyWake 唤醒到此
        // user_event；try_recv 非阻塞，无在途/未完成则空过。先取值再清字段，规避 rx 借用与赋值冲突。
        if let Some((done, skipped, errors)) = state
            .local_copy_rx
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        {
            state.local_copy_rx = None;
            let kind = if errors > 0 {
                shell::toast::ToastKind::Warn
            } else {
                shell::toast::ToastKind::Info
            };
            state.shell_state.toast.push(
                kind,
                i18n::fmt3(i18n::strings().local_copy_done_fmt, done, skipped, errors),
            );
            // 本机复制完成 → 刷新目标目录，新文件立即显示。
            state.apply_paste_refresh();
            state.window.request_redraw();
        }

        let drain_t0 = Instant::now();
        let active_tab = state.active_tab;
        let focused = state.tabs[active_tab].focused;
        let pane_counts: Vec<usize> = state.tabs.iter().map(|t| t.panes.len()).collect();
        // per-session 通道按「焦点优先」轮询（需求池 P5 + F5 分屏）：
        // 先清焦点窗格的通道——其量受前台回显/输出规模天然限制且
        // 消化快于产出；其余窗格（含激活 tab 的可见兄弟窗格）按
        // BG_DRAIN_CAP 配额逐个消化，超限事件留在各自通道里由有界
        // 容量反压各自的读线程，互不连坐。可见窗格本就按合帧节拍上
        // 屏，配额只是把单轮消化切片，洪泛不抢占焦点窗格的打字。旧
        // 的全局单通道下前台回显最坏要排在 ~2MB 洪泛之后（队头阻塞，
        // 延迟尖峰 10~30ms）。
        let order = drain_order(&pane_counts, active_tab, focused);
        // 每窗格本轮已消化字节数 / 是否有新数据（按 order 下标）。
        let mut consumed = vec![0usize; order.len()];
        let mut got_data = vec![false; order.len()];
        let mut exited: Vec<SessionId> = Vec::new();
        // 非焦点窗格超出本轮配额提前停手（需补发 wake 续处理）。
        let mut backlog = false;
        for (k, &(ti, pi)) in order.iter().enumerate() {
            let is_focused = ti == active_tab && pi == focused;
            // Receiver 克隆一份（Arc 浅拷贝）避免循环内长借用 state。
            let rx = state.tabs[ti].panes[pi].rx.clone();
            loop {
                if !is_focused && consumed[k] >= BG_DRAIN_CAP {
                    // 本轮配额用尽：剩余留到补发的下一个 wake 再消化，
                    // 前台打字不被 yes 级输出抢占主线程。
                    backlog = true;
                    break;
                }
                let Ok(ev) = rx.try_recv() else {
                    break;
                };
                match ev {
                    PtyEvent::Data(bytes) => {
                        consumed[k] += bytes.len();
                        // 调试辅助：LUMEN_VT_LOG=<路径> 时把 PTY 原始字节追加到文件。
                        if let Ok(path) = std::env::var("LUMEN_VT_LOG") {
                            use std::io::Write;
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(path)
                            {
                                let _ = f.write_all(&bytes);
                            }
                        }
                        // 取证设施（B3）：LUMEN_DUMP_PTY=<dir> 时按会话 id
                        // 把原始字节流追加写入 <dir>/pane-<id>.bin，同时把
                        // 可读的转义序列表示追加写入 <dir>/pane-<id>.txt。
                        // 环境变量门控，零开销（仅读一次 env，实际写盘在
                        // 条件分支内），长期保留供现场取证用。
                        if let Ok(dir) = std::env::var("LUMEN_DUMP_PTY") {
                            let sid = state.tabs[ti].panes[pi].id;
                            use std::io::Write;
                            let bin_path = format!("{dir}/pane-{sid}.bin");
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&bin_path)
                            {
                                let _ = f.write_all(&bytes);
                            }
                            let txt_path = format!("{dir}/pane-{sid}.txt");
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&txt_path)
                            {
                                // 人类可读格式：控制字符/转义序列以 <XX> 或
                                // <ESC[...X> 表示，普通可打印字符原样输出。
                                let _ = f.write_all(dump_pty_readable(&bytes).as_bytes());
                            }
                        }
                        state.tabs[ti].panes[pi].advance_terminal(&bytes);
                        #[cfg(feature = "input-editor")]
                        state.refresh_llm_slash_candidates(ti, pi);
                        // M5.3 part3d 被控端：被控期间转发**控制端订阅会话**的焦点窗格 PTY
                        // 输出给控制端（带双 id；与被控端自身焦点解耦——需求 c/e）。整屏初始
                        // 快照由 pump_remote 的 mirror_src 变化触发 SubscriptionStarted，先于
                        // 此实时增量（D3 保序）。订阅会话可为被控端后台 tab，其窗格照常 drain。
                        if matches!(
                            state.remote_ws.session.as_ref().map(|s| s.role),
                            Some(lumen_protocol::remote::Role::Controlled)
                        ) {
                            if let Some(sub_id) = state.remote_ws.sub_target() {
                                // Phase 3：转发订阅会话的**全部窗格**（双路 tee）——非焦点窗格亦推，
                                // 控制端按 session_id 路由到对应镜像。后台窗格受 BG_DRAIN_CAP 节流（D7）。
                                if state.tabs[ti].id == sub_id {
                                    let session_id = state.tabs[ti].panes[pi].id;
                                    state
                                        .remote_ws
                                        .send_output_with_id(sub_id, session_id, &bytes);
                                }
                            }
                        }
                        got_data[k] = true;

                        // —— 块闭合探针（M4.1 批D2）——
                        // advance() 处理 OSC 133;D 后会新增已闭合块；
                        // 探针用已见闭合块数与当前闭合块数比对。
                        #[cfg(feature = "input-editor")]
                        {
                            // 先收集所有需要处理的数据（不持有不可变借用）。
                            #[allow(clippy::type_complexity)]
                            let (closed_now, new_block_data): (
                                usize,
                                Vec<(u64, Option<i32>, Option<String>)>,
                            ) = {
                                let pane = &state.tabs[ti].panes[pi];
                                let blocks = pane.term.blocks();
                                let closed_blocks: Vec<_> =
                                    blocks.iter().filter(|b| b.is_closed()).collect();
                                let closed_now = closed_blocks.len();
                                let last = pane.last_seen_closed_blocks;
                                // M4.2：连同 shell 权威命令文本（cmd_text）一起收集，
                                // 块闭合时用于对账历史记录。
                                let new_data: Vec<(u64, Option<i32>, Option<String>)> =
                                    if closed_now > last {
                                        closed_blocks[last..]
                                            .iter()
                                            .map(|b| (b.id, b.exit_code, b.cmd_text.clone()))
                                            .collect()
                                    } else {
                                        Vec::new()
                                    };
                                (closed_now, new_data)
                            };

                            if !new_block_data.is_empty() {
                                // 耗时：从 pending_submit 的提交时刻到现在（在写 pane 前取）。
                                let duration_ms = state.tabs[ti].panes[pi]
                                    .pending_submit
                                    .as_ref()
                                    .map(|(_, t, _)| t.elapsed().as_millis() as u64)
                                    .unwrap_or(0);
                                // 取 pending_submit 中的文本和 history_idx（clone 脱离借用）。
                                let pending = state.tabs[ti].panes[pi].pending_submit.clone();

                                for (block_id, exit_code, cmd_text) in &new_block_data {
                                    // 设置退出码角标（仅 Compose 态会显示，Running 态下也存，
                                    // 下一次进入 Compose 态时 badge 仍在，按任意键清除）。
                                    if let Some(code) = exit_code {
                                        state.tabs[ti].panes[pi].exit_badge =
                                            Some(lumen_renderer::composer_view::ExitBadge {
                                                exit_code: *code,
                                                duration_ms,
                                            });
                                    }
                                    // 历史库回填 exit_code + duration_ms
                                    if let Some((ref submitted_text, _, history_idx)) = pending {
                                        // 取当前库中该条目的 ts（用于 text+ts 匹配校验）
                                        let ts = state
                                            .history
                                            .entries()
                                            .get(history_idx)
                                            .map(|e| e.ts)
                                            .unwrap_or(0);
                                        state.history.backfill(
                                            history_idx,
                                            submitted_text,
                                            ts,
                                            exit_code.unwrap_or(-1),
                                            duration_ms,
                                        );
                                    }
                                    log::debug!(
                                        "[BlockClosed] block_id={block_id} exit_code={exit_code:?} duration_ms={duration_ms} cmd_text={cmd_text:?}"
                                    );
                                }
                                // M4.2 对账：**所有块 backfill 完成后**再统一对账一次
                                // ——backfill 以 submitted 文本为匹配键，循环内提前
                                // reconcile 会把 entries[idx].text 改成 authoritative，
                                // 毒化同批后续块的 backfill 匹配（审查 finding，致最后
                                // 一条命令 exit_code 丢失）。取最后一个带权威命令文本
                                // 的块，与编辑器提交文本不一致时以 shell 为准校正。
                                if let Some((ref submitted_text, _, history_idx)) = pending {
                                    if let Some(authoritative) = new_block_data
                                        .iter()
                                        .rev()
                                        .find_map(|(_, _, ct)| ct.as_deref())
                                    {
                                        let ts = state
                                            .history
                                            .entries()
                                            .get(history_idx)
                                            .map(|e| e.ts)
                                            .unwrap_or(0);
                                        state.history.reconcile_text(
                                            history_idx,
                                            submitted_text,
                                            ts,
                                            authoritative,
                                        );
                                    }
                                }
                                // 回填完成后清 pending_submit（仅清一次，即使多块闭合）。
                                if pending.is_some() {
                                    state.tabs[ti].panes[pi].pending_submit = None;
                                }
                                state.tabs[ti].panes[pi].last_seen_closed_blocks = closed_now;
                            }
                        }
                    }
                    PtyEvent::Exited => exited.push(state.tabs[ti].panes[pi].id),
                }
            }
        }

        // —— 每窗格的批后处理：应答回写对所有窗格照常执行（后台不回
        // 写 DSR/DA 会卡死对端程序）；渲染调度对激活 tab 的**全部可见
        // 窗格**生效（与后台 tab 的本质区别：可见窗格都要上屏），后台
        // tab 的窗格只更新 ESU 标记并标未读点。
        let mut focused_stats = None;
        // 后台窗格未读点 false→true 翻转：侧栏需要一次重绘（仅翻转
        // 那次请求，已置位后的后续批次不再重复，保持「后台 drain 不
        // 打扰前台渲染节拍」的原设计）。
        let mut needs_shell_redraw = false;
        // 循环内长借用 state.tabs：限频基准先拷出、重绘请求收集为标志。
        let last_term_render_at = state.last_term_render_at;
        let mut want_redraw = false;
        for (k, &(ti, pi)) in order.iter().enumerate() {
            if !got_data[k] {
                continue;
            }
            let visible = ti == active_tab;
            // 最大化期间被隐藏的激活 tab 窗格（P14）：照常消化与回写
            // （下方），但不参与渲染调度（同「后台 tab 不渲染」闸门）；
            // 也不标未读点——所属 tab 本身可见，激活态下挂未读点既
            // 矛盾也无法被 activate() 清除。
            let hidden_by_max = visible && state.tabs[ti].maximized.is_some_and(|m| m != pi);
            let s = &mut state.tabs[ti].panes[pi];
            // 终端应答（DSR/DA/DECRQM 等）回写给 shell。
            let resp = s.term.take_responses();
            if !resp.is_empty() {
                let _ = s.pty.write(&resp);
            }
            // 进入备用屏幕（vim/codex 全屏）时块交互无意义且不可见，
            // 清掉选中态，避免 Ctrl+C 被残留选中块吞成「复制」。
            if s.term.is_alt_screen() && s.selected_block.is_some() {
                s.selected_block = None;
            }
            let sync = s.term.is_synchronized();
            let esu_mark = s.term.esu_mark();
            let frame_completed = esu_mark != s.last_esu_mark && !sync;
            s.last_esu_mark = esu_mark;

            if !visible {
                if !s.has_unseen_output {
                    s.has_unseen_output = true;
                    needs_shell_redraw = true;
                }
                continue;
            }
            if hidden_by_max {
                continue;
            }
            if pi == focused {
                focused_stats = Some((sync, frame_completed, s.term.cursor_unsettled()));
            }
            if frame_completed {
                // 本批完成了 DEC 2026 同步帧：协议语义就是「立即原子
                // 呈现」，零等待直接渲染（codex 打字回显走这条快路）。
                // 但渲染频率以 ~8ms 为下限：极速输入（百帧每秒级回显）
                // 时把积压帧合并，避免渲染请求超出显示能力拖垮主线程。
                // 限频基准用 last_term_render_at（终端帧时间戳，整帧
                // 粒度——多窗格同帧渲染共享一个基准）：鼠标驱动的纯
                // UI 重绘不该反向推迟完成帧的上屏。
                let now = Instant::now();
                let recent = last_term_render_at
                    .filter(|t| now.duration_since(*t) < Duration::from_millis(8));
                if let Some(last) = recent {
                    let at = last + Duration::from_millis(8);
                    s.redraw_at = Some(at);
                    s.redraw_hard_at = None;
                    s.redraw_abs_at = Some(at + Duration::from_millis(50));
                } else {
                    s.redraw_at = None;
                    s.redraw_hard_at = None;
                    s.redraw_abs_at = None;
                    // 直渲请求是「欠帧」：到 RedrawRequested 执行前若有
                    // 新 BSU 批到达重新拉起同步区间，门控可暂缓这帧、
                    // 交给重新武装的渲染计划在 ESU 后补画完整帧（直接
                    // 放行会把半成品画上屏——蓝条闪烁，需求池 P1）；
                    // 暂缓以欠帧起点 + REDRAW_ABS_CAP 为限，见门控注释。
                    s.term_frame_due_since.get_or_insert(now);
                    want_redraw = true;
                }
            } else {
                // 无同步协议的流（普通 shell/claude）：静默合帧，每批
                // 数据推后渲染时刻，流停了才画（见 about_to_wait）；
                // 硬上限自首批起算，保障刷新率。
                let now = Instant::now();
                s.redraw_at = Some(now + REDRAW_DEBOUNCE);
                if s.redraw_hard_at.is_none() {
                    s.redraw_hard_at = Some(now + REDRAW_HARD_CAP);
                    s.redraw_abs_at = Some(now + REDRAW_ABS_CAP);
                }
            }
        }
        // 窗口标题跟随焦点窗格（OSC 标题/cwd 可能随本批数据更新）。
        if focused_stats.is_some() {
            state.update_window_title();
        }
        // 会话快照持久化（F4）：任一窗格的 cwd（OSC 9;9）可能随本批
        // 数据更新，与上次写盘快照比对后按需落盘（实际写频≈用户 cd
        // 频率，比对不同才写）。
        if got_data.iter().any(|&b| b) {
            state.persist_sessions();
            // M5.3 part3c-2（本轮反馈 #3「cd 后远程树延迟数秒才刷新」）：被控端 cd 后秒级刷新控制端
            // 树根。唯一的 send_root_changed 调用点在 pump_remote()（上方约 3704），它跑在
            // term.advance()（约 3780，OSC 9;9 在此把 cwd 改成新目录）**之前**，读到的是旧 cwd → 被
            // send_root_changed 的去重逻辑丢弃；而 cd 后被控端空闲、ControlFlow::Wait 没有「cwd 变了」
            // 的主动唤醒，新 cwd 只能卡到下一个偶发事件（下段 PTY 输出/鼠标键盘）才被顺带推出，用户
            // 感知为数秒延迟。此处在 drain 之后、cwd 已随本批 advance 为最新值时补推一次：
            // send_root_changed 自带去重（remote_root_sent），只在真变化时发帧，cd 当帧即推 RootChanged。
            if matches!(
                state.remote_ws.session.as_ref().map(|s| s.role),
                Some(lumen_protocol::remote::Role::Controlled)
            ) {
                // 修③：跟控制端订阅的会话 cwd（回退被控端焦点 tab）。
                if let Some(cwd) = state
                    .remote_ws
                    .sub_target()
                    .and_then(|sid| state.tabs.iter().find(|t| t.id == sid))
                    .or_else(|| state.tabs.get(state.active_tab))
                    .and_then(remote_root_cwd)
                {
                    state.remote_ws.send_root_changed(cwd);
                }
            }
        }
        if want_redraw || needs_shell_redraw {
            state.window.request_redraw();
        }
        let total: usize = consumed.iter().sum();
        if total > 0 {
            let (sync, fc, unsettled) = focused_stats.unwrap_or_default();
            state.perf_log(format_args!(
                "drain {total}B 耗时 {:?} sync={sync} esu帧={fc} unsettled={unsettled} 后台积压={backlog}",
                drain_t0.elapsed()
            ));
        }

        // —— 生命周期：shell 退出（海风哥 2026-06-13 体验优化）——
        // 多窗格：关闭退出的那格（F5：剩余窗格继续）；
        // 单窗格：原位重启一个新 shell，不关应用（单窗口 `exit` 后立即
        // 换一个干净 PowerShell 继续用，省去重开 app）。
        for sid in exited {
            let Some((ti, pi)) = state.find_pane(sid) else {
                continue;
            };
            if state.tabs[ti].panes.len() > 1 {
                info!("会话 id={sid} 的 shell 已退出（多窗格），关闭该窗格");
                // 多窗格时 close_pane 必返回 false（不会退出应用）。
                state.close_pane(ti, pi);
            } else {
                info!("会话 id={sid} 的 shell 已退出（单窗格），原位重启新 shell");
                if state.respawn_pane(ti, pi) {
                    info!("重启失败、最后会话已关闭，退出应用");
                    event_loop.exit();
                    return;
                }
            }
        }

        // 后台数据滞留：补发一个 wake 接着消化（与转发线程同一套去重）。
        if backlog
            && !state.wake_pending.swap(true, Ordering::AcqRel)
            && state.proxy.send_event(PtyWake).is_err()
        {
            error!("补发 PtyWake 失败：事件循环已关闭");
        }

        // M4.4 批2：drain sidecar 命令补全响应，合并进候选列表。
        #[cfg(feature = "input-editor")]
        {
            let responses = state.completion_sidecar.poll();
            let mut sidecar_merged = false;
            for resp in responses {
                // 丢弃过期响应（id 不匹配当前在途请求）。
                if resp.id != state.completion_req_id || state.completion_req_id == 0 {
                    continue;
                }
                if resp.items.is_empty() {
                    continue;
                }
                // 取当前行文本（用于 char→byte 换算）。
                let line_text = {
                    let ti = state.active_tab;
                    let pi = state.tabs[ti].focused;
                    let view = state.tabs[ti].panes[pi].editor.view();
                    let cur = view.cursor();
                    view.line(cur.line).to_owned()
                };
                // 把 sidecar 候选转成 Completion，按 display 去重后追加。
                // 先收集已有 display 字符串（owned），释放借用后再 push。
                let existing_displays: std::collections::HashSet<String> = state
                    .completion_candidates
                    .iter()
                    .map(|c| c.display.clone())
                    .collect();
                // char→byte 区间只算一次（resp 内所有候选共享同一 ri/rl）。
                let replace_range = Some(completion_sidecar::char_range_to_bytes(
                    &line_text, resp.ri, resp.rl,
                ));
                let mut new_items: Vec<completion::Completion> = Vec::new();
                for item in &resp.items {
                    if item.text.is_empty() {
                        continue;
                    }
                    // ProviderContainer = 目录。
                    let is_dir = item.kind == "ProviderContainer";
                    // display 与 replacement 统一使用 item.text。
                    let display = if is_dir && !item.text.ends_with('/') {
                        format!("{}/", item.text)
                    } else {
                        item.text.clone()
                    };
                    if existing_displays.contains(&display) {
                        continue; // 去重：与文件路径候选同名的跳过。
                    }
                    new_items.push(completion::Completion {
                        display,
                        replacement: item.text.clone(),
                        is_dir,
                        replace_range,
                    });
                }
                state.completion_candidates.extend(new_items);
                sidecar_merged = true;
            }
            if sidecar_merged
                && !state.completion_candidates.is_empty()
                && !state.shell_state.text_editor.is_visible()
            {
                // 若弹窗尚未打开（文件路径候选为空、等 sidecar），现在打开。
                if !state.shell_state.completion.open {
                    state.shell_state.completion.open = true;
                    state.shell_state.completion.selected = 0;
                    state.shell_state.completion.passive = false;
                    state.terminal_focused = false;
                }
                state.window.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.tabs.is_empty() {
            return; // 退出流程中
        }
        // 渲染调度看激活 tab **全部窗格**的计划（后台 tab 的窗格不设
        // 计划、不打扰渲染）。逐窗格判定：
        // - 计划未到点 → 计入下次唤醒时刻（取最早）；
        // - 到点但正处于同步区间且 abs 兜底未到、欠帧未超龄 → 顺延
        //   （小步 2ms 等 ESU，原单会话语义）；
        // - 到点且可渲染 → 清计划、记欠帧起点，本轮立即请求重绘
        //   （其余仍在同步区间的窗格由 RedrawRequested 的逐窗格门控
        //   各自跳过，保留上一完整帧）。
        let now = Instant::now();
        // SSH actors intentionally do not share the local PTY wake channel.
        // Poll only while at least one actor exists, at ~30fps. This keeps
        // background sessions and monitoring current even behind the app lock,
        // while an idle app returns to ControlFlow::Wait with no busy loop.
        if state.ssh_runtime.has_live_connections() {
            state.drain_ssh_runtime();
        }
        // notifier 正常会用 PtyWake 立即唤醒；这里作为安全帧点非阻塞
        // drain，覆盖事件循环合并/关闭前的边界，不引入轮询唤醒。
        state.drain_ssh_sync();
        let ssh_poll_at = state
            .ssh_runtime
            .has_live_connections()
            .then(|| now + Duration::from_millis(33));
        #[cfg(feature = "input-editor")]
        let slash_cleanup_at = state.poll_llm_slash_probe_clear(now);

        // 应用锁退避与自动入口。软件锁默认关闭；所有入口都先经过
        // is_enabled 门控，因此缺少 app_lock.json 的旧用户零行为变化。
        let retry_was_active = !state.app_lock.retry_remaining(now).is_zero();
        state.app_lock.expire_retry_if_due(now);
        if retry_was_active && state.app_lock.retry_remaining(now).is_zero() {
            state.lock_ui.clear_error();
            state.window.request_redraw();
        }
        #[cfg(target_os = "windows")]
        if snap_layouts::take_security_lock_request()
            && state.app_lock.is_enabled()
            && state.app_lock.config().lock_on_resume()
        {
            state.lock_now();
        }
        if state.app_lock.is_enabled()
            && !state.app_lock.is_locked()
            && state.app_lock.config().idle_timeout_minutes() > 0
            && now >= state.next_lock_poll
        {
            state.next_lock_poll = now + Duration::from_secs(1);
            let idle = app_lock::system_idle_duration()
                .unwrap_or_else(|| now.saturating_duration_since(state.last_local_input));
            let limit =
                Duration::from_secs(u64::from(state.app_lock.config().idle_timeout_minutes()) * 60);
            if idle >= limit {
                state.lock_now();
            }
        }

        // 拖选边缘 auto-scroll：本地终端 或 镜像（远程视图）拖选进行中、鼠标停在内容区
        // 上/下边缘外时，按节流定时滚动一行 + 续选（露出 scrollback 上/下内容），并安排
        // 下次 tick。优先于下方渲染调度——它自带 request_redraw + WaitUntil 自维持节拍。
        let mirror_selecting = state.remote_ws.mirror_pane_selecting_sid().is_some();
        if !state.app_lock.is_locked()
            && state.autoscroll_drag != 0
            && (state.focused_pane().selecting || mirror_selecting)
        {
            if state.autoscroll_at.is_none_or(|t| now >= t) {
                if mirror_selecting {
                    state.tick_autoscroll_mirror_drag();
                } else {
                    state.tick_autoscroll_drag();
                }
                state.autoscroll_at = Some(now + AUTOSCROLL_DRAG_TICK);
                state.window.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                state.autoscroll_at.unwrap_or(now + AUTOSCROLL_DRAG_TICK),
            ));
            return;
        }
        // 未到点计划中的最早时刻（含 egui 计划）。
        let mut wake: Option<Instant> = ssh_poll_at;
        #[cfg(feature = "input-editor")]
        if let Some(at) = slash_cleanup_at {
            wake = Some(wake.map_or(at, |current| current.min(at)));
        }
        if state.app_lock.is_enabled()
            && !state.app_lock.is_locked()
            && state.app_lock.config().idle_timeout_minutes() > 0
        {
            wake = Some(state.next_lock_poll);
        }
        let retry_remaining = state.app_lock.retry_remaining(now);
        if !retry_remaining.is_zero() {
            let tick = now + retry_remaining.min(Duration::from_secs(1));
            wake = Some(wake.map_or(tick, |w| w.min(tick)));
        }
        // 任一窗格到点且可渲染 → 立即重绘。
        let mut fire = false;
        // 有到点但被同步区间顺延的窗格。
        let mut deferred = false;
        for s in &mut state.tabs[state.active_tab].panes {
            // 终端渲染时刻 = 静默窗口与强制刷新中先到者。
            let Some(t) = s
                .redraw_at
                .map(|soft| s.redraw_hard_at.map_or(soft, |h| soft.min(h)))
            else {
                continue;
            };
            if now < t {
                wake = Some(wake.map_or(t, |w| w.min(t)));
                continue;
            }
            // 到点但正处于同步区间：小步顺延等帧完成（ESU 通常随下一
            // 批数据立刻到达），但不超过绝对兜底时刻；欠帧已超龄（上
            // 轮被门控暂缓、又熬过了一个 REDRAW_ABS_CAP）则不再顺延。
            if s.term.is_synchronized()
                && s.redraw_abs_at.is_some_and(|a| now < a)
                && s.term_frame_due_since
                    .is_none_or(|d| now.duration_since(d) < REDRAW_ABS_CAP)
            {
                deferred = true;
                continue;
            }
            // 到点且可渲染：清计划（只清自己的，不连带其他窗格）。
            s.redraw_at = None;
            s.redraw_hard_at = None;
            s.redraw_abs_at = None;
            // 计划到点 = 欠一帧终端渲染。计划已清空，若执行重绘前新
            // 数据又拉起同步区间（abs 重新武装到未来），同步门控可暂
            // 缓这帧等 ESU 补画，但暂缓以本起点 + REDRAW_ABS_CAP 为限
            // （绝对兜底到点的强制渲染不许被无限顺延吃掉）。
            s.term_frame_due_since.get_or_insert(now);
            fire = true;
        }
        // egui 重绘计划（动画等）独立成项：到点即清并请求重绘——
        // 例外是「终端窗格全部顺延中且无其他到点窗格」时跟着顺延
        // （2ms 粒度，对 UI 动画无感），避免把半成品终端帧画上屏
        // （原单会话语义）。
        match state.egui_repaint_at {
            Some(e) if now >= e => {
                if fire || !deferred {
                    state.egui_repaint_at = None;
                    fire = true;
                }
            }
            Some(e) => wake = Some(wake.map_or(e, |w| w.min(e))),
            None => {}
        }

        if fire {
            // 重绘在途；ControlFlow 显式回 Wait（粘性的 WaitUntil(过去
            // 时刻) 会让事件循环全速空转，历史事故见 git log）。
            event_loop.set_control_flow(ControlFlow::Wait);
            state.window.request_redraw();
            return;
        }
        if deferred {
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_millis(2)));
            return;
        }
        match wake {
            Some(t) => event_loop.set_control_flow(ControlFlow::WaitUntil(t)),
            // 没有任何待渲染计划时必须显式回到 Wait：ControlFlow 是粘
            // 性的，残留的 WaitUntil(过去时刻) 会让事件循环全速空转
            // （曾导致 ESU 直渲后单核拉满、键盘处理抖动、conhost 被抢
            // CPU）。
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.tabs.is_empty() {
            return; // 退出流程中（exit 后仍可能有滞后事件）
        }

        // 非 Windows 的自动锁空闲计时兜底；Windows 实际判定优先使用
        // GetLastInputInfo，但仍同步记录，API 失败时可退回本应用事件。
        if matches!(
            &event,
            WindowEvent::KeyboardInput { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::CursorMoved { .. }
                | WindowEvent::Touch(_)
        ) {
            state.last_local_input = Instant::now();
        }

        // 应用锁快捷键属于最外层本机安全入口：在 egui/终端快捷键之前
        // 精确匹配，启用后无论当前打开设置页还是其他覆盖层都可立即锁定。
        if state.app_lock.is_enabled() && !state.app_lock.is_locked() {
            if let WindowEvent::KeyboardInput { event: key, .. } = &event {
                if key.state == ElementState::Pressed
                    && !key.repeat
                    && state
                        .app_lock
                        .config()
                        .shortcut()
                        .matches(state.modifiers, key.physical_key)
                    && state.lock_now()
                {
                    return;
                }
            }
        }

        // Ctrl+Shift+1/2/3 切换本地/远程/SSH。覆盖层或编辑态打开时
        // 不抢键；应用锁已在上方优先处理并由下方总闸隔离。
        if !state.app_lock.is_locked()
            && !state.shell_state.settings.open
            && !state.shell_state.login.open
            && !state.shell_state.history_search.open
            && (!state.shell_state.completion.open || state.shell_state.completion.passive)
            && state.shell_state.renaming.is_none()
            && state.shell_state.pane_renaming.is_none()
            && state.shell_state.ssh_session_renaming.is_none()
            && !state.shell_state.filetree.dialog_open()
            && !state.shell_state.text_editor.is_visible()
        {
            if let WindowEvent::KeyboardInput { event: key, .. } = &event {
                if key.state == ElementState::Pressed && !key.repeat {
                    if let Some(next) = view_mode_shortcut(state.modifiers, key.physical_key) {
                        if state.switch_view_mode(next) {
                            if let Some(err) = state.settings.save() {
                                state.shell_state.toast.push(
                                    shell::toast::ToastKind::Error,
                                    i18n::fmt1(
                                        i18n::strings().toast_settings_save_failed_fmt,
                                        &err,
                                    ),
                                );
                            }
                            state.window.request_redraw();
                        }
                        return;
                    }
                }
            }
        }

        // —— egui 先行消化事件 ——
        // 终端聚焦时键盘与 IME 整体绕过 egui：Tab/方向键不被 egui 的
        // 焦点导航偷走、IME 提交不被双投。其余事件先喂 egui（面板悬停
        // 高亮、按钮交互都靠它），Resized/CloseRequested 等窗口级事件
        // egui 看过后仍由我们自行处理。
        // RedrawRequested 绝不喂 egui：egui-winit 对它一律返回
        // repaint:true，照做 request_redraw 会形成「重绘请求自循环」，
        // 事件循环全速空转单核拉满（实测踩过，性质同 main.rs 历史上的
        // ControlFlow 粘性空转事故）。
        // 注：resp.consumed 在本布局下对鼠标无判别力（终端区被
        // CentralPanel 覆盖，悬停即视为「在 egui 区域上」）——鼠标按
        // 终端区矩形路由（mouse_in_term），键盘/IME 按 terminal_focused
        // 路由，不依赖 consumed。
        // 激进修复（composer Win10 IME，H1）：焦点翻转窗口期的 IME 事件，
        // 若焦点窗格 Compose 态且无覆盖层，也绕过 egui 直达 composer
        // ——否则首个 Ime::Preedit 会漏给 egui 画在默认控件位（≈最左）。
        let ime_route_composer =
            matches!(event, WindowEvent::Ime(_)) && state.ime_should_route_to_composer();
        let bypass_egui = matches!(event, WindowEvent::RedrawRequested)
            || (!state.app_lock.is_locked()
                && ((!state.settings.layout.view_mode.is_ssh() && state.terminal_focused)
                    || state.routes_input_to_ssh())
                && matches!(
                    event,
                    WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_)
                ))
            || (!state.app_lock.is_locked() && ime_route_composer)
            // 诊断开关（B1）：无交互桌面的自动化环境里物理光标不在窗口
            // 内，每个注入的 WM_MOUSEMOVE 都伴随系统补发的 WM_MOUSELEAVE
            // （TrackMouseEvent 语义），egui 的指针态被清空导致注入的
            // 按下被丢弃。设 LUMEN_DIAG_IGNORE_CURSOR_LEFT=1 时不把
            // CursorLeft 喂给 egui（仅自动化拖动测试用，正常使用不设）。
            || (matches!(event, WindowEvent::CursorLeft { .. })
                && std::env::var_os("LUMEN_DIAG_IGNORE_CURSOR_LEFT").is_some());
        // IME 诊断（composer Win10 取证，核心判据）：观察焦点翻转期首个
        // Ime::Preedit 的 bypass_egui / terminal_focused / 路由决策。坐实 H1
        // 后可移除。
        if let WindowEvent::Ime(ref ime) = event {
            log::info!(
                "[IME-RAW] {ime:?} bypass_egui={bypass_egui} tf={} route_composer={ime_route_composer}",
                state.terminal_focused
            );
        }
        if !bypass_egui {
            let resp = state.egui_state.on_window_event(&state.window, &event);
            if resp.repaint {
                // 事件驱动重绘的 8ms 合帧下限：egui-winit 对几乎一切
                // 输入事件（含 CursorMoved）都返回 repaint:true，高回报
                // 率鼠标（1000Hz）划过窗口时无脑 request_redraw 会让每
                // 个事件循环迭代渲染一帧（Mailbox 非阻塞呈现不被垂直
                // 同步限速），主线程被渲染占满、打字处理被挤——与 ESU
                // 直渲同款的退化。距上帧不足 8ms 时合入 egui_repaint_at
                // 计划，由 about_to_wait 统一调度（复用同步区间顺延与
                // ControlFlow 复位逻辑，不会空转）。
                let now = Instant::now();
                let recent = state
                    .last_render_at
                    .filter(|t| now.duration_since(*t) < Duration::from_millis(8));
                if let Some(last) = recent {
                    let at = last + Duration::from_millis(8);
                    state.egui_repaint_at = Some(state.egui_repaint_at.map_or(at, |e| e.min(at)));
                } else {
                    state.window.request_redraw();
                }
            }
        }

        // 锁定期间只允许窗口生命周期/布局事件继续进入业务分支。
        // 键鼠/IME 已在上方交给锁屏 egui，随后必须在最外层截断，绝不
        // 到达终端、文件树、设置、配对或窗口拖动等普通交互。
        if state.app_lock.is_locked()
            && !matches!(
                &event,
                WindowEvent::CloseRequested
                    | WindowEvent::Destroyed
                    | WindowEvent::Resized(_)
                    | WindowEvent::ScaleFactorChanged { .. }
                    | WindowEvent::ThemeChanged(_)
                    | WindowEvent::RedrawRequested
                    | WindowEvent::ModifiersChanged(_)
            )
        {
            return;
        }

        match event {
            WindowEvent::CloseRequested => {
                // 退出前以此刻的会话列表落盘（F4）：正常运行中每次
                // 变更已即时写盘，这里兜底拿住「最后一次变更与关窗
                // 之间」的状态（快照一致时内部自动跳过）。
                state.persist_sessions();
                // 命令历史库：原子重写磁盘（去重 + 截断到 MAX_ENTRIES）。
                // 失败只记 warn，不阻断退出。（M4.1 批D2）
                #[cfg(feature = "input-editor")]
                state.history.flush_on_exit();
                event_loop.exit();
            }
            // 外部文件拖入终端（需求3）：winit 的 DroppedFile 不携带落点
            // 坐标（平台限制），故插入**焦点窗格**的命令行（区别于文件树
            // 拖放按鼠标落点窗格）。路径转义 / Compose 分流与文件树拖放
            // 共用 insert_path_into_pane。
            WindowEvent::DroppedFile(path) => {
                if state.settings.layout.view_mode.is_ssh() {
                    return;
                }
                let idx = state.tabs[state.active_tab].focused;
                state.insert_path_into_pane(idx, &path);
                state.window.request_redraw();
            }
            WindowEvent::ModifiersChanged(mods) => state.modifiers = mods.state(),
            WindowEvent::ThemeChanged(t) => {
                // 系统深浅模式切换（P12 Sync with OS）：记录新状态；
                // 开启跟随时即时切到对应槽位主题。不写盘——设置本身
                // 没变，变的是系统侧。
                let dark = t == winit::window::Theme::Dark;
                if state.os_dark != dark {
                    state.os_dark = dark;
                    info!("系统主题切换：{}", if dark { "深色" } else { "浅色" });
                    if state.settings.appearance.sync_with_os {
                        state.apply_theme();
                        state.window.request_redraw();
                    }
                }
            }
            WindowEvent::Resized(size) => {
                state.renderer.resize_surface(size.width, size.height);
                // B3-8：整窗 resize 标志——通知下一帧 RedrawRequested
                // 穿透 divider_resize_held 门控，确保 term/PTY resize
                // 必达。整窗 resize 是 OS 级事件，与分隔条拖动无关。
                state.window_just_resized = true;
                // 终端行列数跟随 egui 布局出的终端区矩形，统一在
                // RedrawRequested 里检测变化并 resize（离屏纹理同步重建）。
                state.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // DPI 迁移（跨显示器拖动/改系统缩放）：egui-winit 已在
                // 上方消化此事件更新 pixels_per_point，渲染器侧的缩放
                // 与字体度量必须同步更新，否则终端文字物理字号永久停
                // 在启动时的 DPI（且设置页改字号也按错误 DPI 生效）。
                // 行列数重算、全会话 resize、离屏重建由下一帧
                // RedrawRequested 的矩形/网格对照检查自动完成（与设置
                // 页改字号同链路）；伴随的 Resized 事件已有分支处理
                // surface 重配。
                // B3-8：DPI 变更也是 OS 级 resize，同样需要穿透
                // divider_resize_held 门控（伴随的 Resized 一般也会置
                // 此标志，双保险无妨）。
                state.window_just_resized = true;
                state.renderer.set_scale_factor(scale_factor as f32);
                let ap = &state.settings.appearance;
                state
                    .renderer
                    .reconfigure_font(&ap.font_family, ap.font_size);
                state.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // 文件树 Ctrl+C / Ctrl+V 必须先于 SSH 终端的提前返回。
                // egui 会把 Ctrl+V 消费成文本 Paste，Windows 文件剪贴板
                // 没有文本时甚至不会留下 V 键信号，因此继续在 winit 层
                // 统一裁决三种模式。TreeView 本身会向 egui 请求键盘焦点，
                // 因此不能再用 blanket `egui_wants_keyboard_input` 反向挡掉；
                // 搜索框获得焦点时 TreeView 的 focused 会自然变为 false。
                let filetree_shortcut_available = state.filetree_focused
                    && !state.shell_state.settings.open
                    && !state.shell_state.login.open
                    && !state.shell_state.history_search.open
                    && (!state.shell_state.completion.open || state.shell_state.completion.passive)
                    && state.shell_state.renaming.is_none()
                    && state.shell_state.pane_renaming.is_none()
                    && state.shell_state.ssh_session_renaming.is_none()
                    && state.shell_state.renaming_device.is_none()
                    && !state.shell_state.filetree.dialog_open()
                    && !state.shell_state.text_editor.is_visible()
                    && state.shell_state.ssh_credentials.is_none();
                if let Some(shortcut) = filetree_clipboard_shortcut(
                    event.state,
                    event.repeat,
                    state.modifiers,
                    event.physical_key,
                    filetree_shortcut_available,
                ) {
                    match shortcut {
                        FileTreeClipboardShortcut::Copy => state.filetree_ctrl_c(),
                        FileTreeClipboardShortcut::Paste => state.filetree_ctrl_v(),
                    }
                    state.window.request_redraw();
                    return;
                }

                if state.settings.layout.view_mode.is_ssh() {
                    if event.state == ElementState::Pressed && state.routes_input_to_ssh() {
                        if let Some(bytes) = input::encode_key(&event, state.modifiers) {
                            if let Err(error) = state.ssh_runtime.send_input(bytes) {
                                log::warn!("SSH 输入发送失败: {error}");
                            } else {
                                state.last_key_at = Some(Instant::now());
                            }
                        }
                    }
                    return;
                }
                // —— M4.1 批B：事件 → keymap 查表 → Action → dispatch ——
                //
                // 原八层 if-else 拦截链已全部平移进 keymap 静态表
                // （crates/lumen-app/src/keymap.rs）。此处为「瘦身后」
                // 的入口：组装 GuardState、查表、执行结果。
                //
                // 无法入表的特例（说明为什么不入表）：
                // 1. IME：Ime::Commit / Ime::Preedit 事件走 WindowEvent::Ime
                //    分支（下方），不经过 KeyboardInput，故不在此表内。
                // 2. 重命名文本输入：重命名编辑中键盘归 egui 输入框，
                //    terminal_focused=false 的闸已拦住，keymap 中 renaming
                //    守卫只影响外壳快捷键层，无需单独入表。
                // 3. login.open 期间外壳快捷键全部静默：由 overlay_open
                //    守卫 + terminal_focused=false 联合处理，符合设计稿。

                let pressed = event.state == ElementState::Pressed;
                let plain_end_pressed = pressed
                    && state.modifiers == ModifiersState::default()
                    && matches!(
                        &event.logical_key,
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::End)
                    );
                let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);

                // 组装守卫状态（从 AppState 采样，不缓存）。
                // M5.3 part4b：镜像态（控制中+远程视图）下 Ctrl+C 的「复制 vs 中断」裁决
                // 须按**镜像选区**而非本地窗格——否则有镜像选区时 Ctrl+C 判不出选区→走
                // 中断（还把 \x03 转发给被控端），且本地窗格残留选区/块/编辑缓冲会改道。
                // 故镜像态把选择类守卫用镜像选区填、块/编辑器选区置空、compose 缓冲视作空：
                // 有镜像选区→CopySelection（dispatch 已路由复制），无→Interrupt（dispatch
                // 已路由转发 \x03）。普通字符等其余键不受影响。
                let mirror_active = state.is_mirror_active();
                let guard = keymap::GuardState {
                    has_selection: if mirror_active {
                        state.remote_ws.has_mirror_active_selection()
                    } else {
                        state.tabs[ti].panes[pi]
                            .selection
                            .as_ref()
                            .is_some_and(|s| !s.is_empty())
                    },
                    has_selected_block: !mirror_active
                        && state.tabs[ti].panes[pi].selected_block.is_some(),
                    is_alt_screen: state.tabs[ti].panes[pi].term.is_alt_screen(),
                    overlay_open: state.shell_state.settings.open
                        || state.shell_state.login.open
                        || state.shell_state.history_search.open
                        || (state.shell_state.completion.open
                            && !state.shell_state.completion.passive)
                        || state.shell_state.text_editor.is_visible(),
                    renaming: state.shell_state.renaming.is_some()
                        || state.shell_state.pane_renaming.is_some()
                        || state.shell_state.ssh_session_renaming.is_some(),
                    filetree_dialog_open: state.shell_state.filetree.dialog_open(),
                    shortcut_capture: state.shell_state.settings.is_capturing_shortcut(),
                    terminal_focused: state.terminal_focused,
                    // part4c：镜像态按**被控端**焦点窗格 win32 模式裁决（控制端转发
                    // win32 编码 + key-up）；本地态按本地窗格 + env 门控。
                    win32_input: if mirror_active {
                        state.remote_ws.mirror_win32_input()
                    } else {
                        // 新版 Claude 等开 DEC 9001 即期望 win32 格式键盘——开了就自动
                        // 启用。旧的 opt-in env 门控会让本地输入仍发 VT、被新版 Claude
                        // 拒收（输入不了）。LUMEN_NO_WIN32_INPUT 为 opt-out 逃生口。
                        state.tabs[ti].panes[pi].term.win32_input()
                            && std::env::var_os("LUMEN_NO_WIN32_INPUT").is_none()
                    },
                    // M4.1 批D1：Compose 态编辑器缓冲是否为空（影响 Ctrl+C / Ctrl+D）。
                    // 镜像态视作空：Ctrl+C 无镜像选区时落 Interrupt（转发中断）而非 CancelLine。
                    #[cfg(feature = "input-editor")]
                    compose_buf_empty: mirror_active
                        || state.tabs[ti].panes[pi].editor.view().text().is_empty(),
                    #[cfg(not(feature = "input-editor"))]
                    compose_buf_empty: true,
                    // M4.1 批D2：光标所在行位置（影响 ↑/↓ 历史导航 vs 多行移动分流）
                    #[cfg(feature = "input-editor")]
                    compose_cursor_at_first_line: {
                        let view = state.tabs[ti].panes[pi].editor.view();
                        view.cursor().line == 0
                    },
                    #[cfg(not(feature = "input-editor"))]
                    compose_cursor_at_first_line: true,
                    #[cfg(feature = "input-editor")]
                    compose_cursor_at_last_line: {
                        let view = state.tabs[ti].panes[pi].editor.view();
                        let lc = view.line_count();
                        view.cursor().line == lc.saturating_sub(1)
                    },
                    #[cfg(not(feature = "input-editor"))]
                    compose_cursor_at_last_line: true,
                    // M4.1 批3：光标在文档末尾（末行字节偏移 = 末行长度）
                    #[cfg(feature = "input-editor")]
                    compose_cursor_at_doc_end: {
                        let view = state.tabs[ti].panes[pi].editor.view();
                        let cur = view.cursor();
                        let lc = view.line_count();
                        let at_last = cur.line == lc.saturating_sub(1);
                        if at_last {
                            // 末行最后字节偏移 = 末行字节长度
                            let last_line_len = view
                                .lines()
                                .nth(lc.saturating_sub(1))
                                .map(|l| l.len())
                                .unwrap_or(0);
                            cur.byte == last_line_len
                        } else {
                            false
                        }
                    },
                    // M4.1 批3：ghost 是否非空（缓存命中时复用，否则重算）
                    #[cfg(feature = "input-editor")]
                    ghost_exists: {
                        let rev = state.tabs[ti].panes[pi].editor.revision();
                        if state.ghost_cache.0 != rev {
                            let text = state.tabs[ti].panes[pi].editor.view().text();
                            let ghost = if text.contains('\n') || text.is_empty() {
                                None
                            } else {
                                state.history.find_ghost_prefix(&text)
                            };
                            state.ghost_cache = (rev, ghost);
                        }
                        state.ghost_cache.1.is_some()
                    },
                    // 第十一轮：编辑器选区非空（Ctrl+C 第一级 / Ctrl+X 判断）。
                    // 镜像态置空：不让本地编辑器残留选区把镜像 Ctrl+C 改道成复制本地文本。
                    #[cfg(feature = "input-editor")]
                    has_editor_selection: !mirror_active
                        && state.tabs[ti].panes[pi].editor.view().has_selection(),
                };

                // 求值当前有效输入模式（纯推导，不缓存）。
                let mode = effective_session_mode(&state.tabs[ti].panes[pi], state.force_fallback);

                // 查表。M5.3 part4c：镜像态强制按非 Compose（Running）路由——否则控制端
                // 本地窗格停在自己提示符（Compose 态）时，普通字符/按键会被 keymap 第 9 层
                // 判成**本地编辑器**输入（灌进本地 composer 而非转发给被控端，且 win32
                // release 仍转发→幽灵 key-up）。Running 路由让字符/按键落 PassThrough 转发，
                // Ctrl+C/V/Shift 等仍由 guard（选区/中断/粘贴镜像感知）在层 5/10/11 正确处理。
                let lookup_mode = if mirror_active {
                    mode::InputMode::Running
                } else {
                    mode
                };
                let mut result = keymap::lookup_with_shortcuts(
                    &event,
                    state.modifiers,
                    lookup_mode,
                    pressed,
                    &guard,
                    &state.settings.keyboard,
                );
                #[cfg(feature = "input-editor")]
                if !mirror_active {
                    let pane = &state.tabs[ti].panes[pi];
                    let llm_active =
                        pane.llm_cli.is_some() || llm_cli::detect(None, &pane.term).is_some();
                    if pressed
                        && llm_cli_native_navigation_passthrough(
                            llm_active,
                            pane.editor.view().text().is_empty(),
                            state.shell_state.completion.open,
                            state.modifiers,
                            event.physical_key,
                        )
                    {
                        result = Some(keymap::LookupResult::PassThrough);
                    }
                }
                #[cfg(feature = "input-editor")]
                if pressed
                    && state.modifiers.is_empty()
                    && state.shell_state.completion.open
                    && state.shell_state.completion.passive
                    && !state.completion_candidates.is_empty()
                {
                    match event.physical_key {
                        PhysicalKey::Code(KeyCode::ArrowUp) => {
                            let count = state.completion_candidates.len();
                            let selected = &mut state.shell_state.completion.selected;
                            *selected = if *selected == 0 {
                                count - 1
                            } else {
                                *selected - 1
                            };
                            result = Some(keymap::LookupResult::Consumed);
                            state.window.request_redraw();
                        }
                        PhysicalKey::Code(KeyCode::ArrowDown) => {
                            let count = state.completion_candidates.len();
                            state.shell_state.completion.selected =
                                (state.shell_state.completion.selected + 1) % count;
                            result = Some(keymap::LookupResult::Consumed);
                            state.window.request_redraw();
                        }
                        PhysicalKey::Code(KeyCode::Escape) => {
                            state.clear_llm_slash_shadow(ti, pi);
                            result = Some(keymap::LookupResult::Consumed);
                            state.window.request_redraw();
                        }
                        PhysicalKey::Code(KeyCode::Enter)
                        | PhysicalKey::Code(KeyCode::NumpadEnter) => {
                            let idx = state
                                .shell_state
                                .completion
                                .selected
                                .min(state.completion_candidates.len() - 1);
                            let candidate = &state.completion_candidates[idx];
                            let replacement = candidate.replacement.clone();
                            let (start, end) = candidate.replace_range.unwrap_or((0, 0));
                            state.tabs[ti].panes[pi].editor.apply(
                                &lumen_editor::EditAction::SetSelection(lumen_editor::Selection {
                                    anchor: lumen_editor::Position {
                                        line: 0,
                                        byte: start,
                                    },
                                    cursor: lumen_editor::Position { line: 0, byte: end },
                                }),
                            );
                            state.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::InsertText(replacement));
                            state.close_passive_completion();
                            // `result` 保持 Enter 对应的 Submit；dispatch 会先清
                            // shadow，再提交刚接受的完整命令。
                        }
                        _ => {}
                    }
                }

                // 任意按键命中 → 清退出码角标（设计稿 §3.2 第⑥步，M4.1 批D2）。
                // 仅 Compose 态有 exit_badge；result=None（keymap 拦截）时也清，
                // 防止角标因未命中的修饰键抬起而留住。
                #[cfg(feature = "input-editor")]
                if result.is_some() {
                    state.tabs[ti].panes[pi].exit_badge = None;
                }

                match result {
                    None => {
                        // keymap 未命中（通常是 terminal_focused=false 的闸），
                        // 不写 PTY。
                    }

                    Some(keymap::LookupResult::ShellAction(shell_action)) => {
                        // 外壳级动作：不走 dispatch，直接执行外壳逻辑。
                        use keymap::ShellAction;
                        match shell_action {
                            ShellAction::NewPane => {
                                // 远程视图：新建窗格到订阅会话（远程 split），否则本地新建。
                                if state.is_mirror_active() {
                                    state.remote_ws.send_new_remote_pane();
                                } else {
                                    state.new_pane();
                                }
                            }
                            ShellAction::ClosePane => {
                                if state.is_mirror_active() {
                                    state.remote_ws.send_focused_pane_op(
                                        lumen_protocol::remote::PaneOpKind::Close,
                                    );
                                } else if state.close_pane(ti, pi) {
                                    info!("最后一个会话已关闭，退出应用");
                                    event_loop.exit();
                                }
                            }
                            ShellAction::ToggleMaximizePane => {
                                if state.is_mirror_active() {
                                    state.remote_ws.send_focused_pane_op(
                                        lumen_protocol::remote::PaneOpKind::ToggleMaximize,
                                    );
                                } else {
                                    let focused = state.tabs[state.active_tab].focused;
                                    state.toggle_maximize_pane(state.active_tab, focused);
                                }
                            }
                            ShellAction::ToggleSettings => {
                                // 登录覆盖层打开时不响应（键盘归 egui）。
                                if !state.shell_state.login.open {
                                    if state.shell_state.settings.open {
                                        state.shell_state.settings.open = false;
                                        state.terminal_focused = true;
                                    } else {
                                        state.shell_state.settings.open_with(&state.settings);
                                        state.terminal_focused = false;
                                    }
                                    state.window.request_redraw();
                                }
                            }
                            ShellAction::NewTab => {
                                // 设置页打开时不响应（避免在覆盖层背后偷偷增删）。
                                if !state.shell_state.settings.open {
                                    if state.is_mirror_active() {
                                        state.remote_ws.new_remote_tab(); // 远程新建被控端会话。
                                    } else {
                                        state.new_tab();
                                    }
                                }
                            }
                            ShellAction::CloseTab => {
                                if !state.shell_state.settings.open {
                                    if state.is_mirror_active() {
                                        if let Some(t) = state.remote_ws.subscribed_tab() {
                                            state.remote_ws.close_remote_tab(t);
                                            // 远程关订阅会话。
                                        }
                                    } else if state.close_tab(state.active_tab) {
                                        info!("最后一个会话已关闭，退出应用");
                                        event_loop.exit();
                                    }
                                }
                            }
                            ShellAction::ToggleFiletree => {
                                if !state.shell_state.settings.open {
                                    // 文件树开合：终端区宽度随之变化，下一帧
                                    // egui 布局产出新矩形并触发离屏重建+resize。
                                    let new_visible = !state.shell_state.filetree.visible;
                                    state.shell_state.filetree.visible = new_visible;
                                    // 第十九轮持久化：Ctrl+B 路径写盘，重启还原。
                                    // 与顶栏②按钮路径共用同一 settings 字段，两入口
                                    // 保持状态源一致（ShellState::filetree.visible）。
                                    state.settings.layout.filetree_visible = new_visible;
                                    if let Some(err) = state.settings.save() {
                                        state.shell_state.toast.push(
                                            shell::toast::ToastKind::Error,
                                            i18n::fmt1(
                                                i18n::strings().toast_settings_save_failed_fmt,
                                                &err,
                                            ),
                                        );
                                    }
                                    state.window.request_redraw();
                                }
                            }
                            ShellAction::CycleTab(dir) => {
                                if !state.shell_state.settings.open {
                                    state.cycle_tab(dir);
                                }
                            }
                        }
                    }

                    Some(keymap::LookupResult::Win32KeyUp) => {
                        // win32-input-mode 抬起事件：encode_key_win32(Kd=0)。part4c：镜像态
                        // 转发给被控端（与 key-down 配对），否则写本地 PTY。
                        if let Some(bytes) = input::encode_key_win32(&event, state.modifiers, false)
                        {
                            if state.is_mirror_active() {
                                state.remote_ws.send_input(&bytes);
                                state.last_key_at = Some(Instant::now());
                            } else if let Err(e) = state.tabs[ti].panes[pi].write_user_input(&bytes)
                            {
                                error!("写入 PTY 失败（win32 key-up）: {e:#}");
                            }
                        }
                    }

                    Some(keymap::LookupResult::Consumed) => {
                        // 按键已消费（如 Ctrl+Shift+C 无选区），不写 PTY。
                    }

                    Some(keymap::LookupResult::ComposeTab) => {
                        // Compose 态 Tab：M4.4 批1 文件路径补全 + 批2 命令补全。
                        #[cfg(feature = "input-editor")]
                        {
                            let ti = state.active_tab;
                            let pi = state.tabs[ti].focused;
                            if state.shell_state.completion.open
                                && state.shell_state.completion.passive
                                && !state.completion_candidates.is_empty()
                            {
                                let idx = state
                                    .shell_state
                                    .completion
                                    .selected
                                    .min(state.completion_candidates.len() - 1);
                                let candidate = &state.completion_candidates[idx];
                                let replacement = candidate.replacement.clone();
                                let (start, end) = candidate.replace_range.unwrap_or((0, 0));
                                state.tabs[ti].panes[pi].editor.apply(
                                    &lumen_editor::EditAction::SetSelection(
                                        lumen_editor::Selection {
                                            anchor: lumen_editor::Position {
                                                line: 0,
                                                byte: start,
                                            },
                                            cursor: lumen_editor::Position { line: 0, byte: end },
                                        },
                                    ),
                                );
                                state.tabs[ti].panes[pi]
                                    .editor
                                    .apply(&lumen_editor::EditAction::InsertText(replacement));
                                state.close_passive_completion();
                                state.sync_llm_slash_probe(ti, pi);
                                state.window.request_redraw();
                            } else {
                                // 取当前行文本与光标字节偏移。
                                let (line_text, cursor_byte) = {
                                    let view = state.tabs[ti].panes[pi].editor.view();
                                    let cur = view.cursor();
                                    let line = view.line(cur.line).to_owned();
                                    (line, cur.byte)
                                };
                                let cwd = state.tabs[ti].panes[pi]
                                    .term
                                    .cwd()
                                    .map(|p| p.to_path_buf())
                                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                                let (_, token) = completion::current_token(&line_text, cursor_byte);
                                let candidates = completion::complete_path(token, &cwd);

                                // 批2：计算光标的 char 偏移，发送 sidecar 命令补全请求。
                                // char 偏移 = line_text[..cursor_byte] 的 Unicode char 数。
                                let cursor_char = line_text[..cursor_byte.min(line_text.len())]
                                    .chars()
                                    .count();
                                let cwd_str = cwd.to_string_lossy();
                                let req_id = state.completion_sidecar.request(
                                    &line_text,
                                    cursor_char,
                                    &cwd_str,
                                );
                                state.completion_req_id = req_id;

                                if candidates.is_empty() {
                                    // 无文件路径候选，但命令补全可能异步到达：
                                    // 先清候选列表、打开弹窗（空状态）等待 sidecar 响应；
                                    // 若 sidecar 也无候选才降级提示。
                                    // 此处先只清旧候选，弹窗在 sidecar 响应到达后开。
                                    state.completion_candidates.clear();
                                    // 无文件路径候选时先不开弹窗（等 sidecar），但不推 toast。
                                } else {
                                    state.completion_candidates = candidates;
                                    let comp = &mut state.shell_state.completion;
                                    comp.open = true;
                                    comp.selected = 0;
                                    comp.passive = false;
                                    state.terminal_focused = false;
                                    state.window.request_redraw();
                                }
                            }
                        }
                        // 无 input-editor feature 时沿用占位提示。
                        #[cfg(not(feature = "input-editor"))]
                        {
                            let s = i18n::strings();
                            state
                                .shell_state
                                .toast
                                .push(shell::toast::ToastKind::Info, s.toast_compose_tab_hint);
                        }
                    }

                    Some(keymap::LookupResult::ComposeHistorySearch) => {
                        // Compose 态 Ctrl+R：打开历史搜索面板（M4.3）。
                        let hs = &mut state.shell_state.history_search;
                        hs.open = true;
                        hs.query.clear();
                        hs.selected = 0;
                        hs.focus_query = true;
                        // 面板打开期间键盘归 egui，不进终端。
                        state.terminal_focused = false;
                        state.window.request_redraw();
                    }

                    Some(keymap::LookupResult::ComposeEsc) => {
                        // Compose 态 Esc：关浮层 / 清选区（D1 内不清编辑器文本）。
                        // 批D1：仅清选区；浮层（历史面板等）D2 接入。
                        state.tabs[ti].panes[pi].selection = None;
                        state.window.request_redraw();
                    }

                    // M4.1 批3：接受 ghost text（→/End 在文末 + ghost 非空）
                    // 把 ghost 后缀 InsertText 进编辑器，ghost_cache 顺带失效（revision 变）。
                    #[cfg(feature = "input-editor")]
                    Some(keymap::LookupResult::AcceptGhost) => {
                        if let Some(ghost) = state.ghost_cache.1.take() {
                            state.ghost_cache.0 = 0; // 让缓存在下帧重算
                            state.dispatch(
                                action::Action::Edit(action::EditAction::InsertText(ghost)),
                                ti,
                                pi,
                            );
                            state.last_key_at = Some(Instant::now());
                        }
                    }

                    Some(keymap::LookupResult::TerminalAction(action)) => {
                        // 经由 dispatch 执行终端 Action。
                        state.dispatch(action, ti, pi);
                        // 按键记录（端到端延迟埋点）。
                        state.last_key_at = Some(Instant::now());
                    }

                    Some(keymap::LookupResult::PassThrough) => {
                        // M5.3 part4：镜像视图生效（控制中+远程视图）则把按键编码转发
                        // 给被控端、不写本地窗格（bug3：本地视图时仍是本地输入）。
                        // part4c：镜像态按被控端 win32 模式选编码（win32 则 key-down，
                        // key-up 走 Win32KeyUp 分支转发）；否则标准 encode_key。
                        if state.is_mirror_active() {
                            let bytes = if state.remote_ws.mirror_win32_input() {
                                input::encode_key_win32(&event, state.modifiers, true)
                            } else {
                                input::encode_key(&event, state.modifiers)
                            };
                            if let Some(bytes) = bytes {
                                state.remote_ws.send_input(&bytes);
                                state.last_key_at = Some(Instant::now());
                            }
                        } else {
                            // 兜底直通：encode_key / encode_key_win32 编码后写 PTY。
                            // DEC 9001 开启即自动用 win32（去掉旧 opt-in env 门控，否则
                            // 新版 Claude 开 9001 后本地仍发 VT、输入不了）。
                            // LUMEN_NO_WIN32_INPUT 为 opt-out 逃生口。
                            let use_win32 = state.tabs[ti].panes[pi].term.win32_input()
                                && std::env::var_os("LUMEN_NO_WIN32_INPUT").is_none();
                            let bytes = if use_win32 {
                                input::encode_key_win32(&event, state.modifiers, true)
                            } else {
                                input::encode_key(&event, state.modifiers)
                            };
                            if let Some(bytes) = bytes {
                                let write_t0 = Instant::now();
                                let pane = &mut state.tabs[ti].panes[pi];
                                pane.term.grid_mut().scroll_to_bottom();
                                match pane.write_user_input(&bytes) {
                                    Ok(()) if plain_end_pressed => {
                                        pane.reset_alternate_scroll_distance_hint();
                                    }
                                    Ok(()) => {}
                                    Err(e) => error!("写入 PTY 失败: {e:#}"),
                                }
                                state.last_key_at = Some(write_t0);
                                state.perf_log(format_args!(
                                    "key 写入耗时 {:?}",
                                    write_t0.elapsed()
                                ));
                            }
                        }
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                state.mouse_pos = (position.x, position.y);
                if state.settings.layout.view_mode.is_ssh() {
                    return;
                }
                // HUD resize 必须独占整段拖动；否则 CursorMoved 会同时进入终端
                // 鼠标上报或文本拖选，表现为尺寸跳动、CLI 被意外操作。
                if state.mouse_on_llm_hud() {
                    return;
                }

                // 镜像态（远程视图）：拖选进行中则更新选区终点并 return；其余
                // 镜像态移动落到下方既有逻辑（local_drag 在镜像态恒 false，最终
                // 只更新 hover，不会误触本地鼠标上报）。
                if state.is_mirror_active() {
                    // 鼠标上报开（Claude/codex 全屏）→ 移动/拖动转发给被控端，不走本地
                    // 镜像拖选。但若本次拖动按下时已归了本地镜像拖选（上报未开时起的手），
                    // 全程交给本地收尾、绝不中途被上报劫持（对称本地臂的 local_drag 闸门——
                    // 否则中途松 Shift / 被控端中途开上报会冻结镜像拖选、丢选区）。
                    let mirror_dragging = state.remote_ws.mirror_pane_selecting_sid().is_some()
                        || state.remote_ws.mirror_selecting();
                    if !mirror_dragging && state.report_mirror_mouse_motion() {
                        return;
                    }
                    // 镜像拖选边缘 auto-scroll 方向（拖到拖选窗格上/下边缘外则非 0），由
                    // about_to_wait 定时回看滚动 + 续选；回到区内则清零停滚。
                    if mirror_dragging {
                        let dir = state.autoscroll_dir_for_mirror_drag();
                        state.autoscroll_drag = dir;
                        if dir == 0 {
                            state.autoscroll_at = None;
                        } else {
                            state.window.request_redraw();
                        }
                    }
                    // 多窗格 per-pane 拖选：clamp 到拖选起始窗格矩形（不跨格）。
                    if let Some(sid) = state.remote_ws.mirror_pane_selecting_sid() {
                        if let Some((row, col)) = state.mirror_pane_cell_clamped(sid) {
                            if state.remote_ws.mirror_pane_sel_update(row, col) {
                                state.window.request_redraw();
                            }
                        }
                        return;
                    }
                    // 单窗格镜像既有拖选。
                    if state.remote_ws.mirror_selecting() {
                        if let Some((row, col)) = state.mirror_cell_clamped() {
                            if state.remote_ws.mirror_sel_update(row, col) {
                                state.window.request_redraw();
                            }
                        }
                        return;
                    }
                } else {
                    // 本地态：鼠标上报（Button 拖动 / Any 任意移动）开启时，移动
                    // 交给程序，不走本地拖选 / 链接 hover。但一旦本地拖选 / footer
                    // 拖选已经起手（按下时归了本地），本次拖动全程交给本地分支收尾，
                    // 绝不中途被上报劫持（否则跨窗格 / 中途松 Shift 会冻结、丢选区）。
                    #[allow(unused_mut)]
                    let mut local_drag = state.focused_pane().selecting;
                    #[cfg(feature = "input-editor")]
                    {
                        local_drag = local_drag || state.footer_dragging;
                    }
                    if !local_drag && state.report_mouse_motion() {
                        return;
                    }
                }

                // ── footer 拖选跟踪（第十一轮，input-editor feature）────
                #[cfg(feature = "input-editor")]
                if state.footer_dragging {
                    if let Some((rel_x, rel_y, cell_w, cell_h, fp, visual_lines)) =
                        state.mouse_footer_relative()
                    {
                        let line_refs: Vec<&str> =
                            visual_lines.iter().map(|row| row.text.as_str()).collect();
                        let visual_pos = footer_mouse::clamped_position(
                            rel_x, rel_y, cell_w, cell_h, fp, &line_refs,
                        );
                        let (line, byte) =
                            lumen_renderer::composer_view::source_position_for_wrapped(
                                &visual_lines,
                                (visual_pos.line, visual_pos.byte),
                            );
                        let cursor_pos = lumen_editor::Position { line, byte };
                        let anchor = state.footer_drag_anchor;
                        let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);
                        let old_sel = state.tabs[ti].panes[pi].editor.view().selection();
                        let new_sel = lumen_editor::Selection {
                            anchor,
                            cursor: cursor_pos,
                        };
                        if old_sel != new_sel {
                            state.dispatch(
                                action::Action::Edit(action::EditAction::SetSelection(
                                    action::Selection {
                                        anchor: action::Position {
                                            line: anchor.line,
                                            byte: anchor.byte,
                                        },
                                        head: action::Position {
                                            line: cursor_pos.line,
                                            byte: cursor_pos.byte,
                                        },
                                    },
                                )),
                                ti,
                                pi,
                            );
                        }
                    }
                    // 不再走终端拖选
                } else if state.focused_pane().selecting {
                    // 拖选边缘 auto-scroll：鼠标停在内容区上/下边缘外则置方向（非 0），
                    // 由 about_to_wait 定时滚动 + 续选；回到区内则清零停滚。
                    let dir = state.autoscroll_dir_for_drag();
                    state.autoscroll_drag = dir;
                    if dir == 0 {
                        state.autoscroll_at = None;
                    } else {
                        state.window.request_redraw(); // 唤起 about_to_wait 开始 tick
                    }
                    // 终端区拖选跟随焦点窗格：端点按窗格矩形换算（cell_at 已
                    // 夹紧，拖出窗格边界即收在边缘行列）。
                    if let Some(head) = state.sel_point_at_mouse() {
                        let mut moved = false;
                        if let Some(sel) = state.focused_pane_mut().selection.as_mut() {
                            if sel.head != head {
                                sel.head = head;
                                moved = true;
                            }
                        }
                        if moved {
                            state.window.request_redraw();
                        }
                    }
                }

                // F10：非拖选时探测鼠标下的可点击链接（更新 hover 下划线
                // 与手型光标态）。拖选/footer 拖选期间不抢。
                #[allow(unused_mut)]
                let mut busy = state.focused_pane().selecting;
                #[cfg(feature = "input-editor")]
                {
                    busy = busy || state.footer_dragging;
                }
                if state.is_mirror_active() {
                    // 镜像态：探测**镜像**窗格链接（只 URL），写同一 hover 字段。
                    // 本地 update_link_hover 只看本地窗格，镜像态调它会误清 hover。
                    state.update_mirror_link_hover();
                } else if !busy {
                    state.update_link_hover();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                if state.settings.layout.view_mode.is_ssh() {
                    return;
                }
                // F10：指针移出窗口，清除链接 hover 态（否则离屏纹理里残留
                // 一条 hover 下划线，直到该窗格下次重渲才消失）。probe 也清成
                // None，原格重入时不会因 probe 相等而跳过重新探测。
                if state.hovered_link.is_some() {
                    state.hovered_link = None;
                    state.hover_probe_cell = None;
                    state.window.request_redraw();
                } else {
                    state.hover_probe_cell = None;
                }
                // 离屏后清掉 motion 节流缓存：重入窗口的首个移动正常上报。
                state.mouse_report_last_cell = None;
                // 加固：离窗也结束上报拖动——给程序补发配对 Release 再清
                // held（正常情况下 winit 按下时捕获指针、窗外释放仍会送达，
                // 这里是双保险，且保证程序不残留幻影按住）。
                state.release_held_report_buttons();
            }
            WindowEvent::Focused(focused) => {
                if state.settings.layout.view_mode.is_ssh() {
                    if state
                        .ssh_runtime
                        .active_terminal()
                        .is_some_and(lumen_term::Terminal::focus_event)
                    {
                        let sequence = if focused {
                            b"\x1b[I".to_vec()
                        } else {
                            b"\x1b[O".to_vec()
                        };
                        let _ = state.ssh_runtime.send_input(sequence);
                    }
                    return;
                }
                // 失焦相当于交互中断：向焦点窗格补发配对 Release 再清按住态
                // 与 motion 节流缓存。winit 在失活、非自愿丢失指针捕获时不会
                // 合成按键释放——不补发则程序留下幻影按住、本地 held 卡住后
                // 纯悬停又会被误报成拖动。
                if !focused {
                    state.release_held_report_buttons();
                    state.mouse_report_last_cell = None;
                }
                // 焦点上报（DEC 1004）：窗口获/失焦时通知焦点窗格里的程序
                // （`ESC[I` = 获焦，`ESC[O` = 失焦）。未开启则不发。
                let on = state.focused_pane().term.focus_event();
                if on {
                    let seq: &[u8] = if focused { b"\x1b[I" } else { b"\x1b[O" };
                    let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);
                    if let Err(e) = state.tabs[ti].panes[pi].write_user_input(seq) {
                        log::error!("焦点上报写 PTY 失败: {e:#}");
                    }
                }
            }
            WindowEvent::MouseInput {
                state: btn_state,
                button,
                ..
            } => {
                if state.settings.layout.view_mode.is_ssh() {
                    if button == MouseButton::Left && btn_state == ElementState::Pressed {
                        let in_terminal = state.mouse_in_ssh_terminal();
                        state.terminal_focused = in_terminal && state.terminal_focus_allowed();
                        if in_terminal {
                            state.filetree_focused = false;
                            state.egui_ctx.memory_mut(|memory| {
                                if let Some(focused) = memory.focused() {
                                    memory.surrender_focus(focused);
                                }
                            });
                        } else if !state.filetree_hovered {
                            state.filetree_focused = false;
                        }
                    }
                    return;
                }
                // HUD 是终端内工具：所有鼠标键都由 egui Area 自己处理，
                // 不穿透到终端选区/鼠标上报，也不夺走终端键盘焦点。
                if state.mouse_on_llm_hud() {
                    if button == MouseButton::Left && btn_state == ElementState::Pressed {
                        state.terminal_focused = state.terminal_focus_allowed();
                    }
                    return;
                }
                match (button, btn_state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        // 无边框窗口边缘拖动 resize（左/右/下及下方两角）：命中窗口
                        // 外缘则记下方向、下一帧 RedrawRequested 内发起系统 resize 拖动
                        // （窗口操作须在该处执行，见 drag_window 注释）；本次按下不
                        // 聚焦/不建选区/不交出焦点。优先于其余命中判定（最外缘几像素）。
                        if let Some(dir) = resize_edge_dir(
                            &state.window,
                            state.mouse_pos,
                            state.egui_ctx.pixels_per_point(),
                        ) {
                            state.pending_resize_dir = Some(dir);
                            state.window.request_redraw();
                            return;
                        }
                        // M5.3 part4b：控制中+远程视图，点在镜像区内 → 起镜像拖选（作用于
                        // 显示的镜像终端），不走本地窗格选区；保持终端焦点（键盘续转发）。
                        // 但须让位本地布局的关闭✕/分隔条/侧栏拖宽手柄（它们的命中区可能落在
                        // 终端区内或左缘几像素），否则控制中无法操作这些控件、反而误起拖选。
                        if state.is_mirror_active()
                            && !state.mouse_on_pane_close()
                            && !state.mouse_on_pane_divider()
                            && !state.mouse_on_panel_resize()
                        {
                            // 鼠标上报开（Claude/codex 全屏）→ 左键按下转发给被控端，不起
                            // 本地镜像拖选（上报未开时返 false，落到下面镜像拖选）。
                            if state.report_mirror_mouse_button(MouseButton::Left, true) {
                                state.terminal_focused = true;
                                return;
                            }
                            // Phase 4 多窗格：点哪个镜像窗格 → 选它做**焦点**（输入/回看/复制/IME 目标）+
                            // 起该窗格 per-pane 拖选。单窗格镜像走既有 part4b 单选区。
                            // Shift+左键 = 范围扩展：保留现有选区锚点、把 head 续到点击处（标准
                            // 「先拖选一段、Shift+点别处扩展」语义）；无选区则等价新建。
                            let shift = state.modifiers.shift_key();
                            if !state.remote_ws.mirror_panes().is_empty() {
                                if let Some((sid, row, col)) = state.mirror_pane_cell_at_mouse() {
                                    state.terminal_focused = true;
                                    state.remote_ws.set_mirror_active_pane(sid);
                                    if shift {
                                        state.remote_ws.mirror_pane_sel_extend(sid, row, col);
                                    } else {
                                        state.remote_ws.mirror_pane_sel_start(sid, row, col);
                                    }
                                    state.window.request_redraw();
                                    return;
                                }
                            } else if let Some((row, col)) = state.mirror_cell_at_mouse() {
                                state.terminal_focused = true;
                                if shift {
                                    state.remote_ws.mirror_sel_extend(row, col);
                                } else {
                                    state.remote_ws.mirror_sel_start(row, col);
                                }
                                state.window.request_redraw();
                                return;
                            }
                        }
                        // 点的是窗格关闭按钮：动作由 egui 侧处理（✕ →
                        // pane_close），这里不聚焦不建选区，也不视作
                        // 「点击面板交出焦点」——关完接着打字不该断流。
                        if state.mouse_on_pane_close() {
                            return;
                        }
                        // 按在分隔条上：拖动调比例由 egui 侧处理（F7③，
                        // divider_drag），这里不聚焦/不建选区，也不交出
                        // 终端焦点——调完比例接着打字不该断流。
                        if state.mouse_on_pane_divider() {
                            return;
                        }
                        // 按在侧栏/文件树栏的拖宽手柄上（P10）：拖宽由
                        // egui 面板处理，这里同样不聚焦/不建选区/不交出
                        // 终端焦点——调完宽度接着打字不该断流。
                        if state.mouse_on_panel_resize() {
                            return;
                        }
                        // 焦点仲裁（F5）：点击窗格聚焦该窗格 + 终端拿键盘/
                        // IME 焦点；点击 egui 面板交出焦点（路由随之切换）。
                        let Some(pi) = state.pane_under_mouse() else {
                            state.terminal_focused = false;
                            if !state.filetree_hovered {
                                state.filetree_focused = false;
                            }
                            return;
                        };
                        state.terminal_focused = true;
                        state.filetree_focused = false;
                        state.egui_ctx.memory_mut(|memory| {
                            if let Some(focused) = memory.focused() {
                                memory.surrender_focus(focused);
                            }
                        });
                        state.focus_pane(pi);

                        // ── footer 区域分流（第十一轮，input-editor feature）─
                        // Compose/可见态下点击 footer 区域时，不建终端选区，
                        // 转入编辑器鼠标处理路径。键盘续走编辑器（terminal_focused=true 保持）。
                        #[cfg(feature = "input-editor")]
                        if state.mouse_on_footer() {
                            if let Some((rel_x, rel_y, cell_w, cell_h, fp, visual_lines)) =
                                state.mouse_footer_relative()
                            {
                                let line_refs: Vec<&str> = visual_lines
                                    .iter()
                                    .map(|row| row.text.as_str())
                                    .collect();
                                // 像素 → 编辑器位置
                                let visual_pos = footer_mouse::pixel_to_position(
                                    rel_x, rel_y, cell_w, cell_h, fp, &line_refs,
                                );
                                let (line, byte) =
                                    lumen_renderer::composer_view::source_position_for_wrapped(
                                        &visual_lines,
                                        (visual_pos.line, visual_pos.byte),
                                    );
                                let pos = lumen_editor::Position { line, byte };
                                // 显示列（用于 click-count 位移检测）
                                let display_col = (rel_x / cell_w.max(1.0)).floor() as usize;
                                let row = visual_pos.line;
                                let kind = state.footer_click_state.record_click(
                                    row,
                                    display_col,
                                    std::time::Instant::now(),
                                );

                                let (ti, pi) =
                                    (state.active_tab, state.tabs[state.active_tab].focused);

                                let action = match kind {
                                    footer_mouse::ClickKind::Single => {
                                        let shift = state.modifiers.shift_key();
                                        let cur_anchor = state.tabs[ti].panes[pi]
                                            .editor
                                            .view()
                                            .selection()
                                            .anchor;
                                        footer_mouse::single_click_action(pos, shift, cur_anchor)
                                    }
                                    footer_mouse::ClickKind::Double => {
                                        let line_text = state.tabs[ti].panes[pi]
                                            .editor
                                            .view()
                                            .lines()
                                            .nth(pos.line)
                                            .unwrap_or("")
                                            .to_owned();
                                        let sel = footer_mouse::word_selection(pos, &line_text);
                                        lumen_editor::EditAction::SetSelection(sel)
                                    }
                                    footer_mouse::ClickKind::Triple => {
                                        let line_text = state.tabs[ti].panes[pi]
                                            .editor
                                            .view()
                                            .lines()
                                            .nth(pos.line)
                                            .unwrap_or("")
                                            .to_owned();
                                        let sel = footer_mouse::line_selection(pos, &line_text);
                                        lumen_editor::EditAction::SetSelection(sel)
                                    }
                                };

                                // 将 lumen_editor::EditAction 包装为 app 层 Action
                                // 单击时记录锚点（拖选用）
                                let app_action = lumen_editor_action_to_app_action(action);
                                state.dispatch(app_action, ti, pi);

                                // 记录拖选锚点（单击/双击/三击都可能继续拖）
                                let new_anchor =
                                    state.tabs[ti].panes[pi].editor.view().selection().anchor;
                                state.footer_drag_anchor = new_anchor;
                                state.footer_dragging = true;
                            }
                            return;
                        }

                        // 鼠标上报开启（且非 Shift）：本次按下交给程序处理，不建本地
                        // 选区（已在上面完成聚焦该窗格）。显式 !is_mirror_active() 守卫：
                        // 镜像态恒不触发本地上报，与左右键 Released / 中键四处一致——镜像
                        // 态点击在上面镜像分支处理；若镜像未命中（标题栏/窗格间隙/几何
                        // 错位）也绝不穿透写本地 PTY、卡住单边 held。
                        if !state.is_mirror_active()
                            && state.report_mouse_button(MouseButton::Left, true)
                        {
                            return;
                        }

                        // 选区在点中的窗格（即新焦点窗格）建立。
                        let Some(p) = state.sel_point_at_mouse() else {
                            return;
                        };
                        let pane_id = state.focused_pane().id;
                        // 新拖选起手：复位边缘 auto-scroll 方向，绝不继承上次拖选的陈旧值。
                        state.autoscroll_drag = 0;
                        state.autoscroll_at = None;
                        // Shift+左键 = 范围快选：从上次（本窗格）普通左键点位扩展选区到此处、
                        // 保留锚点。仅当三条都满足才扩展：① **非鼠标上报终端**——上报态（Claude
                        // 全屏）Shift 是「逃生到本地选区」、应按普通拖选以按下点为锚，不做范围
                        // 扩展（否则第二次拖选锚点错乱）；② 记忆点位是本窗格的；③ 锚点绝对行仍落在
                        // 当前 grid 有效区间内（跨备用屏 / 主屏切换、或滚出 scrollback 则失效，避免
                        // 坐标系串台高亮错范围）。否则退化为新建。普通左键 = 新建空选区并记锚点。
                        let reporting = state.focused_pane().term.mouse_protocol().is_on();
                        let prev = state.last_left_click.filter(|&(id, a)| {
                            id == pane_id
                                && state
                                    .focused_pane()
                                    .term
                                    .grid()
                                    .line_by_abs(a.line)
                                    .is_some()
                        });
                        let shift_extend =
                            !reporting && state.modifiers.shift_key() && prev.is_some();
                        let anchor = if shift_extend {
                            prev.map_or(p, |(_, a)| a)
                        } else {
                            p
                        };
                        {
                            let s = state.focused_pane_mut();
                            s.selecting = true;
                            s.selection = Some(Selection { anchor, head: p });
                            // 范围扩展：清掉单击锚点时选中的命令块，避免块高亮与文本选区并存、
                            // 或复制取了块而非选区文本。
                            if shift_extend {
                                s.selected_block = None;
                            }
                        }
                        // 仅普通点击更新记忆锚点（Shift 扩展保持原锚点供连续扩展）。
                        if !shift_extend {
                            state.last_left_click = Some((pane_id, p));
                        }
                        state.window.request_redraw();
                    }
                    (MouseButton::Left, ElementState::Released) => {
                        // footer 拖选结束（input-editor feature）。
                        #[cfg(feature = "input-editor")]
                        if state.footer_dragging {
                            state.footer_dragging = false;
                            return;
                        }

                        // 镜像态：鼠标上报开 → 左键释放编码转发给被控端（与按下配对，
                        // 上报未开 / 该键未上报按住时返 false，落到下面镜像拖选收尾）。
                        if state.is_mirror_active()
                            && state.report_mirror_mouse_button(MouseButton::Left, false)
                        {
                            return;
                        }
                        // F10：镜像态 Ctrl+Click 落在 URL 链接上 → 本地浏览器打开（**只放
                        // URL**，见 mirror_link_at_mouse）。Ctrl 让位后释放落到这里
                        // （report_mirror_mouse_button 对未 held 键返 false）。先于
                        // copy-on-select / sel_end：命中即开、并清掉起手的空拖选。
                        // `!has_mirror_active_selection()`：只在空选区（未拖动）时开，与本地
                        // `if selection.is_empty()` 对称——Ctrl+拖出一段非空选区不误开链接。
                        if state.is_mirror_active()
                            && state.modifiers.control_key()
                            && !state.remote_ws.has_mirror_active_selection()
                        {
                            if let Some(link) = state.mirror_link_at_mouse() {
                                log::info!("F10：镜像 Ctrl+Click 打开链接 {:?}", link.target);
                                links::open(&link.target);
                                if state.remote_ws.mirror_pane_selecting() {
                                    state.remote_ws.mirror_pane_sel_end();
                                }
                                state.window.request_redraw();
                                return;
                            }
                        }
                        // 镜像 Shift+拖选松手**不自动复制**（海风哥 2026-07 要求去掉 copy-on-select）：
                        // 只选中、保留高亮，复制交给 Ctrl+C / 右键。
                        // 镜像态拖选结束（空选区=仅点击则清掉）；多窗格 per-pane / 单窗格各一路。
                        if state.is_mirror_active() && state.remote_ws.mirror_pane_selecting() {
                            state.remote_ws.mirror_pane_sel_end();
                            state.autoscroll_drag = 0;
                            state.autoscroll_at = None;
                            state.window.request_redraw();
                            return;
                        }
                        if state.is_mirror_active() && state.remote_ws.mirror_selecting() {
                            state.remote_ws.mirror_sel_end();
                            state.autoscroll_drag = 0;
                            state.autoscroll_at = None;
                            state.window.request_redraw();
                            return;
                        }
                        // 本地态：鼠标上报开启（且非 Shift）→ 把释放编码发给程序。
                        if !state.is_mirror_active()
                            && state.report_mouse_button(MouseButton::Left, false)
                        {
                            return;
                        }

                        // 本次按下不在窗格上（点的是 egui 面板）则与终端无关。
                        if !state.focused_pane().selecting {
                            return;
                        }
                        state.focused_pane_mut().selecting = false;
                        // 拖选结束：停掉边缘 auto-scroll。
                        state.autoscroll_drag = 0;
                        state.autoscroll_at = None;
                        // Shift+拖选松手**不自动复制**（海风哥 2026-07 要求去掉 copy-on-select）：
                        // 只选中、保留高亮，复制交给 Ctrl+C / 右键。普通单击(空选区)才清块/开链接。
                        if state.focused_pane().selection.is_some_and(|s| s.is_empty()) {
                            // F10：**Ctrl+单击**落在可点击链接上 → 用系统默认
                            // 程序/浏览器打开（对齐 VSCode 终端 Ctrl+Click 惯例）。
                            // 普通单击保持「选中/清除命令块」，不误触开链接
                            // （海风哥反馈：只 click 就开体验不好）。
                            if state.modifiers.control_key() {
                                if let Some(link) = state.link_at_mouse() {
                                    log::info!("F10：Ctrl+Click 打开链接 {:?}", link.target);
                                    links::open(&link.target);
                                    state.focused_pane_mut().selection = None;
                                    state.window.request_redraw();
                                    return;
                                }
                            }
                            // 单击（未拖动）：选中/清除所在命令块。
                            // 备用屏幕下块行号坐标系不可用，不做块选中。
                            let p = state.sel_point_at_mouse();
                            let s = state.focused_pane_mut();
                            s.selection = None;
                            if let Some(p) = p {
                                if !s.term.is_alt_screen() {
                                    let hit = s.term.block_at_line(p.line).map(|b| b.id);
                                    s.selected_block =
                                        if hit == s.selected_block { None } else { hit };
                                }
                            }
                            state.window.request_redraw();
                        }
                    }
                    (MouseButton::Right, ElementState::Pressed) => {
                        // 镜像态**有非空选区 → 右键优先本地复制**，抢在 report_mirror 转发之前
                        // （对齐本地右键 7046：修 Claude 等全屏 TUI 里右键被鼠标上报吃掉、下面
                        // 复制那条路根本走不到——「镜像里选中却复制不了」的直接成因）。仅命中镜像
                        // 区时拦截（与下方粘贴同门控、与本地「右键须在终端区」对称）。写剪贴板成功
                        // → 清选区 + 弹「已复制」toast；失败/不可用则保留选区便于重试。
                        if state.is_mirror_active()
                            && state.remote_ws.has_mirror_active_selection()
                            && (state.mirror_pane_at_mouse().is_some()
                                || state.mirror_cell_at_mouse().is_some())
                        {
                            if let Some(text) = state.remote_ws.copy_mirror_active() {
                                match state.clipboard.as_mut().map(|c| c.set_text(text.clone())) {
                                    Some(Ok(())) => {
                                        state.remote_ws.clear_mirror_active_selection();
                                        state.show_copied_toast(&text);
                                    }
                                    Some(Err(e)) => error!("写剪贴板失败: {e}"),
                                    None => log::warn!("剪贴板不可用，复制跳过"),
                                }
                            }
                            state.window.request_redraw();
                            return;
                        }
                        // 无选区：鼠标上报开（Claude/codex 全屏，程序可能用右键弹自己的菜单）
                        // → 右键按下转发给被控端，不走本地粘贴（上报未开返 false，落到下面
                        // 镜像右键粘贴）。
                        if state.is_mirror_active()
                            && state.report_mirror_mouse_button(MouseButton::Right, true)
                        {
                            return;
                        }
                        // M5.3 part4b 镜像右键无选区（上报未开）→ 粘贴转发给被控端（沿用本地
                        // 终端右键惯例）。仅命中镜像区时拦截。
                        if state.is_mirror_active()
                            && (state.mirror_pane_at_mouse().is_some()
                                || state.mirror_cell_at_mouse().is_some())
                        {
                            if let Some(Ok(text)) = state.clipboard.as_mut().map(|c| c.get_text()) {
                                state.remote_ws.send_paste(&text);
                            }
                            state.window.request_redraw();
                            return;
                        }
                        // 右键也按「点击窗格聚焦」仲裁（F5）：复制/粘贴作用
                        // 于点中的窗格。
                        let Some(pidx) = state.pane_under_mouse() else {
                            return;
                        };
                        state.focus_pane(pidx);
                        state.terminal_focused = true;

                        // ── footer 区域右键：弹出编辑器上下文菜单（第十一轮）─
                        #[cfg(feature = "input-editor")]
                        if state.mouse_on_footer() {
                            // 记录弹出位置，egui 帧内渲染菜单（见 RedrawRequested 处理）
                            state.footer_context_menu_at = Some(state.mouse_pos);
                            state.window.request_redraw();
                            return;
                        }

                        // 右键（终端区，Windows Terminal 惯例）。字段级下标：clipboard 需同时可变借用。
                        let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);
                        // **有非空选区 → 右键优先本地复制**，哪怕鼠标上报开启（修 Claude 等全屏 TUI
                        // 里右键被上报吃掉、下面的复制那条路根本走不到——正是「Claude 里选中却复制不了」
                        // 的直接成因）。复制成功清选区 + 弹「已复制」toast。
                        if state.tabs[ti].panes[pi]
                            .selection
                            .is_some_and(|s| !s.is_empty())
                        {
                            if let Some(text) =
                                state.tabs[ti].panes[pi].copy_selection(&mut state.clipboard)
                            {
                                state.tabs[ti].panes[pi].selection = None;
                                state.show_copied_toast(&text);
                            }
                            state.window.request_redraw();
                            return;
                        }
                        // 无选区 + 鼠标上报开启（非镜像）→ 右键交给程序（其自有右键菜单）。
                        if !state.is_mirror_active()
                            && state.report_mouse_button(MouseButton::Right, true)
                        {
                            return;
                        }
                        // 无选区 → 粘贴（Windows Terminal 惯例）。
                        state.tabs[ti].panes[pi].paste_clipboard(&mut state.clipboard);
                    }
                    (MouseButton::Right, ElementState::Released) if !state.is_mirror_active() => {
                        // 镜像态不走本地上报（远程右键在 Pressed 已处理并 return；
                        // 镜像态此分支不匹配，落到 `_ => {}`）。
                        state.report_mouse_button(MouseButton::Right, false);
                    }
                    (MouseButton::Middle, ElementState::Pressed) if !state.is_mirror_active() => {
                        // 镜像态（远程视图）中键不处理（落到 `_ => {}`）；本地态与左/右
                        // 键一致先做焦点仲裁（F5）再上报，否则中键上报会写给非焦点窗格、
                        // 且释放回退焦点窗格时与按下目标对不齐（留下幻影按住）。点在
                        // egui 面板上则不聚焦。
                        if let Some(pidx) = state.pane_under_mouse() {
                            state.focus_pane(pidx);
                            state.terminal_focused = true;
                        }
                        state.report_mouse_button(MouseButton::Middle, true);
                    }
                    (MouseButton::Middle, ElementState::Released) if !state.is_mirror_active() => {
                        state.report_mouse_button(MouseButton::Middle, false);
                    }
                    // 镜像态（控制端）：上报开时转发右键释放 / 中键按下·释放给被控端，与
                    // 各自按下配对（上报未开 / 该键未上报按住时 report_mirror 返 false、无
                    // 副作用——镜像右键复制粘贴已在 Right Pressed 处理）。
                    (MouseButton::Right, ElementState::Released) if state.is_mirror_active() => {
                        state.report_mirror_mouse_button(MouseButton::Right, false);
                    }
                    (MouseButton::Middle, ElementState::Pressed) if state.is_mirror_active() => {
                        state.report_mirror_mouse_button(MouseButton::Middle, true);
                    }
                    (MouseButton::Middle, ElementState::Released) if state.is_mirror_active() => {
                        state.report_mirror_mouse_button(MouseButton::Middle, false);
                    }
                    _ => {}
                }
            }
            // IME 组合开始（焦点失而复得后的首个组合串关键）：立即把候选框
            // 钉到焦点窗格光标，**别等下一帧 RedrawRequested**——否则首字组合串
            // 会用 egui 残留的左上角位置画在最左、且 OS 自绘组合串成孤儿删不掉
            // （Win10「窗口/tab/窗格失焦再回来打字首字缩最左」真因；WT/Warp 无此
            // 问题，是 Lumen 焦点回来未及时复位 IME 候选框所致）。
            WindowEvent::Ime(Ime::Enabled) => {
                state.update_ime_cursor_area(true);
            }
            // IME 预编辑（M4.1 批D2，设计稿 §7.3）：
            // Compose 态：更新 session.preedit（不进编辑器文档，不参与 undo）。
            // text 为空或 cursor_range 为 None + 空串 → 清空预编辑（预编辑取消）。
            // 其余态：事件本身已由 egui-winit 处理（路由已交 egui），此处忽略。
            WindowEvent::Ime(Ime::Preedit(text, cursor)) => {
                if state.settings.layout.view_mode.is_ssh() {
                    // The platform IME owns composition; only Commit is sent
                    // to the remote UTF-8 terminal.
                    return;
                }
                // 激进修复（composer Win10）：焦点翻转期首字也归 composer。
                let route = state.ime_should_route_to_composer();
                log::info!(
                    "[IME-PREEDIT] text={text:?} cursor={cursor:?} tf={} route_composer={route} \
                     will_drop={}",
                    state.terminal_focused,
                    !state.terminal_focused && !route
                );
                if !state.terminal_focused && !route {
                    return;
                }
                // M5.3 part4c：镜像态不把 preedit 灌进本地编辑器——组合由本地 OS IME 负责
                // （候选框已定位到被控端光标处），仅在 Commit 时转发提交文本给被控端。
                if state.is_mirror_active() {
                    return;
                }
                let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);
                #[cfg(feature = "input-editor")]
                {
                    let mode =
                        effective_session_mode(&state.tabs[ti].panes[pi], state.force_fallback);
                    if mode == mode::InputMode::Compose {
                        if text.is_empty() {
                            // 空串 = 预编辑结束/取消
                            state.tabs[ti].panes[pi].preedit = None;
                        } else {
                            state.tabs[ti].panes[pi].preedit =
                                Some(lumen_renderer::composer_view::PreeditState {
                                    text,
                                    cursor_range: cursor,
                                });
                        }
                        state.window.request_redraw();
                        return;
                    }
                }
                // 非 Compose 态或 feature 未开启：丢弃（PTY 终端自行处理 IME）。
                let _ = (text, cursor);
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                if state.settings.layout.view_mode.is_ssh() {
                    if state.routes_input_to_ssh() {
                        if let Err(error) = state.ssh_runtime.send_input(text.into_bytes()) {
                            log::warn!("SSH IME 文本发送失败: {error}");
                        } else {
                            state.last_key_at = Some(Instant::now());
                        }
                    }
                    return;
                }
                // 仅终端聚焦时把 IME 提交文本写入 shell（焦点窗格）；
                // egui 输入框聚焦时事件已喂给 egui 消化，再写 PTY 就是
                // 双投。激进修复（composer Win10）：焦点翻转期 Compose 态
                // 也放行，让 preedit 直达 composer 后的 commit 不丢。
                let route = state.ime_should_route_to_composer();
                log::info!(
                    "[IME-COMMIT] text={text:?} tf={} route_composer={route}",
                    state.terminal_focused
                );
                if !state.terminal_focused && !route {
                    return;
                }
                // M5.3 part4c：镜像态把 IME 提交文本（中文等）转发给被控端 PTY，不写本地、
                // 不进本地编辑器（控制端本地 IME 仅负责候选/组合，提交即转发）。
                if state.is_mirror_active() {
                    state.remote_ws.send_input(text.as_bytes());
                    state.last_key_at = Some(Instant::now());
                    return;
                }
                // M4.1 批D1：IME 分流——设计稿 §7.3
                // Compose 态：提交文本进编辑器（InsertText），不写 PTY。
                // 其余态：与按键路径一致，回滚到底部后写 PTY。
                let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);
                #[cfg(feature = "input-editor")]
                {
                    let mode =
                        effective_session_mode(&state.tabs[ti].panes[pi], state.force_fallback);
                    if mode == mode::InputMode::Compose {
                        // 提交时清空 preedit（M4.1 批D2）
                        state.tabs[ti].panes[pi].preedit = None;
                        // IME 提交进编辑器（走 dispatch 确保门控逻辑一致）
                        state.dispatch(
                            action::Action::Edit(action::EditAction::InsertText(text)),
                            ti,
                            pi,
                        );
                        return;
                    }
                }
                // 非 Compose 态（含 feature 未开启）：直通 PTY
                // 与按键路径一致：输入即回滚到底部——翻看历史时提交
                // 中文，视图不跳回底部会看不到自己的回显。
                let s = state.tabs[ti].panes[pi].term.grid_mut();
                s.scroll_to_bottom();
                let s = state.focused_pane_mut();
                if let Err(e) = s.write_user_input(text.as_bytes()) {
                    error!("写入 PTY 失败: {e:#}");
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                #[cfg(feature = "input-editor")]
                if state.shell_state.completion.open
                    && state
                        .shell_state
                        .completion
                        .popup_rect
                        .zip(state.egui_ctx.pointer_latest_pos())
                        .is_some_and(|(rect, pointer)| rect.contains(pointer))
                {
                    // egui_state 已在上方收到本次滚轮；命中补全弹层时到此
                    // 截断，避免同一事件继续滚动底下的终端。
                    state.window.request_redraw();
                    return;
                }
                if state.settings.layout.view_mode.is_ssh() {
                    if !state.mouse_in_ssh_terminal() {
                        return;
                    }
                    let lines = match delta {
                        MouseScrollDelta::LineDelta(_, y) => (y * 3.0) as isize,
                        MouseScrollDelta::PixelDelta(position) => {
                            (position.y / f64::from(state.renderer.cell_size().1)) as isize
                        }
                    };
                    if state.ssh_runtime.scroll_active(lines) {
                        state.window.request_redraw();
                    }
                    return;
                }
                // 终端窗格区内滚轮归终端，区外（侧栏等）归 egui；滚动
                // 作用于**焦点窗格**（F5 拍板：键盘/IME/滚轮/选区全部
                // 跟焦点，悬停别的窗格不抢路由——要滚哪个先点哪个）。
                //
                // 指针可能正悬在滚动条轨道（Foreground 层）上，此时
                // pane_under_mouse 因命中 egui 层返回 None——但用户意图仍是
                // 滚终端，故补一判：在轨道上也放行，避免右缘整列成滚轮死区。
                // 命中判定：镜像态查**镜像窗格**矩形（mirror_pane_at_mouse，海风哥反馈③：旧逻辑用
                // pane_under_mouse 只认本地窗格、镜像态恒 None → 滚轮被吃、scroll_mirror 没被调）；
                // 本地态查 pane_under_mouse。轨道上也放行（右缘不成死区）。
                let over_term = if state.is_mirror_active() {
                    state.mirror_pane_at_mouse().is_some()
                } else {
                    state.pane_under_mouse().is_some()
                };
                if !over_term && !state.mouse_on_scrollbar_track() {
                    return;
                }
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y * 3.0) as isize,
                    MouseScrollDelta::PixelDelta(p) => {
                        (p.y / state.renderer.cell_size().1 as f64) as isize
                    }
                };
                if lines == 0 {
                    return;
                }
                // 镜像态（远程视图）控制端滚轮：按**被控端焦点会话的终端模式**
                // 路由（与本地态对称，不按程序名特判）：
                //   - 开了鼠标上报（如 Claude 全屏 ?1003h）→ 把滚轮编码成鼠标
                //     上报，`send_input` 转发给被控端 PTY，程序自己滚、重绘同步两端；
                //   - 备用屏开启 Alternate Scroll（如 Codex transcript 的 ?1007h）→
                //     转成光标上/下键转发；
                //   - 没开（PowerShell / inline）→ 滚控制端**本地镜像 scrollback** 回看，
                //     不转发、不碰被控端（各端各看各的历史，原设计语义）。
                // Shift+滚轮强制本地回看（逃生通道）。坐标取镜像窗格内单元格、目标
                // 会话即鼠标所在镜像窗格（上面 set_mirror_active_pane → send_input 定位）。
                // 修正既有 bug：原无条件 scroll_mirror、从不转发，致 Claude 全屏时控制端
                // 滚轮被吞、本地镜像又无 scrollback 可滚 → 看着无效（海风哥 2026-06-30）。
                if state.is_mirror_active() {
                    let cell = state.mirror_pane_cell_at_mouse();
                    if let Some((sid, _, _)) = cell {
                        state.remote_ws.set_mirror_active_pane(sid);
                    }
                    let shift = state.modifiers.shift_key();
                    let mods = state.mouse_mods();
                    let up = lines > 0;
                    let notches = lines.unsigned_abs().div_ceil(3).max(1);
                    let forward: Option<Vec<u8>> = match cell {
                        Some((sid, row, col)) if !shift => state
                            .remote_ws
                            .mirror_panes()
                            .iter()
                            .find(|p| p.session_id == sid)
                            .and_then(|mp| {
                                let proto = mp.term.mouse_protocol();
                                if proto.is_on() {
                                    let enc = mp.term.mouse_encoding();
                                    let kind = if up {
                                        MouseEventKind::WheelUp
                                    } else {
                                        MouseEventKind::WheelDown
                                    };
                                    let mut buf = Vec::new();
                                    for _ in 0..notches {
                                        if let Some(b) = encode_mouse(
                                            proto,
                                            enc,
                                            MouseEvent {
                                                kind,
                                                col,
                                                row,
                                                mods,
                                            },
                                        ) {
                                            buf.extend_from_slice(&b);
                                        }
                                    }
                                    return (!buf.is_empty()).then_some(buf);
                                }
                                (mp.term.is_alt_screen() && mp.term.alternate_scroll())
                                    .then(|| input::encode_alternate_scroll(up, notches))
                            }),
                        _ => None,
                    };
                    if let Some(buf) = forward {
                        state.remote_ws.send_input(&buf);
                    } else {
                        state.remote_ws.scroll_mirror(lines);
                    }
                    state.window.request_redraw();
                    return;
                }
                // 鼠标上报开启（如 Claude 的全屏 TUI）时，滚轮交给程序：编码成
                // SGR/X10 鼠标按钮 64(上)/65(下)写 PTY，程序自己滚它的视口——
                // 不再滚本地 scrollback。否则，备用屏的 DECSET 1007（如 Codex
                // transcript）把滚轮转成方向键；最后才回退本地 scrollback。
                // Shift+滚轮始终强制本地（逃生通道）。
                let up = lines > 0;
                let notches = lines.unsigned_abs().div_ceil(3).max(1);
                if state.report_mouse_wheel(up, notches)
                    || state.report_alternate_scroll_wheel(up, notches)
                {
                    return;
                }
                state
                    .focused_pane_mut()
                    .term
                    .grid_mut()
                    .scroll_display(lines);
                state.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                // 问题4（B4 修复）：无边框窗口最小化时 winit inner_size
                // 缩为约 160×28 小条（非 0×0，绕过原有 0 尺寸守卫），
                // egui 布局与 PTY resize 会以此极小尺寸执行，导致
                // layout.rs 的 clamp 产生 max < min panic，进而在
                // wgpu swapchain 释放时触发次生 panic。
                // 守卫：最小化态 (is_minimized == true) 或宽/高 < 120
                // 物理像素（160×28 小条实测值）时跳过整帧渲染与布局。
                let locked = state.app_lock.is_locked();
                let sz = state.window.inner_size();
                const MIN_RENDERABLE: u32 = 120;
                let too_small = sz.width < MIN_RENDERABLE || sz.height < MIN_RENDERABLE;
                let minimized = state.window.is_minimized().unwrap_or(false);
                if minimized || sz.width == 0 || sz.height == 0 || (too_small && !locked) {
                    // 锁定态即便最小化不重画，窗口捕获保护也已同步开启，
                    // DWM 缩略图不会继续暴露锁前 surface。
                    return;
                }
                if locked {
                    let Some(lock_out) = state.render_app_lock() else {
                        // Surface Lost/Outdated 的恢复不能依赖偶发后续事件；
                        // 锁屏是安全帧，失败后主动补排，直到覆盖旧业务帧。
                        state.egui_repaint_at = Some(Instant::now() + Duration::from_millis(16));
                        state.window.request_redraw();
                        return;
                    };
                    if lock_out.close {
                        state.persist_sessions();
                        #[cfg(feature = "input-editor")]
                        state.history.flush_on_exit();
                        event_loop.exit();
                        return;
                    }
                    if lock_out.minimize {
                        state.window.set_minimized(true);
                    }
                    if let Some(mut password) = lock_out.unlock_password {
                        if !state.app_lock.crypto_busy()
                            && state.app_lock.retry_remaining(Instant::now()).is_zero()
                            && !state.app_lock.config().storage_error()
                        {
                            if let Some(verifier) = state.app_lock.protected_verifier() {
                                state.spawn_lock_crypto(
                                    app_lock::CryptoRequest::unlock(password, verifier),
                                    true,
                                );
                            } else {
                                use zeroize::Zeroize as _;
                                password.zeroize();
                                state.app_lock.mark_storage_error();
                                state.prepare_locked_ui();
                            }
                        } else {
                            // 防御：UI 在 busy/退避/存储错误时不会产出密码，
                            // 若陈旧事件仍到达也先覆写明文再丢弃。
                            use zeroize::Zeroize as _;
                            password.zeroize();
                        }
                    }
                    return;
                }
                // surface 帧先行取得：失败（Lost/Outdated 已就地重配）则
                // 本帧整体跳过——egui 输入与 textures_delta 都未消费，
                // 状态不丢，等下一次重绘。
                let Some(frame) = state.renderer.acquire_frame() else {
                    return;
                };
                let render_t0 = Instant::now();
                state.probe_llm_clis(render_t0);

                // —— DEC 2026 同步区间门控（事件驱动重绘的保护层，F5
                // 起**逐窗格**判定）——
                // M3 起鼠标划过窗口等任意 egui repaint 都会触发本处理器，
                // 而 BSU..ESU 之间 grid 是边收边改的半成品（光标游走、
                // 未画完的行）——静默合帧/小步顺延只管**定时调度**路径，
                // 管不住事件驱动的 request_redraw。此处兜底：同步区间内
                // 且渲染计划在途（abs 兜底未到点）的窗格，跳过其终端离
                // 屏渲染——egui 照常布局合成（悬停高亮不受影响），该
                // 窗格纹理保留上一完整帧，ESU 到达后由快路/计划补画；
                // 其余窗格照常渲染（逐窗格门控互不连坐）。跳过时也不动
                // take_dirty 与光标冻结状态（属于「真渲染」的配套动作，
                // 提前执行会吃掉 damage、错推冻结时间轴）。
                // 欠帧（term_frame_due_since）不再无条件放行：ESU 快路
                // 的 request_redraw 与 WM_PAINT 之间若有新 BSU 批被
                // drain（流式输出下的常见竞态），旧逻辑会把半成品 grid
                // 画上屏——蓝条随未归位的光标行伸缩、内容闪烁（需求池
                // P1 的来源之一）。改为：同步区间内欠帧也暂缓，交给该
                // 批 drain 时重新武装的渲染计划（abs 在途是跳过的前提，
                // 且 abs 必伴随 redraw_at，补画一定会被调度）在 ESU 后
                // 补画完整帧；但暂缓以欠帧起点 + REDRAW_ABS_CAP 为限，
                // 超龄后无论是否同步一律放行——保住「应用不会卡死在
                // BSU 画面冻结」的绝对兜底语义（worst case 与原 abs
                // 兜底同量级，普通流量根本到不了）。
                let mut skip_pane: Vec<bool> = state.tabs[state.active_tab]
                    .panes
                    .iter()
                    .map(|s| {
                        s.term.is_synchronized()
                            && s.redraw_abs_at.is_some_and(|a| render_t0 < a)
                            && s.term_frame_due_since
                                .is_none_or(|d| render_t0.duration_since(d) < REDRAW_ABS_CAP)
                    })
                    .collect();
                // 最大化期间其余窗格无条件跳过渲染（P14：不可见，纹理
                // 不上屏；后台照常消化输出，还原/重启时强制补帧）。
                if let Some(m) = state.tabs[state.active_tab].maximized {
                    for (i, skip) in skip_pane.iter_mut().enumerate() {
                        if i != m {
                            *skip = true;
                        }
                    }
                }

                for (i, s) in state.tabs[state.active_tab].panes.iter_mut().enumerate() {
                    if skip_pane[i] {
                        continue;
                    }
                    s.term.grid_mut().take_dirty();
                    // 光标跟随策略（逐窗格）：正常情况下零延迟跟随终端
                    // 光标；处于「帧尾未归位」窗口（ESU 后还没重新显示
                    // 光标）时冻结旧位置，等归位序列或超时，避免画出
                    // 重绘残留位。
                    let now = Instant::now();
                    let g = s.term.grid();
                    let seen = (g.cursor.row, g.cursor.col, g.cursor.visible);
                    // 同行近距移动是打字/退格的特征，即时跟随不冻结；
                    // 动画残留位的特征是跨行大跳，才需要等归位确认。
                    let typing_move = seen.2
                        && s.cursor_displayed.2
                        && seen.0 == s.cursor_displayed.0
                        && seen.1.abs_diff(s.cursor_displayed.1) <= 4;
                    if !s.term.cursor_unsettled() || typing_move {
                        s.cursor_frozen_at = None;
                        s.cursor_displayed = seen;
                    } else {
                        let frozen = *s.cursor_frozen_at.get_or_insert(now);
                        if now.duration_since(frozen) >= CURSOR_FREEZE_CAP {
                            s.cursor_displayed = seen;
                            s.cursor_frozen_at = None;
                        } else if s.cursor_displayed != seen {
                            // 安排超时时刻补画一帧，防止光标停滞在旧位。
                            let at = frozen + CURSOR_FREEZE_CAP;
                            s.redraw_at = Some(s.redraw_at.map_or(at, |x| x.min(at)));
                        }
                    }
                }

                // —— 窗格离屏纹理懒创建（新窗格/恢复后的首帧）——
                // 先按 1x1 占位注册到 egui 拿稳定 TextureId（run_ui 录
                // 制 Image 需要它）；本帧布局后的矩形对照会立即按实际
                // 尺寸重建并原地换绑——egui 在 pass 录制时才解析纹理，
                // 占位尺寸不会真的被采样上屏。
                for i in 0..state.tabs[state.active_tab].panes.len() {
                    let sid = state.tabs[state.active_tab].panes[i].id;
                    if state.pane_textures.contains_key(&sid) {
                        continue;
                    }
                    state.renderer.ensure_offscreen(sid, 1, 1);
                    let Some(view) = state.renderer.offscreen_view(sid) else {
                        continue; // 刚 ensure 过必在；防御分支
                    };
                    let tex = state.egui_renderer.register_native_texture(
                        state.renderer.device(),
                        view,
                        wgpu::FilterMode::Nearest,
                    );
                    state.pane_textures.insert(sid, tex);
                }

                // —— egui 帧：跑 UI 布局，产出本帧各窗格矩形 ——
                let raw_input = state.egui_state.take_egui_input(&state.window);

                // —— 最大化越界修复（第十轮问题1）——
                // 无边框 + WS_THICKFRAME 最大化时，Windows 将窗口推至约
                // (-8,-8)，尺寸比工作区大 ~16px（隐藏不可见粗边框）。egui
                // 按完整 inner_size 布局，右/下贴边内容画在屏幕外被裁剪。
                //
                // 修复：用 MonitorFromWindow + GetMonitorInfoW 实算四边越界量，
                // shrink raw_input.screen_rect.max（只改 max，保持 min=(0,0)），
                // 使 egui 内容区等于实际可见区域。
                //
                // 坐标链路验证：
                //   鼠标事件坐标 = 客户区坐标（原点 (0,0)），screen_rect.min
                //   仍为 (0,0)，两者坐标系一致，无需平移。
                //   snap_layouts 按钮换算：egui rect × ppp + inner_position；
                //   shrink 后按钮 egui 坐标贴 shrunk max，× ppp + (-8) = 工作区
                //   右边界，正确（不再超出屏幕）。
                // —— 最大化越界修复（第十一轮根因分析：无需 shrink screen_rect）——
                //
                // 第十轮曾尝试：GetWindowRect 检测到 8px overflow → shrink raw_input.screen_rect。
                // 但第十一轮诊断证明该思路错误，原因：
                //   1. winit 的 window.inner_size() 调用 GetClientRect（非 GetWindowRect），
                //      返回的是客户区物理像素（2560px on 2560px monitor），已排除 8px 不可见
                //      阴影边框。
                //   2. GetWindowRect 返回的 8px overflow 是系统管理的不可见 THICKFRAME 阴影，
                //      不在客户区内，不影响内容布局。
                //   3. shrink screen_rect 反而使 egui 布局比可见区域窄 8px，造成右侧 8px 空白。
                //
                // 真正原因（第十一轮定位）：
                //   footer label "[ 编辑模式 ]" 用 `label_char_count * cw` 估算宽度，
                //   但 CJK 汉字在等宽终端字体中渲染为 2×cw（全角），导致文字实际宽度约为
                //   估算值的 1.5×，label_x 偏右，文本溢出纹理右边界被裁剪。
                //   修复已落 lumen-renderer/src/lib.rs（改用 layout_runs().line_w 实测宽度）。
                //   statusbar 按钮同样受 CJK 宽度估算影响，修复已落 shell/statusbar.rs。
                //
                // 此处不再 shrink screen_rect。query_maximized_overflow / maximized_overflow
                // 纯函数已有单测保留（算法正确，只是本场景不需要应用它）。

                // F7② 会话图标：节流轮询各 tab 前台运行程序 → 懒加载其 exe
                // 图标纹理（侧栏隐藏时 probe 内部直接跳过，零开销）。
                let icon_now = Instant::now();
                state.probe_session_icons(icon_now);
                state.ensure_session_icon_textures(icon_now);
                // 控制端镜像视图：把被控端传来的远程会话图标位图 ensure 成本地纹理
                // （内容寻址缓存；下面构造远程 TabItem 时按 hash 取纹理）。
                if state.is_mirror_active() {
                    state.ensure_remote_icon_textures();
                }
                #[cfg(feature = "input-editor")]
                let attachment_overlay = state.attachment_overlay();
                #[cfg(feature = "input-editor")]
                let mut remove_attachment_req: Option<(SessionId, u64)> = None;
                // part3d（K3）：远程视图 + 控制中 → 会话栏整组替换为被控端的远程会话列表
                // （active = 当前订阅会话；点击切换 = 订阅，由下方 activate 分流）。否则画本地
                // tab 列表（原 F7② 两行条目：名称行 + 路径行 + 前台程序 exe 图标）。
                let entries: Vec<shell::TabItem> = if state.is_mirror_active() {
                    let sub = state.remote_ws.subscribed_tab();
                    state
                        .remote_ws
                        .remote_tabs()
                        .iter()
                        .map(|t| shell::TabItem {
                            id: t.id,
                            name: t.name.clone(),
                            path: t.path.clone(),
                            active: Some(t.id) == sub,
                            // F7②-remote：被控端传来的前台程序图标位图 → 本地纹理
                            // （ensure_remote_icon_textures 已按内容 hash 建好）；无则回退字形。
                            icon: t
                                .icon
                                .as_ref()
                                .and_then(|bm| state.remote_icon_tex.get(&remote_icon_hash(bm)))
                                .map(egui::TextureHandle::id),
                            busy: t.busy,
                        })
                        .collect()
                } else {
                    state
                        .tabs
                        .iter()
                        .enumerate()
                        .map(|(i, t)| shell::TabItem {
                            id: t.id,
                            name: t.display_name(),
                            path: t.cwd_path(),
                            active: i == state.active_tab,
                            icon: state.session_icon_for(t.id),
                            busy: t.is_busy(),
                        })
                        .collect()
                };
                let tab = &state.tabs[state.active_tab];
                // 本帧布局对应的窗格 id 快照：下方动作（关 tab/增删窗
                // 格）可能改变结构，矩形与窗格的对应关系以此校验。
                let layout_pane_ids: Vec<SessionId> = tab.panes.iter().map(|p| p.id).collect();
                let panes_view: Vec<shell::PaneView> = tab
                    .panes
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        // 窗格标题（F7①）：与侧栏 display_title 同源
                        // 取值（cwd > OSC 标题），但标题栏空间窄，cwd
                        // 取尾目录名（盘根等无尾名时回完整路径）；悬停
                        // 提示完整 cwd。两者皆无回退「窗格 N」。
                        let cwd = p.term.cwd();
                        // 自定义名（需求2）非空时优先；否则回退默认链
                        // （cwd 尾目录名 > OSC 标题 > 「窗格 N」）。
                        let title = p
                            .custom_title
                            .clone()
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or_else(|| {
                                cwd.map(|c| {
                                    c.file_name().map_or_else(
                                        || c.display().to_string(),
                                        |t| t.to_string_lossy().into_owned(),
                                    )
                                })
                                .or_else(|| {
                                    let t = p.term.title();
                                    (!t.is_empty()).then(|| t.to_owned())
                                })
                                .unwrap_or_else(|| {
                                    i18n::fmt1(i18n::strings().pane_default_name_fmt, i + 1)
                                })
                            });
                        shell::PaneView {
                            id: p.id,
                            tex: state.pane_textures.get(&p.id).copied(),
                            focused: i == tab.focused,
                            title,
                            title_hover: cwd.map(|c| c.display().to_string()),
                        }
                    })
                    .collect();
                let was_renaming = state.shell_state.renaming.is_some();
                let was_pane_renaming = state.shell_state.pane_renaming.is_some();
                let was_ssh_session_renaming = state.shell_state.ssh_session_renaming.is_some();
                // 文件树输入：焦点窗格的 cwd（OSC 9;9 上报）与空闲态
                // （cd 注入闸门，见 Terminal::shell_waiting_input）。
                // 焦点窗格 cwd：先取 OSC 9;9 上报值（Windows shell 集成）。
                let osc_cwd = tab
                    .focused_pane()
                    .term
                    .cwd()
                    .map(std::path::Path::to_path_buf);
                // unix：OSC 9;9 未上报（bash/zsh 无集成）→ 读 shell 进程实时 cwd 兜底
                // （cd 后由 filetree::sync_root 变化检测触发重载）。
                #[cfg(unix)]
                let active_cwd = osc_cwd.or_else(|| {
                    tab.focused_pane()
                        .pty
                        .shell_pid()
                        .and_then(os_cwd::shell_cwd)
                });
                #[cfg(not(unix))]
                let active_cwd = osc_cwd;
                // LLM HUD 只读取焦点 CLI 已画在终端中的模型/上下文状态；
                // 不注入探测命令，也不从账号配置估算 token。
                let llm_hud = if !state.settings.layout.view_mode.is_remote()
                    && !state.settings.layout.view_mode.is_ssh()
                {
                    let pane = tab.focused_pane();
                    pane.llm_cli
                        .or_else(|| llm_cli::detect(None, &pane.term))
                        .map(|kind| {
                            let metrics = llm_cli::hud_metrics(&pane.term, kind);
                            #[cfg(feature = "input-editor")]
                            let bottom_inset =
                                pane.footer_committed_h / state.egui_ctx.pixels_per_point();
                            #[cfg(not(feature = "input-editor"))]
                            let bottom_inset = 0.0;
                            shell::hud::HudView {
                                session_id: pane.id,
                                kind,
                                model: metrics.model,
                                context: metrics.context,
                                usage: metrics.usage,
                                project_path: active_cwd
                                    .as_ref()
                                    .map(|path| path.display().to_string()),
                                foreground_pid: pane.llm_foreground_pid,
                                busy: pane.is_busy(),
                                session_elapsed: pane
                                    .llm_started_at
                                    .map_or(Duration::ZERO, |started| started.elapsed()),
                                bottom_inset,
                            }
                        })
                } else {
                    None
                };
                let shell_idle = tab.focused_pane().term.shell_waiting_input();
                let remote_shell_idle = state.remote_ws.focused_mirror_shell_idle();
                let ssh_shell_idle = state.ssh_runtime.active_shell_idle();
                // 背景图参数（P13）：仅当纹理已加载且 settings 启用时传入。
                // 同时检查 enabled：用户本帧拨动开关关闭后，bg_texture 清空在
                // apply_background_image（run_ui 之后）才执行；提前在此过滤
                // 可保证关闭语义对 egui 层当帧即时生效，避免一帧闪烁。
                let bg_image = state
                    .bg_texture
                    .as_ref()
                    .filter(|_| state.settings.appearance.background.enabled)
                    .map(|tex| shell::BgImageInput {
                        texture_id: tex.texture_id,
                        width: tex.width,
                        height: tex.height,
                        opacity: state.settings.appearance.background.opacity,
                        dim: state.settings.appearance.background.dim,
                    });
                // 历史搜索面板行数据（M4.3）：仅面板打开时计算（最多取 50 条）。
                // 面板关闭时传空 Vec，不做 fuzzy_search 开销。
                // 历史搜索面板行数据（M4.3）：仅面板打开时计算（取前 20 条，由 fuzzy_search 内部截断）。
                // 面板关闭时传空 Vec，不做 fuzzy_search 开销。
                let history_rows_owned: Vec<shell::history_search_ui::HistoryRow> =
                    if state.shell_state.history_search.open {
                        let query = &state.shell_state.history_search.query;
                        state
                            .history
                            .fuzzy_search(query)
                            .into_iter()
                            .map(|hit| {
                                let entry = &state.history.entries()[hit.entry_idx];
                                shell::history_search_ui::HistoryRow {
                                    text: entry.text.clone(),
                                    exit_code: entry.exit_code,
                                    match_spans: hit.match_spans,
                                }
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                // 补全弹窗候选视图（M4.4 批1）：仅 input-editor feature 下，
                // completion.open 时构造 CompletionView 传入 shell；否则传 None。
                #[cfg(feature = "input-editor")]
                let completion_candidate_rows: Vec<
                    shell::completion_ui::CandidateRow,
                > = if state.shell_state.completion.open {
                    state
                        .completion_candidates
                        .iter()
                        .map(|c| shell::completion_ui::CandidateRow {
                            display: c.display.clone(),
                            is_dir: c.is_dir,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                // 锚点：footer 上方合理位置（底部 = 窗口高 - statusbar - footer 估算高度）。
                // 取 egui 逻辑坐标：statusbar 高度 + 一行 footer 高度约 40px 之上。
                // 首版不要求精确跟随光标列，x 取终端区左缘附近固定值即可。
                #[cfg(feature = "input-editor")]
                let completion_view_owned: Option<
                    shell::completion_ui::CompletionView<'_>,
                > = if state.shell_state.completion.open && !completion_candidate_rows.is_empty() {
                    let scale = state.egui_ctx.pixels_per_point();
                    let win_h = state.window.inner_size().height as f32 / scale;
                    // 锚点（弹窗左下，向上展开）：x 对齐 footer 内光标列、
                    // y 取 footer 顶部——补全弹窗左缘跟随光标（海风哥反馈）。
                    // footer 矩形不可用（极端态）时回退「侧栏右缘 + 底部估算」。
                    let (anchor_x, anchor_y) =
                        if let Some((fx, fy, _, _)) = state.focused_footer_rect_px() {
                            let fp = state.renderer.padding() * 0.4;
                            let (cell_w, _) = state.renderer.cell_size();
                            let pane = state.focused_pane();
                            let ev = pane.editor.view();
                            let cur = ev.cursor();
                            let col = lumen_renderer::composer_view::footer_byte_to_col(
                                ev.lines().nth(cur.line).unwrap_or_default(),
                                cur.byte,
                            ) as f32;
                            ((fx + fp + col * cell_w) / scale, fy / scale)
                        } else {
                            let ay = win_h - shell::statusbar::HEIGHT - 20.0 - 4.0;
                            (state.settings.layout.sidebar_width + 12.0, ay)
                        };
                    Some(shell::completion_ui::CompletionView {
                        candidates: &completion_candidate_rows,
                        anchor: egui::pos2(anchor_x, anchor_y),
                    })
                } else {
                    None
                };
                // 共享 token 句柄（登录后懒建）：心跳 worker 自动续期时写回，WS + REST 共读同一份，
                // 确保续期后处处用新 token（治本：免 7 天到期后全面 401）。必须单一实例供两者共享。
                let current_server_url = cloud::server_url();
                let account_server_origin =
                    profile_server_origin(state.profile.as_ref(), &current_server_url)
                        .filter(|origin| cloud::validate_server_transport(origin).is_ok());
                if state.auth_token.is_none() {
                    state.auth_token =
                        profile_auth_token(state.profile.as_ref(), &current_server_url);
                }
                // M5.2：已登录但远程线程未起（启动时已登录 / 刚登录）→ 启动；
                // 每帧收取后台心跳/设备列表回包。M5.3：远程控制 WS 同生命周期。
                if !state.remote.is_running() {
                    if let (Some(auth), Some(server_origin)) =
                        (state.auth_token.clone(), account_server_origin.as_ref())
                    {
                        let exp = state.profile.as_ref().map_or(0, |p| p.token_expires_at);
                        let ctx = state.egui_ctx.clone();
                        // 传 proxy + wake_pending：设备列表后台线程拉到新数据后须唤醒空闲 winit 循环
                        // （否则停在远程视图时在线状态不自动刷新，要切 tab 才更新）。
                        if let Err(error) = state.remote.start(
                            server_origin.clone(),
                            auth,
                            exp,
                            ctx,
                            state.proxy.clone(),
                            state.wake_pending.clone(),
                        ) {
                            log::warn!("启动远程心跳失败: {}", error.user_message());
                        }
                    }
                }
                if !state.remote_ws.is_running() {
                    if let (Some(auth), Some(server_origin)) =
                        (state.auth_token.clone(), account_server_origin.as_ref())
                    {
                        match state.remote_ws.start(
                            server_origin.clone(),
                            auth,
                            state.egui_ctx.clone(),
                            state.proxy.clone(),
                            state.wake_pending.clone(),
                        ) {
                            Ok(()) => {
                                if !state.remote_restore_attempted {
                                    state.remote_restore_attempted = true;
                                    if let Some((device_id, device_name)) = state
                                        .profile
                                        .as_ref()
                                        .and_then(profile::Profile::remote_restore_target)
                                        .map(|(id, name)| (id.to_owned(), name.to_owned()))
                                    {
                                        if state
                                            .remote_ws
                                            .restore_controller_session(device_id, device_name)
                                        {
                                            log::info!("正在恢复上次远程控制会话");
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                log::warn!("启动远程 WS 失败: {}", error.user_message());
                            }
                        }
                    }
                }
                let _ = state.remote.poll();
                // 自动续期落地：心跳 worker 已把新 token 写回共享句柄；这里持久化到 profile，
                // 使重启后也用新 token（否则重启读旧 token、又可能过期）。
                if let Some((tok, exp)) = state.remote.take_refreshed_token() {
                    if let Some(p) = state.profile.as_mut() {
                        p.token = Some(tok);
                        p.token_expires_at = exp;
                        p.save();
                        log::info!("token 已自动续期并落盘（到期 {exp}）");
                    }
                }
                // 远程 WS 的 poll / 被控端输入应用 / 整屏快照转发已移到 user_event
                // （App::pump_remote），失焦也能及时处理（否则配对/输入/镜像会卡到
                // 焦点回来）；此处只读镜像态供渲染。
                // M5.3 part3b 控制端镜像纹理：仅「远程」视图 + 控制中（is_mirror_active）
                // 才确保镜像离屏纹理已注册（保留 id，首次注册后复用），取其 egui 句柄供
                // shell 以 Image 铺满终端区；镜像内容在下方窗格渲染段画进该纹理（wgpu
                // 上色，复用窗格渲染器、控制端主题就地解析颜色）。bug3：本地视图不画。
                // 内容由下方窗格渲染段后的镜像渲染块（搜索 MIRROR_OFFSCREEN_ID 的
                // renderer.render）每帧画入；尺寸变化时该块重绑 egui 纹理。退出控制/远程
                // 视图时在 else 分支释放（仿 release_pane_resources，延后注销），再次控制
                // 必重新注册全新纹理，杜绝悬挂句柄。
                // M5.3 part3d Phase 3c：订阅**多窗格**会话时，按窗格数管理 per-pane 离屏纹理并构造
                // shell 多窗格镜像数据；内容由下方多窗格镜像渲染段每帧画入。单窗格 / 非控制时释放。
                let multi_active =
                    state.is_mirror_active() && !state.remote_ws.mirror_panes().is_empty();
                let remote_mirror_multi = if multi_active {
                    let n = state.remote_ws.mirror_panes().len();
                    // 窗格数减少：释放多余纹理 + 离屏（延后注销）。
                    while state.mirror_pane_textures.len() > n {
                        let i = state.mirror_pane_textures.len() - 1;
                        state.renderer.drop_offscreen(mirror_pane_offscreen_id(i));
                        if let Some(tex) = state.mirror_pane_textures.pop() {
                            state.pending_tex_free.push(tex);
                        }
                    }
                    // 窗格数增加 / 首次：注册缺失纹理（保留 id，后续复用、尺寸变时换绑）。
                    while state.mirror_pane_textures.len() < n {
                        let i = state.mirror_pane_textures.len();
                        let oid = mirror_pane_offscreen_id(i);
                        state.renderer.ensure_offscreen(oid, 1, 1);
                        match state.renderer.offscreen_view(oid) {
                            Some(view) => {
                                let tex = state.egui_renderer.register_native_texture(
                                    state.renderer.device(),
                                    view,
                                    wgpu::FilterMode::Nearest,
                                );
                                state.mirror_pane_textures.push(tex);
                            }
                            None => break,
                        }
                    }
                    // 布局比例 / 焦点都不复刻被控端（缩放/分隔条/选中不同步）——只取最大化结构；
                    // shell 按窗格数均分画 pane_rects、无焦点高亮。
                    // 镜像比例布局（初始复刻被控端、控制端可拖；shell 据此画 pane_rects/分隔条）。
                    // 多窗格时 mirror_layout 必为 Some（SubscriptionStarted 多窗格臂建），均分仅兜底。
                    let (maximized, layout) = state.remote_ws.mirror_layout().map_or_else(
                        || (None, shell::layout::PaneLayout::uniform(n)),
                        |l| (l.maximized.map(|m| m as usize), l.layout.clone()),
                    );
                    let focused_idx = state.remote_ws.mirror_active_pane_idx();
                    let panes: Vec<shell::MirrorPaneView> = state
                        .remote_ws
                        .mirror_panes()
                        .iter()
                        .enumerate()
                        .filter_map(|(i, mp)| {
                            state
                                .mirror_pane_textures
                                .get(i)
                                .map(|&tex| shell::MirrorPaneView {
                                    tex,
                                    title: mirror_pane_title(&mp.term, i),
                                })
                        })
                        .collect();
                    Some(shell::MirrorMultiInput {
                        panes,
                        layout,
                        maximized,
                        focused_idx,
                    })
                } else {
                    // 退出多窗格：释放全部 per-pane 纹理 + 离屏（延后注销）。
                    while let Some(tex) = state.mirror_pane_textures.pop() {
                        let i = state.mirror_pane_textures.len();
                        state.renderer.drop_offscreen(mirror_pane_offscreen_id(i));
                        state.pending_tex_free.push(tex);
                    }
                    None
                };
                // 单窗格镜像离屏纹理（既有路径；多窗格时不画单 Image，置 None）。
                let remote_mirror_tex = if state.is_mirror_active() && !multi_active {
                    if state.mirror_texture.is_none() {
                        state.renderer.ensure_offscreen(MIRROR_OFFSCREEN_ID, 1, 1);
                        if let Some(view) = state.renderer.offscreen_view(MIRROR_OFFSCREEN_ID) {
                            let tex = state.egui_renderer.register_native_texture(
                                state.renderer.device(),
                                view,
                                wgpu::FilterMode::Nearest,
                            );
                            state.mirror_texture = Some(tex);
                        }
                    }
                    state.mirror_texture
                } else {
                    // 退出控制/远程视图 或 进入多窗格：释放单镜像离屏 + egui 纹理（延后注销）。
                    if let Some(tex) = state.mirror_texture.take() {
                        state.renderer.drop_offscreen(MIRROR_OFFSCREEN_ID);
                        state.pending_tex_free.push(tex);
                    }
                    None
                };
                // 状态栏文件传输进度（控制端活跃 Fetch/Put 聚合；空闲 None → 状态栏照常显示 cwd）。
                // owned，借给 shell_input；须在其前算、生命周期覆盖本帧渲染。
                let transfer_status = state.remote_ws.transfer_status();
                if state.settings.layout.view_mode.is_ssh()
                    && state.ssh_runtime.has_active_terminal()
                    && state.ssh_texture.is_none()
                {
                    state.renderer.ensure_offscreen(SSH_OFFSCREEN_ID, 1, 1);
                    if let Some(view) = state.renderer.offscreen_view(SSH_OFFSCREEN_ID) {
                        state.ssh_texture = Some(state.egui_renderer.register_native_texture(
                            state.renderer.device(),
                            view,
                            wgpu::FilterMode::Nearest,
                        ));
                    }
                }
                let ssh_runtime_view = state.ssh_runtime.active_view();
                let ssh_session_views = state.ssh_runtime.session_views();
                let ssh_file_tree_view = state.ssh_runtime.active_file_tree_view();
                let ssh_connection_test_view = state.ssh_runtime.connection_test_view();
                // M7 片 6：被控端来件配对时，把配对码同时渲染成二维码。
                // 三样东西只有这里拿得全：规范化 origin、账户 id、本机 device_id。
                // 任何一样取不到就不出二维码——横幅退化成只显示 9 位数字，功能不缺失
                // （扫码只是数字码的另一种呈现），所以这里静默 None、不报错。
                let pairing_qr_payload = state.remote_ws.incoming.as_ref().and_then(|inc| {
                    let profile = state.profile.as_ref()?;
                    let origin =
                        profile_server_origin(Some(profile), &cloud::server_url())?;
                    Some(lumen_protocol::pairing_qr::PairingQrPayload::new(
                        &origin,
                        profile.user_id.as_deref()?,
                        // 被控端 = 手机要连的目标，所以 t 填**本机** device_id。
                        profile.device_id.as_deref()?,
                        &inc.pairing_code,
                        inc.expires_at,
                    ))
                });
                let shell_input = shell::ShellInput {
                    panes: &panes_view,
                    layout: tab.layout.clone(),
                    maximized: tab.maximized,
                    tabs: &entries,
                    profile: state.profile.as_ref(),
                    // 头像菜单更新项：有可用更新时给版本号（显示「更新到 vX」）。
                    // Windows 等静默下载就绪才显示；非 Windows 无下载链路，检查到即显示。
                    update_version: state
                        .update_available
                        .as_ref()
                        .filter(|_| !cfg!(windows) || state.update_ready.is_some())
                        .map(|u| u.version.to_string()),
                    // 登录态过期判定（自动续期之外的兜底）：本地时钟判过期，或服务端实际拒绝
                    // （list_devices 401 / 列表里已无本机 did）。后者修「token 被服务端失效但
                    // 本地 exp 未到 → 不提示、静默显绿却隐身、用户只能手动两台都重登」的痛点。
                    token_expired: {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0i64, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
                        let local_expired = state
                            .profile
                            .as_ref()
                            .is_some_and(|p| p.token_expires_at > 0 && now >= p.token_expires_at);
                        local_expired || (state.profile.is_some() && state.remote.needs_relogin())
                    },
                    cwd: active_cwd.as_deref(),
                    shell_idle,
                    remote_shell_idle,
                    ssh_shell_idle,
                    os_dark: state.os_dark,
                    bg_image,
                    // 底部状态栏所需：当前有效输入模式 + 经典直通开关（M4.1 批E）
                    #[cfg(feature = "input-editor")]
                    input_mode: effective_session_mode(tab.focused_pane(), state.force_fallback),
                    #[cfg(feature = "input-editor")]
                    force_fallback: state.force_fallback,
                    transfer: transfer_status.as_ref(),
                    link: state.remote_ws.p2p_link_state(),
                    // 服务器连接指示三态：①未配置地址→None(不显示)；②已配置但未登录(心跳未起)
                    // →未连接(黄)；③已登录→据心跳连接态映射 已连接(绿)/连接错误(红)/连接中(黄)。
                    server_conn: {
                        if crate::cloud::server_url().trim().is_empty() {
                            None
                        } else if !state.remote.is_running() {
                            Some(shell::statusbar::ServerConnBadge::Disconnected)
                        } else {
                            match state.remote.server_conn() {
                                crate::remote::ServerConn::Connected => {
                                    Some(shell::statusbar::ServerConnBadge::Connected)
                                }
                                crate::remote::ServerConn::Error => {
                                    Some(shell::statusbar::ServerConnBadge::Error)
                                }
                                crate::remote::ServerConn::Connecting => {
                                    Some(shell::statusbar::ServerConnBadge::Disconnected)
                                }
                            }
                        }
                    },
                    history_rows: &history_rows_owned,
                    #[cfg(feature = "input-editor")]
                    completion_view: completion_view_owned,
                    #[cfg(not(feature = "input-editor"))]
                    completion_view: None,
                    llm_hud,
                    remote_devices: &state.remote.devices,
                    ssh_inventory: state
                        .ssh_store
                        .as_ref()
                        .map_or(&state.ssh_empty_inventory, ssh::SshStore::inventory),
                    ssh_runtime: ssh_runtime_view.as_ref(),
                    ssh_sessions: &ssh_session_views,
                    ssh_file_tree: ssh_file_tree_view.as_ref(),
                    ssh_connection_test: ssh_connection_test_view.as_ref(),
                    ssh_terminal_tex: state.ssh_texture,
                    active_device_id: state.remote.active_device_id.as_deref(),
                    remote_pairing: state.remote_ws.pairing.as_ref(),
                    pairing_qr: pairing_qr_payload.as_ref(),
                    remote_incoming: state.remote_ws.incoming.as_ref(),
                    remote_session: state.remote_ws.session.as_ref(),
                    remote_mirror_tex,
                    remote_mirror_multi,
                    // part3c-2：**远程视图（远程 tab）** 一律画远程树——未控制时 remote_filetree
                    // 为 None → 画「等待 cwd」占位，绝不回落本机树（修 #2：未连接设备时远程 tab
                    // 显示本地树）。注意用 view_mode（远程 tab 选中）而非 is_mirror_active
                    // （= 控制中 且 远程 tab），否则未控制时回落本地树。
                    remote_filetree: if state.settings.layout.view_mode.is_remote() {
                        state.remote_ws.remote_filetree()
                    } else {
                        None
                    },
                    // part3c-2 #7：文件剪贴板来源侧 + 待决覆盖冲突项数（驱动菜单 / 覆盖模态）。
                    file_clipboard_side: state.remote_ws.file_clipboard().map(|c| c.side),
                    ssh_file_clipboard_available: state.ssh_file_clipboard.is_some(),
                    overwrite_conflict_count: state
                        .pending_paste
                        .as_ref()
                        .map(|p| p.conflict_count)
                        .or_else(|| state.pending_ssh_download.as_ref().map(|_| 1))
                        .or_else(|| state.remote_ws.upload_conflict_count()),
                };
                // 「回到底部」浮动按钮目标（上一帧几何；run_ui 闭包内绘制、
                // 闭包后处理点击）。须在可变借用 state.shell_state 之前算好。
                let scroll_to_bottom_targets = if state.settings.layout.view_mode.is_ssh() {
                    Vec::new()
                } else {
                    state.scroll_to_bottom_overlays()
                };
                let mut scroll_to_bottom_req: Option<(SessionId, ScrollToBottomAction)> = None;
                // 终端滚动条目标几何（同样须在借 shell_state 前算好）。
                // 拖动/点击后把目标绝对 display_offset 记入 scroll_set_req，
                // 闭包后落到对应 grid。
                let scrollbar_targets = if state.settings.layout.view_mode.is_ssh() {
                    Vec::new()
                } else {
                    state.scrollbar_overlays()
                };
                let mut scroll_set_req: Option<(SessionId, usize)> = None;
                let scrollbar_drag = &mut state.scrollbar_drag;
                let shell_state = &mut state.shell_state;
                let app_settings = &mut state.settings;
                let app_lock_input = shell::settings_ui::AppLockInput {
                    config: state.app_lock.config(),
                    busy: state.app_lock.crypto_busy(),
                    retry_remaining: state.app_lock.retry_remaining(Instant::now()),
                };
                // F3 更新弹窗配色（当前主题色板；app_settings 借用后用 disjoint
                // 的 state.os_dark 求生效主题）。
                let modal_pal = shell::theme::shell_palette(settings::theme_info(
                    app_settings.effective_theme_id(state.os_dark),
                ));
                // M3.8：传入当前窗口最大化态，顶栏据此切换最大化/还原图标。
                let is_maximized = state.window.is_maximized();
                let mut shell_out = None;
                // footer 右键菜单请求（第十一轮）：egui Area 在帧内弹出。
                #[cfg(feature = "input-editor")]
                let footer_ctx_menu_req = state.footer_context_menu_at.take();
                #[cfg(feature = "input-editor")]
                let mut footer_ctx_action: Option<action::Action> = None;
                // F10：悬停可点击链接时**始终**把光标设为手型（Warp 体验，
                // 海风哥拍板）+ 弹「打开文件/链接（Ctrl+单击）」提示浮层。
                // egui 拥有光标，须在帧内 set（终端区是图像、不自带光标，
                // 帧尾 last-set 生效；hover 结束自然回落默认）。
                let link_hover_active = state.hovered_link.is_some();
                // hover 链接的提示浮层：文案按目标类型（URL/文件）+ 鼠标
                // 逻辑坐标（物理像素 / ppp）。
                let link_tooltip: Option<(egui::Pos2, &'static str)> =
                    state.hovered_link.as_ref().map(|h| {
                        let s = crate::i18n::strings();
                        let text = match h.target {
                            links::LinkTarget::Url(_) => s.link_open_url_hint,
                            links::LinkTarget::File { .. } => s.link_open_file_hint,
                        };
                        let ppp = state.egui_ctx.pixels_per_point();
                        let pos = egui::pos2(
                            state.mouse_pos.0 as f32 / ppp,
                            state.mouse_pos.1 as f32 / ppp,
                        );
                        (pos, text)
                    });
                // F3：Windows 安装包已就绪（静默下载完成）且未「稍后」时弹窗——点
                // 「立即更新」直接拉起已下好的安装器（Warp 式预下载）。非 Windows
                // 无下载链路：检查到新版且未「稍后」即弹窗，引导前往下载页。
                let update_modal: Option<update::UpdateInfo> = if !state.update_dismissed
                    && (state.update_ready.is_some() || !cfg!(windows))
                {
                    state.update_available.clone()
                } else {
                    None
                };
                let mut update_action: Option<UpdateAction> = None;
                // 边缘 resize 光标反馈（窗口外缘 → 对应方向；None=不在边缘）：本帧
                // 在 run_ui 闭包末用 egui set_cursor_icon 注入（走 egui 光标缓存、
                // 离开边缘时能正确恢复）。drag 的发起在下方 RedrawRequested 内。
                let resize_cursor = resize_edge_dir(
                    &state.window,
                    state.mouse_pos,
                    state.egui_ctx.pixels_per_point(),
                )
                .map(|dir| {
                    use winit::window::ResizeDirection as D;
                    match dir {
                        D::West | D::East => egui::CursorIcon::ResizeHorizontal,
                        D::South | D::North => egui::CursorIcon::ResizeVertical,
                        D::SouthWest | D::NorthEast => egui::CursorIcon::ResizeNeSw,
                        D::SouthEast | D::NorthWest => egui::CursorIcon::ResizeNwSe,
                    }
                });
                let full_output = state.egui_ctx.run_ui(raw_input, |ui| {
                    shell_out = Some(shell::show(
                        ui,
                        &shell_input,
                        shell_state,
                        app_settings,
                        app_lock_input,
                        is_maximized,
                    ));
                    #[cfg(feature = "input-editor")]
                    if let Some(overlay) = &attachment_overlay {
                        egui::Area::new(egui::Id::new((
                            "lumen_llm_attachment_strip",
                            overlay.session_id,
                        )))
                        .order(egui::Order::Foreground)
                        .fixed_pos(overlay.rect.min)
                        .show(ui.ctx(), |ui| {
                            // Area 的默认可用高度会延伸到窗口底部；这里只设
                            // set_min_size 会令横向 ScrollArea 把整块高度占满，
                            // 从而遮住文字输入区。先精确分配附件栏矩形，再把
                            // 背景、子 UI 和 clip 全部锁在该矩形内。
                            let (strip_rect, _) =
                                ui.allocate_exact_size(overlay.rect.size(), egui::Sense::hover());
                            let painter = ui.painter().with_clip_rect(strip_rect);
                            painter.rect_filled(strip_rect, 0.0, modal_pal.bg_dark);
                            painter.rect_stroke(
                                strip_rect,
                                0.0,
                                egui::Stroke::new(1.0_f32, modal_pal.panel_outline),
                                egui::StrokeKind::Inside,
                            );

                            let content_rect = strip_rect.shrink2(egui::vec2(6.0, 4.0));
                            let mut content_ui = ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(content_rect)
                                    .layout(egui::Layout::left_to_right(egui::Align::Min)),
                            );
                            content_ui.set_clip_rect(content_rect);
                            egui::ScrollArea::horizontal()
                                .id_salt(("llm_attachment_scroll", overlay.session_id))
                                .max_height(content_rect.height())
                                .auto_shrink([false, false])
                                .show(&mut content_ui, |ui| {
                                    ui.horizontal_top(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{} 图片",
                                                overlay.cli_name
                                            ))
                                            .small()
                                            .color(modal_pal.fg_dim),
                                        );
                                        ui.separator();
                                        for item in &overlay.items {
                                            ui.vertical(|ui| {
                                                ui.add(
                                                    egui::Image::new(
                                                        egui::load::SizedTexture::new(
                                                            item.texture,
                                                            item.size,
                                                        ),
                                                    )
                                                    .corner_radius(4.0),
                                                )
                                                .on_hover_text(format!(
                                                    "{} · {}×{}",
                                                    item.label,
                                                    item.original_size.0,
                                                    item.original_size.1
                                                ));
                                                ui.horizontal(|ui| {
                                                    ui.label(
                                                        egui::RichText::new(&item.label)
                                                            .monospace()
                                                            .color(modal_pal.fg),
                                                    );
                                                    if ui
                                                        .small_button("×")
                                                        .on_hover_text("移除图片")
                                                        .clicked()
                                                    {
                                                        remove_attachment_req =
                                                            Some((overlay.session_id, item.id));
                                                    }
                                                });
                                            });
                                            ui.add_space(8.0);
                                        }
                                    });
                                });
                        });
                    }
                    // 边缘 resize 光标：放在 shell::show 之后设，覆盖边缘处控件的
                    // 默认光标（egui 末次 set_cursor_icon 生效）。
                    if let Some(c) = resize_cursor {
                        ui.ctx().set_cursor_icon(c);
                    }
                    // ── 「回到底部」浮动按钮（窗格上滚超过一整屏时，底部
                    // 居中的圆形向下箭头；点击回到最新输出）──
                    for target in &scroll_to_bottom_targets {
                        let resp =
                            egui::Area::new(egui::Id::new(("lumen_scroll_to_bottom", target.sid)))
                                .order(egui::Order::Foreground)
                                .fixed_pos(target.rect.min)
                                .show(ui.ctx(), |ui| {
                                    let (r, resp) = ui.allocate_exact_size(
                                        target.rect.size(),
                                        egui::Sense::click(),
                                    );
                                    let hovered = resp.hovered();
                                    let p = ui.painter();
                                    let c = r.center();
                                    let radius = r.width() / 2.0;
                                    // 圆底：平时弹层灰、hover 强调色（Warp 式白底）。
                                    p.circle_filled(
                                        c,
                                        radius,
                                        if hovered {
                                            modal_pal.accent
                                        } else {
                                            modal_pal.bg_panel
                                        },
                                    );
                                    p.circle_stroke(
                                        c,
                                        radius,
                                        egui::Stroke::new(1.0_f32, modal_pal.panel_outline),
                                    );
                                    // 向下箭头（竖杆 + 两撇箭头头），hover 反相配色。
                                    let arrow = if hovered {
                                        modal_pal.accent_fg
                                    } else {
                                        modal_pal.fg
                                    };
                                    let st = egui::Stroke::new(2.0_f32, arrow);
                                    p.line_segment(
                                        [egui::pos2(c.x, c.y - 6.0), egui::pos2(c.x, c.y + 5.0)],
                                        st,
                                    );
                                    p.line_segment(
                                        [
                                            egui::pos2(c.x - 4.5, c.y + 0.5),
                                            egui::pos2(c.x, c.y + 5.0),
                                        ],
                                        st,
                                    );
                                    p.line_segment(
                                        [
                                            egui::pos2(c.x + 4.5, c.y + 0.5),
                                            egui::pos2(c.x, c.y + 5.0),
                                        ],
                                        st,
                                    );
                                    resp
                                });
                        if resp.inner.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.inner.clicked() {
                            scroll_to_bottom_req = Some((target.sid, target.action));
                        }
                    }
                    // ── 终端区滚动条（有历史的窗格右缘竖向拖动条）──
                    // 滑块：平时半透明，hover/拖动时提亮。拖滑块跟手（记
                    // 抓取锚点）、点轨道空白处跳转（滑块中心落到点击点）。
                    for geom in &scrollbar_targets {
                        let track = geom.track;
                        let thumb = geom.thumb;
                        let sb = geom.scrollback;
                        let sid = geom.sid;
                        let resp = egui::Area::new(egui::Id::new(("lumen_scrollbar", sid)))
                            .order(egui::Order::Foreground)
                            .fixed_pos(track.min)
                            .show(ui.ctx(), |ui| {
                                let (_r, resp) = ui.allocate_exact_size(
                                    track.size(),
                                    egui::Sense::click_and_drag(),
                                );
                                resp
                            })
                            .inner;
                        let dragging = matches!(*scrollbar_drag, Some((s, _)) if s == sid);
                        let active = dragging || resp.hovered();
                        // 轨道反算：把指针 Y（减抓取锚点）映射回 display_offset。
                        // offset = sb × (1 - top/movable)，top∈[0, movable]。
                        let movable = (track.height() - thumb.height()).max(0.0);
                        let offset_from_top = |anchor: f32, p: egui::Pos2| -> usize {
                            if movable <= 0.0 {
                                return 0;
                            }
                            let top = (p.y - track.top() - anchor).clamp(0.0, movable);
                            (sb as f32 * (1.0 - top / movable))
                                .round()
                                .clamp(0.0, sb as f32) as usize
                        };
                        if resp.drag_started() {
                            // 抓滑块 → 锚点 = 指针到滑块顶的距离（跟手）；
                            // 抓轨道空白 → 锚点 = 半个滑块（中心对准指针，跳转）。
                            if let Some(p) = resp.interact_pointer_pos() {
                                let anchor = if thumb.contains(p) {
                                    p.y - thumb.top()
                                } else {
                                    thumb.height() / 2.0
                                };
                                *scrollbar_drag = Some((sid, anchor));
                            }
                        }
                        if let (Some((dsid, anchor)), Some(p)) =
                            (*scrollbar_drag, resp.interact_pointer_pos())
                        {
                            if dsid == sid && (resp.dragged() || resp.drag_started()) {
                                scroll_set_req = Some((sid, offset_from_top(anchor, p)));
                            }
                        }
                        // 纯点击（未达拖动阈值）：与拖动起步同构——点滑块本体
                        // 用跟手锚点（offset 不变、不跳），只有点轨道空白才把
                        // 滑块中心对准点击点跳转。否则单击滑块上/下半区会窜走
                        // （offset 跳数千行），违反「点滑块不动」的标准滚动条语义。
                        if resp.clicked() {
                            if let Some(p) = resp.interact_pointer_pos() {
                                let anchor = if thumb.contains(p) {
                                    p.y - thumb.top()
                                } else {
                                    thumb.height() / 2.0
                                };
                                scroll_set_req = Some((sid, offset_from_top(anchor, p)));
                            }
                        }
                        if resp.drag_stopped() {
                            *scrollbar_drag = None;
                        }
                        if active {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
                        }
                        // 绘制：active 时画淡轨道底；滑块 active 提亮、平时半透明。
                        // 圆角半径取轨道半宽（10/2），画成胶囊。
                        let painter = ui.painter();
                        if active {
                            painter.rect_filled(track, 5.0, modal_pal.fg_dim.gamma_multiply(0.10));
                        }
                        // 滑块横向内缩 2px、画成胶囊，观感更轻。
                        let tr = thumb.shrink2(egui::vec2(2.0, 0.0));
                        let alpha = if active { 0.85 } else { 0.40 };
                        painter.rect_filled(tr, 5.0, modal_pal.fg_dim.gamma_multiply(alpha));
                    }
                    if link_hover_active {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    // F10：hover 链接提示浮层（VSCode 风「打开文件 [Ctrl+Click]」
                    // 的中文版），锚在鼠标右下方，不拦截点击。
                    if let Some((pos, text)) = link_tooltip {
                        egui::Area::new(egui::Id::new("lumen_link_tooltip"))
                            .fixed_pos(pos + egui::vec2(14.0, 18.0))
                            .order(egui::Order::Tooltip)
                            .interactable(false)
                            .show(ui.ctx(), |ui| {
                                egui::Frame::popup(ui.style()).show(ui, |ui| {
                                    ui.label(text);
                                });
                            });
                    }
                    // F3 更新提示框：用 egui::Modal（**不是** Window）——设置页
                    // 也是 Modal，后 show 的 Modal 层级更高，故更新框能压在设置
                    // 面板之上、在任何界面都可见（修：原 Window 默认 Order::Middle
                    // 被设置 Modal 盖住，用户在设置页点检查更新看不到弹窗）。
                    // backdrop 聚焦并阻断下层，按钮动作帧后施加。
                    if let Some(info) = &update_modal {
                        let s = crate::i18n::strings();
                        let p = &modal_pal;
                        egui::Modal::new(egui::Id::new("lumen_update_modal"))
                            // backdrop 透明：不 dim 背景（海风哥 2026-06-14：弹窗
                            // 背景不要半透明；对话框本体仍 bg_dark 实色 + 边框）。
                            .backdrop_color(egui::Color32::TRANSPARENT)
                            .frame(
                                egui::Frame::new()
                                    .fill(p.bg_dark)
                                    .corner_radius(egui::CornerRadius::same(10))
                                    .inner_margin(egui::Margin::same(20)),
                            )
                            .show(ui.ctx(), |ui| {
                                ui.set_width(404.0);
                                // —— 标题行：accent 圆底下载图标 + 大标题 ——
                                ui.horizontal(|ui| {
                                    let (r, _) = ui.allocate_exact_size(
                                        egui::vec2(28.0, 28.0),
                                        egui::Sense::hover(),
                                    );
                                    {
                                        let painter = ui.painter();
                                        let c = r.center();
                                        painter.circle_filled(c, 13.0, p.accent);
                                        let st = egui::Stroke::new(1.8_f32, p.accent_fg);
                                        // 向下箭头（竖杆 + 两撇箭头头）
                                        painter.line_segment(
                                            [
                                                egui::pos2(c.x, c.y - 5.5),
                                                egui::pos2(c.x, c.y + 4.5),
                                            ],
                                            st,
                                        );
                                        painter.line_segment(
                                            [
                                                egui::pos2(c.x - 4.0, c.y + 0.5),
                                                egui::pos2(c.x, c.y + 4.5),
                                            ],
                                            st,
                                        );
                                        painter.line_segment(
                                            [
                                                egui::pos2(c.x + 4.0, c.y + 0.5),
                                                egui::pos2(c.x, c.y + 4.5),
                                            ],
                                            st,
                                        );
                                    }
                                    ui.add_space(8.0);
                                    ui.label(
                                        egui::RichText::new(s.update_modal_title)
                                            .size(18.0)
                                            .strong()
                                            .color(p.fg),
                                    );
                                });
                                ui.add_space(12.0);
                                // —— 版本号 + 「已就绪」提示 ——
                                ui.label(
                                    egui::RichText::new(crate::i18n::fmt1(
                                        s.update_modal_version_fmt,
                                        info.version,
                                    ))
                                    .size(14.0)
                                    .color(p.fg),
                                );
                                ui.add_space(3.0);
                                ui.label(
                                    egui::RichText::new(if cfg!(windows) {
                                        s.update_modal_ready_hint
                                    } else {
                                        s.update_modal_manual_hint
                                    })
                                    .size(12.0)
                                    .color(p.fg_dim),
                                );
                                ui.add_space(14.0);
                                // —— 更新内容：小标题 + 带边框可滚动卡片 ——
                                if !info.notes.trim().is_empty() {
                                    ui.label(
                                        egui::RichText::new(s.update_modal_notes_label)
                                            .size(12.0)
                                            .strong()
                                            .color(p.fg_dim),
                                    );
                                    ui.add_space(5.0);
                                    egui::Frame::new()
                                        .fill(p.bg_highlight)
                                        .corner_radius(egui::CornerRadius::same(6))
                                        .inner_margin(egui::Margin::same(10))
                                        .show(ui, |ui| {
                                            egui::ScrollArea::vertical()
                                                .max_height(176.0)
                                                .auto_shrink([false, false])
                                                .show(ui, |ui| {
                                                    ui.label(
                                                        egui::RichText::new(info.notes.trim())
                                                            .color(p.fg),
                                                    );
                                                });
                                        });
                                    ui.add_space(16.0);
                                }
                                // —— 按钮行：主 CTA 立即更新（非 Windows=前往下载）/ 稍后（左），跳过此版本（右弱化）——
                                ui.horizontal(|ui| {
                                    let install = egui::Button::new(
                                        egui::RichText::new(if cfg!(windows) {
                                            s.update_btn_install
                                        } else {
                                            s.update_btn_download
                                        })
                                        .strong()
                                        .color(p.accent_fg),
                                    )
                                    .fill(p.accent)
                                    .min_size(egui::vec2(104.0, 32.0));
                                    if ui.add(install).clicked() {
                                        update_action = Some(if cfg!(windows) {
                                            UpdateAction::Install
                                        } else {
                                            UpdateAction::OpenDownload
                                        });
                                    }
                                    ui.add_space(8.0);
                                    if ui
                                        .add(
                                            egui::Button::new(s.update_btn_later)
                                                .min_size(egui::vec2(60.0, 32.0)),
                                        )
                                        .clicked()
                                    {
                                        update_action = Some(UpdateAction::Later);
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .add(
                                                    egui::Button::new(
                                                        egui::RichText::new(s.update_btn_skip)
                                                            .color(p.fg_dim),
                                                    )
                                                    .fill(egui::Color32::TRANSPARENT),
                                                )
                                                .clicked()
                                            {
                                                update_action = Some(UpdateAction::Skip);
                                            }
                                        },
                                    );
                                });
                            });
                    }

                    // ── footer 右键菜单（第十一轮，input-editor feature）──
                    #[cfg(feature = "input-editor")]
                    if let Some((mx, my)) = footer_ctx_menu_req {
                        let scale = ui.ctx().pixels_per_point();
                        // 物理像素 → egui 逻辑点
                        let lx = mx as f32 / scale;
                        let ly = my as f32 / scale;
                        let s = crate::i18n::strings();
                        // 查询编辑器选区（用于灰显判断）
                        let has_sel = {
                            let ti = state.active_tab;
                            let pi = state.tabs[ti].focused;
                            state.tabs[ti].panes[pi].editor.view().has_selection()
                        };

                        let area_resp = egui::Area::new(egui::Id::new("footer_ctx_menu"))
                            .fixed_pos(egui::pos2(lx, ly))
                            .order(egui::Order::Foreground)
                            .show(ui.ctx(), |ui| {
                                egui::Frame::popup(ui.style()).show(ui, |ui| {
                                    // 复制（有选区时可用）
                                    let copy_btn =
                                        ui.add_enabled(has_sel, egui::Button::new(s.ctx_menu_copy));
                                    if copy_btn.clicked() {
                                        // 复制编辑器选区（dispatch 内处理）
                                        footer_ctx_action = Some(action::Action::Term(
                                            action::TermAction::CopyEditorSelection,
                                        ));
                                    }
                                    // 剪切（有选区时可用）
                                    let cut_btn =
                                        ui.add_enabled(has_sel, egui::Button::new(s.ctx_menu_cut));
                                    if cut_btn.clicked() {
                                        footer_ctx_action = Some(action::Action::Term(
                                            action::TermAction::CutEditorSelection,
                                        ));
                                    }
                                    // 粘贴（始终可用）
                                    if ui.button(s.ctx_menu_paste).clicked() {
                                        footer_ctx_action = Some(action::Action::Term(
                                            action::TermAction::PasteClipboard,
                                        ));
                                    }
                                    // 全选
                                    if ui.button(s.ctx_menu_select_all).clicked() {
                                        footer_ctx_action = Some(action::Action::Edit(
                                            action::EditAction::SelectAll,
                                        ));
                                    }
                                });
                            });
                        // Esc 或点击菜单外：关闭（Area 自然消失，不处理关闭信号）
                        let _ = area_resp;
                    }
                });
                // F3：更新提示框按钮动作（帧后施加）。
                match update_action {
                    Some(UpdateAction::Install) => {
                        // 安装包已就绪（静默预下载完成）：直接拉起安装器，落盘后
                        // 优雅退出（走 CloseRequested 同路径）让它覆盖安装并重启。
                        if let Some(path) = state.update_ready.clone() {
                            match update::launch_installer(&path) {
                                Ok(()) => {
                                    state.shell_state.toast.push(
                                        shell::toast::ToastKind::Info,
                                        i18n::strings().update_toast_installing.to_owned(),
                                    );
                                    state.update_dismissed = true;
                                    state.persist_sessions();
                                    #[cfg(feature = "input-editor")]
                                    state.history.flush_on_exit();
                                    event_loop.exit();
                                    return;
                                }
                                Err(e) => {
                                    log::error!("F3：拉起安装器失败 {e}");
                                    state.update_dismissed = true;
                                    state.shell_state.toast.push(
                                        shell::toast::ToastKind::Error,
                                        i18n::fmt1(
                                            i18n::strings().update_toast_download_failed_fmt,
                                            &e,
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    Some(UpdateAction::OpenDownload) => {
                        // 非 Windows：打开资产下载地址（浏览器），用户手动下载安装。
                        if let Some(info) = state.update_available.as_ref() {
                            links::open(&links::LinkTarget::Url(info.download_url.clone()));
                        }
                        state.update_dismissed = true;
                    }
                    Some(UpdateAction::Skip) => {
                        if let Some(tag) = state.update_available.as_ref().map(|i| i.tag.clone()) {
                            state.settings.update.skip_version = Some(tag);
                            if let Some(err) = state.settings.save() {
                                log::warn!("F3：写盘跳过版本失败: {err}");
                            }
                        }
                        // 跳过=不装这版：删掉已下好的安装包（清理临时文件）。
                        if let Some(path) = state.update_ready.take() {
                            let _ = std::fs::remove_file(path);
                        }
                        state.update_available = None;
                    }
                    Some(UpdateAction::Later) => state.update_dismissed = true,
                    None => {}
                }
                let Some(mut shell_out) = shell_out else {
                    return; // run_ui 必然执行闭包，防御分支
                };
                #[cfg(feature = "input-editor")]
                if let Some((session_id, attachment_id)) = remove_attachment_req {
                    state.remove_llm_attachment(session_id, attachment_id);
                }
                let ssh_actions = std::mem::take(&mut shell_out.ssh_actions);
                state.apply_ssh_ui_actions(ssh_actions);
                if let Some(action) = shell_out.ssh_runtime_action.take() {
                    state.apply_ssh_runtime_action(action);
                }
                for intent in std::mem::take(&mut shell_out.ssh_filetree_intents) {
                    state.apply_ssh_filetree_intent(intent);
                }
                if shell_out.term_clicked {
                    state.terminal_focused = state.terminal_focus_allowed();
                }
                // 文件树悬停/键盘焦点存到下一帧；Ctrl+C/V 按 TreeView 焦点
                // 仲裁，悬停仅保留给鼠标相关判断。
                state.filetree_hovered = shell_out.filetree_hovered;
                state.filetree_focused = shell_out.filetree_focused;

                // ── footer 右键菜单动作 dispatch（第十一轮）───────────────
                #[cfg(feature = "input-editor")]
                if let Some(ctx_action) = footer_ctx_action {
                    let ti = state.active_tab;
                    let pi = state.tabs[ti].focused;
                    state.dispatch(ctx_action, ti, pi);
                }

                // 「回到底部」按钮点击：目标几何来自帧前快照，执行前必须
                // 重算 action。若期间模式/滚动位置已变化，就丢弃陈旧目标，
                // 绝不能把备用屏 End 误注入已经退出的 shell。
                if let Some((sid, requested_action)) = scroll_to_bottom_req {
                    let mut handled = false;
                    if let Some(p) = state.tabs[state.active_tab]
                        .panes
                        .iter_mut()
                        .find(|p| p.id == sid)
                    {
                        let g = p.term.grid();
                        let current_action = scroll_to_bottom_action(
                            g.display_offset(),
                            g.rows(),
                            p.uses_application_alternate_scroll(),
                            p.alternate_scroll_distance_hint(),
                        );
                        if current_action == Some(requested_action) {
                            match requested_action {
                                ScrollToBottomAction::LocalGrid => {
                                    p.term.grid_mut().scroll_to_bottom();
                                    handled = true;
                                }
                                ScrollToBottomAction::AlternateApp => {
                                    let use_win32 = p.term.win32_input()
                                        && std::env::var_os("LUMEN_NO_WIN32_INPUT").is_none();
                                    let bytes = input::encode_plain_end(use_win32);
                                    match p.write_user_input(&bytes) {
                                        Ok(()) => {
                                            p.reset_alternate_scroll_distance_hint();
                                            handled = true;
                                        }
                                        Err(e) => {
                                            error!("备用屏回到底部 End 写入 PTY 失败: {e:#}");
                                        }
                                    }
                                }
                            }
                        } else {
                            log::debug!(
                                "丢弃陈旧的回到底部目标 sid={sid} requested={requested_action:?} current={current_action:?}"
                            );
                        }
                    }
                    if handled {
                        state.window.request_redraw();
                    }
                }

                // 滚动条拖动/点击：把目标绝对 display_offset 落到对应窗格。
                // grid 无「设绝对偏移」API，按目标与当前偏移之差走
                // scroll_display（内部夹紧范围），等价且无需改 lumen-term。
                if let Some((sid, target)) = scroll_set_req {
                    if let Some(p) = state.tabs[state.active_tab]
                        .panes
                        .iter_mut()
                        .find(|p| p.id == sid)
                    {
                        let g = p.term.grid_mut();
                        let delta = target as isize - g.display_offset() as isize;
                        if delta != 0 {
                            g.scroll_display(delta);
                            state.window.request_redraw();
                        }
                    }
                }

                // 滚动条拖动态兜底清理：唯一的 drag_stopped 清零点在 run_ui 闭包
                // 的 scrollbar 循环内，被拖窗格若在拖动中从 scrollbar_targets 掉出
                // （切 tab/关窗格/清屏致 sb=0/内容区跌破 MIN_THUMB），或拖动中窗口
                // 失焦/系统接管指针（Alt+Tab 等）致 egui 漏发 drag_stopped，
                // scrollbar_drag 会滞留 Some → 滚动条恒高亮、状态机不闭环。指针
                // 松开是 OS 级事实，本帧非按下态即无条件清零（与 divider 的
                // layout_dirty/B3-8 松手兜底同款惯例）。
                if state.scrollbar_drag.is_some() && !state.egui_ctx.input(|i| i.pointer.any_down())
                {
                    state.scrollbar_drag = None;
                }

                // 底部状态栏「经典模式」按钮：复用 ToggleFallback 同路径（M4.1 批E）。
                #[cfg(feature = "input-editor")]
                if shell_out.toggle_fallback {
                    let ti = state.active_tab;
                    let pi = state.tabs[ti].focused;
                    state.dispatch(
                        action::Action::Term(action::TermAction::ToggleFallback),
                        ti,
                        pi,
                    );
                }
                // 点击窗格（egui interact 侧的命中，与 window_event 的
                // 原始鼠标路由互为冗余、同语义）：切焦点窗格。
                if let Some(pi) = shell_out.pane_clicked {
                    state.focus_pane(pi);
                }
                // 重命名编辑期间键盘/IME 归 egui 的输入框（右键打开
                // 菜单不经过左键焦点仲裁，必须在此强制让出）；编辑以
                // **键盘**结束（Enter/Esc）才把焦点还给终端——点击别处
                // 取消时那次点击已按鼠标仲裁决定焦点归属（点头像/面板
                // 为 false），无条件翻回 true 会让头像菜单开着时键盘
                // 直通 PTY（Esc 关不掉菜单、打字进 shell）。
                if state.shell_state.renaming.is_some()
                    || state.shell_state.pane_renaming.is_some()
                    || state.shell_state.ssh_session_renaming.is_some()
                    || state.shell_state.renaming_device.is_some()
                {
                    state.terminal_focused = false;
                } else if (was_renaming && shell_out.rename_ended_by_key)
                    || (was_pane_renaming && shell_out.pane_rename_ended_by_key)
                    || (was_ssh_session_renaming && shell_out.ssh_session_rename_ended_by_key)
                    || shell_out.rename_device_ended_by_key
                {
                    state.terminal_focused = state.terminal_focus_allowed();
                }
                // —— egui 弹层（右键菜单/头像菜单等 Popup）焦点路由 ——
                // 打开期间键盘恒归 egui：右键打开菜单不经过左键焦点
                // 仲裁，没有这层时 terminal_focused 仍为 true，Esc 想关
                // 菜单却把 \x1b 写进 PTY（PSReadLine 清掉输入中的命令
                // 行），打字也漏进 shell。关闭那帧的焦点归属按关闭方式
                // 仲裁：键盘（Esc）关闭还给终端（关完直接继续敲命令）；
                // 点击关闭尊重该次点击的鼠标仲裁结果（点终端区已置
                // true、点面板保持 false），不强行翻转。
                let popup_open = egui::Popup::is_any_open(&state.egui_ctx);
                if popup_open {
                    state.terminal_focused = false;
                } else if state.was_popup_open
                    && state.shell_state.renaming.is_none()
                    && state.shell_state.pane_renaming.is_none()
                    && state.shell_state.ssh_session_renaming.is_none()
                    && state.shell_state.renaming_device.is_none()
                    && !state.shell_state.settings.open
                    && !state.shell_state.login.open
                    && !state.shell_state.history_search.open
                    && (!state.shell_state.completion.open || state.shell_state.completion.passive)
                    && !state.shell_state.text_editor.is_visible()
                    && !state.egui_ctx.input(|i| i.pointer.any_click())
                {
                    state.terminal_focused = state.terminal_focus_allowed();
                }
                state.was_popup_open = popup_open;

                // —— 侧栏动作：切换 / 重命名 / 新建 / 关闭（tab 级）——
                if let Some(id) = shell_out.activate {
                    if state.is_mirror_active() {
                        // part3d（K3）：远程视图点击列表项 = 订阅查看该被控端会话（被控端焦点
                        // 不动）；非重复订阅才发（subscribe_tab 内含回看复位）。
                        if state.remote_ws.subscribed_tab() != Some(id) {
                            // 切订阅前补发仍按住的鼠标上报键 Release 给**当前**订阅会话，
                            // 否则切走后旧会话程序残留幻影按住（mirror_report_sid 指向旧
                            // 订阅窗格，flush 在 subscribe_tab 改订阅之前 → 投到正确会话）。
                            state.release_held_report_buttons();
                            state.remote_ws.subscribe_tab(id);
                        }
                    } else if let Some(idx) = state.tabs.iter().position(|t| t.id == id) {
                        if idx != state.active_tab {
                            state.activate(idx);
                        }
                    }
                }
                // —— M5.2 远程设备动作：选中 / 改名 / 删除 ——
                if let Some(id) = shell_out.activate_device {
                    state.remote.active_device_id = Some(id);
                }
                if let Some((id, name)) = shell_out.rename_device {
                    state.remote.rename_device(id, name);
                }
                if let Some(id) = shell_out.delete_device {
                    state.remote.delete_device(id);
                }
                // —— M5.3 远程控制动作：连接 / 配对 / 拒绝 / 断开 ——
                if let Some(id) = shell_out.connect_device {
                    state.remote_ws.request_control(id);
                }
                if let Some(code) = shell_out.submit_pairing_code {
                    state.remote_ws.submit_pairing(code);
                }
                if let Some(code) = shell_out.copy_pairing_code {
                    let ok = matches!(
                        state.clipboard.as_mut().map(|c| c.set_text(code)),
                        Some(Ok(()))
                    );
                    if ok {
                        state.shell_state.toast.push(
                            shell::toast::ToastKind::Info,
                            i18n::strings().remote_toast_pairing_code_copied,
                        );
                    } else {
                        error!("写剪贴板失败（复制配对码）");
                        state.shell_state.toast.push(
                            shell::toast::ToastKind::Error,
                            i18n::strings().toast_copy_failed,
                        );
                    }
                    // push 发生在本帧 egui 布局之后：请求下一帧立即显示。
                    state.window.request_redraw();
                }
                if shell_out.cancel_pairing {
                    state.remote_ws.cancel_pairing();
                }
                if shell_out.decline_control {
                    state.remote_ws.decline();
                }
                if shell_out.end_remote_session {
                    state.clear_remote_restore_target();
                    state.remote_ws.end_session();
                }
                // M5.3 part3d：记录镜像区物理像素矩形（鼠标命中→镜像选区换算，part4b）+ Phase 3
                // 尺寸同步（控制端把订阅多窗格会话各格目标网格尺寸发给被控端，被控端 resize 后 1:1）。
                if state.is_mirror_active() {
                    let ppp = state.egui_ctx.pixels_per_point();
                    let tr = shell_out.term_rect;
                    state.mirror_rect_px = Some((
                        tr.min.x * ppp,
                        tr.min.y * ppp,
                        tr.width() * ppp,
                        tr.height() * ppp,
                    ));
                    // Phase 4：多窗格各格内容矩形物理像素 + session_id（鼠标命中→点焦点 / per-pane 选区）。
                    state.mirror_pane_rects_px.clear();
                    for (i, mp) in state.remote_ws.mirror_panes().iter().enumerate() {
                        let r = shell_out
                            .mirror_pane_rects
                            .get(i)
                            .copied()
                            .unwrap_or(egui::Rect::NOTHING);
                        if r.width() >= 1.0 && r.height() >= 1.0 {
                            state.mirror_pane_rects_px.push((
                                mp.session_id,
                                r.min.x * ppp,
                                r.min.y * ppp,
                                r.width() * ppp,
                                r.height() * ppp,
                            ));
                        }
                    }
                    // 据各格内容矩形像素 + **控制端** cell 算 grid_size_for → SubViewport（去重）：
                    // 订阅期间被控端窗格网格恒按控制端算（前台后台都跟随），故控制端 1:1 无裁切无留白。
                    // `mirror_panes` 覆盖单/多窗格（SubscriptionStarted 统一走 per-pane），空 tab 才不发。
                    if let Some(tab_id) = state.remote_ws.subscribed_tab() {
                        if !state.remote_ws.mirror_panes().is_empty() {
                            let sizes: Vec<(session::SessionId, u16, u16)> = state
                                .remote_ws
                                .mirror_panes()
                                .iter()
                                .enumerate()
                                .filter_map(|(i, mp)| {
                                    let rect = shell_out
                                        .mirror_pane_rects
                                        .get(i)
                                        .copied()
                                        .unwrap_or(egui::Rect::NOTHING);
                                    if !(rect.width() >= 1.0 && rect.height() >= 1.0) {
                                        return None;
                                    }
                                    let pw = (rect.width() * ppp).max(1.0) as u32;
                                    let ph = (rect.height() * ppp).max(1.0) as u32;
                                    let (rows, cols) = state.renderer.grid_size_for(pw, ph);
                                    (rows > 0 && cols > 0).then_some((
                                        mp.session_id,
                                        rows as u16,
                                        cols as u16,
                                    ))
                                })
                                .collect();
                            if !sizes.is_empty() {
                                state.remote_ws.send_sub_viewport(tab_id, sizes);
                            }
                            // part3d Phase 3 布局比例**双向**同步（控制端侧）：①应用被控端发来的比例到
                            // 镜像布局；②把控制端镜像比例变化（用户拖了镜像分隔条）发给被控端。回声由
                            // `sub_layout_baseline` 免疫（应用对端比例即更新基线，不会当本地改动回发）。
                            if let Some((lt, rw, cw)) = state.remote_ws.take_sub_layout() {
                                if lt == tab_id {
                                    if let Some(ml) = state.remote_ws.mirror_layout_mut() {
                                        let n = ml.layout.pane_count();
                                        if let Some(lay) =
                                            shell::layout::PaneLayout::from_weights(n, &rw, &cw)
                                        {
                                            ml.layout = lay;
                                        }
                                    }
                                    state.window.request_redraw();
                                }
                                state.remote_ws.note_sub_layout_baseline(lt, rw, cw);
                            }
                            let weights = state.remote_ws.mirror_layout().map(|ml| {
                                (
                                    ml.layout.row_weights().to_vec(),
                                    ml.layout.col_weights().to_vec(),
                                )
                            });
                            if let Some((rw, cw)) = weights {
                                state.remote_ws.send_sub_layout_if_changed(tab_id, rw, cw);
                            }
                        }
                    }
                } else {
                    state.mirror_rect_px = None;
                    state.mirror_pane_rects_px.clear();
                    // 不再渲染镜像（切到本机 / SSH 视图）：释放对被控端窗格网格的接管，
                    // 让被控端立刻回到按自身窗口排版；切回远程视图下一帧自然重新接管。
                    state.remote_ws.release_sub_viewport();
                }
                // 远程控制一次性通知 → toast（弹窗在 egui 帧后 push，需请求重绘）。
                let remote_notices = state.remote_ws.take_notices();
                if !remote_notices.is_empty() {
                    for n in &remote_notices {
                        match n {
                            remote_ws::Notice::SessionStarted {
                                role: lumen_protocol::remote::Role::Controller,
                                ..
                            }
                            | remote_ws::Notice::SessionRestored => {
                                state.persist_remote_restore_target();
                            }
                            remote_ws::Notice::SessionStarted {
                                role: lumen_protocol::remote::Role::Controlled,
                                ..
                            }
                            | remote_ws::Notice::SessionEnded(_) => {
                                state.clear_remote_restore_target();
                            }
                            _ => {}
                        }
                        let (kind, text) = remote_notice_toast(n);
                        state.shell_state.toast.push(kind, text);
                        // 下载（→本地）/ 上传（→远程）完成 → 刷新粘贴目标目录，新文件立即显示。
                        if matches!(
                            n,
                            remote_ws::Notice::DownloadDone { .. }
                                | remote_ws::Notice::UploadDone { .. }
                        ) {
                            state.apply_paste_refresh();
                        }
                    }
                    state.window.request_redraw();
                }
                if let Some((id, name)) = shell_out.rename {
                    if let Some(t) = state.tabs.iter_mut().find(|t| t.id == id) {
                        // 空名 = 清除自定义名，恢复跟随默认标题（焦点
                        // 窗格 cwd > OSC 标题）。
                        t.custom_title = (!name.is_empty()).then_some(name);
                    }
                    state.update_window_title();
                    // 重命名是结构性变更：落盘（F4）。
                    state.persist_sessions();
                }
                // 窗格重命名（需求2）：按窗格【会话 id】定位焦点 tab 内的
                // 窗格，写其 custom_title（空名 = 清除，回退默认标题）。按 id
                // 而非下标查找——避免后台 shell 异步退出 close_pane 重排下标后
                // 写错窗格（见 pane_renaming 注释）。刷新标题 + 落盘（F4）。
                // 注：OS 窗口标题取 tab 维度（update_window_title 不读窗格自定义
                // 名），调用与 tab 重命名路径保持一致兼作刷新；窗格名生效处在
                // 标题栏（PaneView.title 已优先取 custom_title）。
                if let Some((sid, name)) = shell_out.pane_rename {
                    if let Some(p) = state.tabs[state.active_tab]
                        .panes
                        .iter_mut()
                        .find(|p| p.id == sid)
                    {
                        p.custom_title = (!name.is_empty()).then_some(name);
                    }
                    state.update_window_title();
                    state.persist_sessions();
                }
                if shell_out.new_session {
                    if state.is_mirror_active() {
                        // part3d（需求 d）：远程视图「＋」= 请被控端新建会话（被控端焦点不动），
                        // 成功后自动订阅查看（见 apply_relay 的 NewTabResult）。
                        state.remote_ws.new_remote_tab();
                    } else {
                        state.new_tab();
                    }
                }
                if let Some(id) = shell_out.close {
                    if state.is_mirror_active() {
                        // part3d（需求 d）：远程视图关会话 = 请被控端关该会话；列表 / 订阅回退
                        // 由后续 TabListSnapshot 驱动。
                        state.remote_ws.close_remote_tab(id);
                    } else if let Some(idx) = state.tabs.iter().position(|t| t.id == id) {
                        if state.close_tab(idx) {
                            info!("最后一个会话已关闭，退出应用");
                            event_loop.exit();
                            return; // 不再呈现本帧（应用退出中）
                        }
                    }
                }

                // —— M3.8 自绘标题栏：窗口控制动作处理 ——
                // drag_window / set_minimized / set_maximized 须在 shell::show
                // 同帧（RedrawRequested 内）执行，时序成立（调研 §3 已证）。
                // 无边框窗口边缘 resize 同理在此发起（MouseInput 命中外缘时置位
                // pending_resize_dir）：drag_resize_window 启动系统 resize 模态
                // 循环，失败（如最大化态）静默忽略。
                if let Some(dir) = state.pending_resize_dir.take() {
                    if let Err(e) = state.window.drag_resize_window(dir) {
                        log::debug!("drag_resize_window 失败（忽略）：{e}");
                    }
                }
                if shell_out.drag_title_bar {
                    // drag_window 内部发 WM_NCLBUTTONDOWN + HTCAPTION 启动系统拖动。
                    // 失败（如最大化态下操作）静默忽略——不影响应用逻辑。
                    if let Err(e) = state.window.drag_window() {
                        log::debug!("drag_window 失败（忽略）：{e}");
                    }
                }
                if shell_out.minimize_window {
                    state.window.set_minimized(true);
                }
                if shell_out.toggle_maximize_window {
                    state.window.set_maximized(!state.window.is_maximized());
                }
                if shell_out.close_window {
                    // 关闭窗口：走与 CloseRequested 同路径——落盘后退出。
                    state.persist_sessions();
                    info!("自绘标题栏关闭按钮：落盘后退出");
                    event_loop.exit();
                    return; // 本帧不再继续呈现
                }
                if let Some((lx, ly)) = shell_out.show_window_menu_at {
                    // 逻辑点换算为物理像素，传给 show_window_menu。
                    let scale = state.window.scale_factor();
                    let px = winit::dpi::PhysicalPosition::new(
                        (lx as f64 * scale).round() as i32,
                        (ly as f64 * scale).round() as i32,
                    );
                    state.window.show_window_menu(px);
                }

                // —— M3.8 批2 Snap Layouts：最大化按钮矩形换算为屏幕物理像素 ——
                // egui 逻辑坐标矩形 × pixels_per_point + 窗口客户区屏幕原点
                // = 屏幕物理像素矩形，写入 snap_layouts 原子供子类过程使用。
                //
                // 坐标系说明：
                //   - egui 坐标原点 = 窗口客户区左上角（逻辑像素）。
                //   - 屏幕坐标原点 = 主显示器左上角（物理像素，可为负值）。
                //   - inner_position() 返回客户区左上角的屏幕物理坐标（PhysicalPosition）。
                //   - egui 坐标原点 = 客户区屏幕左上角 = inner_position()（无边框下
                //     NC offset 为 0；最大化时系统将窗口推至约 (-8,-8) 隐藏粗边框，
                //     inner_position 如实反映该值，换算仍正确）。
                //     选用 inner_position 而非 outer_position，是因为 egui 坐标
                //     原点确实对应客户区——无论有无边框、是否最大化都正确。
                #[cfg(target_os = "windows")]
                if let Some(rect) = shell_out.maximize_btn_rect {
                    // inner_position 可能在 Resumed 前失败，用 ok() 静默跳过。
                    if let Ok(origin) = state.window.inner_position() {
                        let ppp = full_output.pixels_per_point;
                        let l = (rect.min.x * ppp).round() as i32 + origin.x;
                        let t = (rect.min.y * ppp).round() as i32 + origin.y;
                        let r = (rect.max.x * ppp).round() as i32 + origin.x;
                        let b = (rect.max.y * ppp).round() as i32 + origin.y;
                        snap_layouts::update_button_rect(l, t, r, b);
                    }
                }

                // —— 窗格级动作（F5 批2）：工具栏「＋」新增 / 窗格 ✕ 关闭
                // （语义同 Ctrl+Shift+D / Ctrl+Shift+W）。结构变更由下方
                // layout_pane_ids 对照检测，本帧跳过矩形应用与终端渲染。
                if shell_out.new_pane {
                    // 远程视图：新建窗格到订阅会话（远程 split，修①），否则本地新建。
                    if state.is_mirror_active() {
                        state.remote_ws.send_new_remote_pane();
                    } else {
                        state.new_pane();
                    }
                }
                if let Some(pi) = shell_out.pane_close {
                    let ti = state.active_tab;
                    // ✕ 仅多窗格时出现，越界/单窗格为防御（关最后一格
                    // 即关 tab，与快捷键同语义）。
                    if pi < state.tabs[ti].panes.len() && state.close_pane(ti, pi) {
                        info!("最后一个会话已关闭，退出应用");
                        event_loop.exit();
                        return; // 不再呈现本帧（应用退出中）
                    }
                }
                // —— 窗格最大化/还原（P14）：标题栏按钮（与
                // Ctrl+Shift+Enter 同语义，toggle 内部含下标防御）。
                if let Some(pi) = shell_out.pane_maximize {
                    state.toggle_maximize_pane(state.active_tab, pi);
                }
                // —— 一键恢复默认布局（P15）：工具栏③复位按钮——全部
                // 比例均分 + 最大化态先退出，复位后落盘。
                if shell_out.layout_reset {
                    state.reset_pane_layout();
                }

                // —— 拖动标题栏换位（F7②）：交换两窗格在 panes 中的
                // 下标——交换的是格子里的「内容」（Session），格子的
                // 几何（布局权重）不动；焦点跟随被拖窗格落位，被换走
                // 的格子若持有焦点则跟去对侧（其余窗格焦点不动）。
                // 下标对应 run_ui 时的布局，结构同帧变更（防御）越界
                // 即跳过；交换后 layout_pane_ids 对照不再一致，本帧
                // 跳过矩形应用、下一帧按新顺序重建（与增删窗格同款
                // 瞬态）。
                if let Some((src, dst)) = shell_out.pane_swap {
                    let tab = &mut state.tabs[state.active_tab];
                    // 最大化期间换位禁用（P14；UI 侧已不发拖动，纯防御）。
                    if src != dst
                        && src < tab.panes.len()
                        && dst < tab.panes.len()
                        && tab.maximized.is_none()
                    {
                        tab.panes.swap(src, dst);
                        if tab.focused == src {
                            tab.focused = dst;
                        } else if tab.focused == dst {
                            tab.focused = src;
                        }
                        // 窗格顺序即持久化顺序：换位是结构性变更，立即
                        // 落盘（沿用既有时机；快照一致时内部自动跳过）。
                        state.update_window_title();
                        state.window.request_redraw();
                        state.persist_sessions();
                    }
                }

                // —— 分隔条调比例（F7③）：拖动把边界拖到指针处（实时
                // 生效——比例变化下一帧产出新矩形，沿用「矩形变化 →
                // 离屏重建 + term/pty resize」既有链路；拖动重绘已被
                // 事件驱动的 8ms 合帧下限节流）；双击恢复该方向均分。
                // 下标对应 run_ui 时的布局，结构同帧变更时由 layout
                // 侧的越界检查兜底（不施加、不 panic）。
                if let Some(kind) = shell_out.divider_reset {
                    let tab = &mut state.tabs[state.active_tab];
                    let changed = match kind {
                        DividerKind::Row(_) => tab.layout.reset_rows(),
                        DividerKind::Col { row, .. } => tab.layout.reset_cols(row),
                    };
                    if changed {
                        state.window.request_redraw();
                        // 双击复位与拖动结束同义：比例落盘（F7 持久化）。
                        state.persist_sessions();
                    }
                } else if let Some((kind, pos)) = shell_out.divider_drag {
                    let area = shell_out.term_rect;
                    let tab = &mut state.tabs[state.active_tab];
                    let changed = match kind {
                        DividerKind::Row(idx) => tab.layout.drag_row_to(idx, pos.y, area),
                        DividerKind::Col { row, idx } => {
                            tab.layout.drag_col_to(row, idx, pos.x, area)
                        }
                    };
                    log::debug!("分隔条拖动: {kind:?} pos={pos:?} changed={changed}");
                    if changed {
                        state.layout_dirty = true;
                        state.window.request_redraw();
                    }
                }
                if shell_out.divider_drag_ended {
                    // 拖动结束才落盘（拖动中不写）；快照一致时内部
                    // 自动跳过。
                    log::debug!("分隔条拖动结束：落盘比例");
                    state.layout_dirty = false;
                    state.persist_sessions();
                }

                // —— M5.3 part3d Phase 3c：镜像分隔条拖动 → 调控制端镜像比例布局（同本地 API，但
                // 作用于 remote_ws 镜像布局而非 tab）。下一帧 mirror_pane_rects 随比例变 →
                // SubViewport 让被控端 resize 到此比例（1:1）；镜像布局不落盘（临时态）。
                // 区域用镜像 area（= term_rect，与 shell 多窗格块算 rects 同源）。
                if let Some(kind) = shell_out.mirror_divider_reset {
                    if let Some(ml) = state.remote_ws.mirror_layout_mut() {
                        let changed = match kind {
                            DividerKind::Row(_) => ml.layout.reset_rows(),
                            DividerKind::Col { row, .. } => ml.layout.reset_cols(row),
                        };
                        if changed {
                            state.window.request_redraw();
                        }
                    }
                } else if let Some((kind, pos)) = shell_out.mirror_divider_drag {
                    let area = shell_out.term_rect;
                    if let Some(ml) = state.remote_ws.mirror_layout_mut() {
                        let changed = match kind {
                            DividerKind::Row(idx) => ml.layout.drag_row_to(idx, pos.y, area),
                            DividerKind::Col { row, idx } => {
                                ml.layout.drag_col_to(row, idx, pos.x, area)
                            }
                        };
                        if changed {
                            state.window.request_redraw();
                        }
                    }
                }

                // —— 需求②：镜像窗格标题栏控件 → 远程操作被控端（PaneOp）。渲染下标 → session_id
                // （mirror_panes 渲染序）；被控端执行后布局变化经 SubscriptionStarted 重发同步回来。
                if state.is_mirror_active() {
                    use lumen_protocol::remote::PaneOpKind;
                    if let Some(tab_id) = state.remote_ws.subscribed_tab() {
                        if let Some(idx) = shell_out.mirror_pane_close {
                            if let Some(sid) = state
                                .remote_ws
                                .mirror_panes()
                                .get(idx)
                                .map(|p| p.session_id)
                            {
                                state.remote_ws.send_pane_op(tab_id, sid, PaneOpKind::Close);
                            }
                        }
                        if let Some(idx) = shell_out.mirror_pane_maximize {
                            if let Some(sid) = state
                                .remote_ws
                                .mirror_panes()
                                .get(idx)
                                .map(|p| p.session_id)
                            {
                                state.remote_ws.send_pane_op(
                                    tab_id,
                                    sid,
                                    PaneOpKind::ToggleMaximize,
                                );
                            }
                        }
                        if let Some((src, dst)) = shell_out.mirror_pane_swap {
                            let a = state
                                .remote_ws
                                .mirror_panes()
                                .get(src)
                                .map(|p| p.session_id);
                            let b = state
                                .remote_ws
                                .mirror_panes()
                                .get(dst)
                                .map(|p| p.session_id);
                            if let (Some(a), Some(b)) = (a, b) {
                                state.remote_ws.send_pane_op(
                                    tab_id,
                                    a,
                                    PaneOpKind::SwapWith { other: b },
                                );
                            }
                        }
                    }
                }

                // —— 覆盖层（设置页/登录页）焦点路由：先处理关闭再处理
                // 打开——登录页关闭时设置页可能仍开着（Account 入口的
                // 叠层场景），后判打开保证焦点不被错误交还终端 ——
                if shell_out.settings_closed || shell_out.login_closed {
                    // 关闭后焦点交还终端（IME 强制复位链路每帧照旧执行）。
                    state.terminal_focused = state.terminal_focus_allowed();
                }

                // —— 历史搜索面板（M4.3）输出处理 ——
                // history_accept：按当前输入模式分流。
                // - Compose 态：填入编辑器（SetText + 光标移末）。
                // - 非 Compose 态（Running / Fallback / AltScreen）：直接写入 PTY，
                //   不带回车，让用户确认后自己回车（验收①）。
                if let Some(text) = shell_out.history_accept {
                    let ti = state.active_tab;
                    let pi = state.tabs[ti].focused;
                    let cur_mode =
                        effective_session_mode(&state.tabs[ti].panes[pi], state.force_fallback);
                    #[cfg(feature = "input-editor")]
                    if cur_mode == mode::InputMode::Compose {
                        state.tabs[ti].panes[pi]
                            .editor
                            .apply(&lumen_editor::EditAction::SetText(text));
                        // 光标移到行末（视觉跟手，与历史导航同款）。
                        state.tabs[ti].panes[pi]
                            .editor
                            .apply(&lumen_editor::EditAction::Move {
                                motion: lumen_editor::Motion::DocEnd,
                                extend: false,
                            });
                    } else {
                        // 非 Compose 态：把命令文本写入 PTY（不含 \r，让用户自己确认）。
                        if let Err(e) = state.tabs[ti].panes[pi].write_user_input(text.as_bytes()) {
                            log::error!("历史搜索填入 PTY 失败: {e:#}");
                        }
                    }
                    #[cfg(not(feature = "input-editor"))]
                    {
                        // 无 input-editor feature 时（理论上不会到此分支，防御性兜底）
                        let _ = cur_mode;
                        if let Err(e) = state.tabs[ti].panes[pi].write_user_input(text.as_bytes()) {
                            log::error!("历史搜索填入 PTY 失败: {e:#}");
                        }
                    }
                    state.shell_state.history_search.open = false;
                    state.terminal_focused = true;
                    state.window.request_redraw();
                }
                // history_closed：关闭面板，焦点还给终端。
                if shell_out.history_closed {
                    state.shell_state.history_search.open = false;
                    state.terminal_focused = true;
                    state.window.request_redraw();
                }
                // history_query_changed：query 变化，下帧重算结果（fuzzy_search 在 run_ui 前算）。
                if shell_out.history_query_changed {
                    state.window.request_redraw();
                }
                // 面板打开期间键盘恒归 egui（终端不收键盘）。
                if state.shell_state.history_search.open {
                    state.terminal_focused = false;
                }

                // —— 补全弹窗（M4.4 批1）输出处理 ——
                // completion_accept：用选定候选的 replacement 替换当前 token。
                // 批2：若候选含 replace_range（命令补全），按其字节区间替换；
                //       否则（文件路径补全）沿用批1 的 current_token 区间逻辑。
                #[cfg(feature = "input-editor")]
                if let Some(idx) = shell_out.completion_accept {
                    if let Some(cand) = state.completion_candidates.get(idx) {
                        let replacement = cand.replacement.clone();
                        let replace_range = cand.replace_range;
                        let ti = state.active_tab;
                        let pi = state.tabs[ti].focused;
                        let cur_line_idx = state.tabs[ti].panes[pi].editor.view().cursor().line;

                        let (sel_start_byte, sel_end_byte) = if let Some((rs, re)) = replace_range {
                            // 命令补全：使用 sidecar 给出的字节区间（已在合并时换算好）。
                            (rs, re)
                        } else {
                            // 文件路径补全：重新计算 current_token 区间（与编辑器当前状态一致）。
                            let view = state.tabs[ti].panes[pi].editor.view();
                            let cur = view.cursor();
                            let line = view.line(cur.line).to_owned();
                            let (start, _) = completion::current_token(&line, cur.byte);
                            (start, cur.byte)
                        };

                        let sel_start_pos = lumen_editor::Position {
                            line: cur_line_idx,
                            byte: sel_start_byte,
                        };
                        let sel_end_pos = lumen_editor::Position {
                            line: cur_line_idx,
                            byte: sel_end_byte,
                        };
                        // 选中替换区间，然后 InsertText 覆盖写入候选文本。
                        state.tabs[ti].panes[pi].editor.apply(
                            &lumen_editor::EditAction::SetSelection(lumen_editor::Selection {
                                anchor: sel_start_pos,
                                cursor: sel_end_pos,
                            }),
                        );
                        state.tabs[ti].panes[pi]
                            .editor
                            .apply(&lumen_editor::EditAction::InsertText(replacement));
                        state.shell_state.completion.open = false;
                        state.shell_state.completion.passive = false;
                        state.completion_candidates.clear();
                        state.completion_req_id = 0; // 取消 sidecar 在途请求（若还有）。
                        state.terminal_focused = true;
                        state.sync_llm_slash_probe(ti, pi);
                        state.window.request_redraw();
                    }
                }
                // completion_closed：关闭弹窗，焦点还给终端。
                if shell_out.completion_closed {
                    state.shell_state.completion.open = false;
                    state.shell_state.completion.passive = false;
                    state.completion_candidates.clear();
                    #[cfg(feature = "input-editor")]
                    {
                        state.completion_req_id = 0; // 丢弃后续 sidecar 响应。
                    }
                    state.terminal_focused = true;
                    state.window.request_redraw();
                }
                // 弹窗打开期间键盘归 egui（终端不收键盘）。
                #[cfg(feature = "input-editor")]
                if state.shell_state.completion.open && !state.shell_state.completion.passive {
                    state.terminal_focused = false;
                }

                // 文件树对话框（新建输入/删除确认）：打开期间键盘/IME
                // 归 egui 的输入框（与重命名编辑同款仲裁）；关闭后交还
                // 终端（与设置页关闭同款，点「取消」也交还）。
                if state.shell_state.filetree.dialog_open() {
                    state.terminal_focused = false;
                } else if shell_out.filetree_dialog_closed {
                    state.terminal_focused = state.terminal_focus_allowed();
                }
                if state.shell_state.text_editor.is_visible() {
                    state.terminal_focused = false;
                    state.filetree_focused = false;
                } else if shell_out.text_editor_closed || shell_out.text_editor_hidden {
                    state.terminal_focused = state.terminal_focus_allowed();
                }
                if shell_out.text_editor_restored {
                    state.terminal_focused = false;
                    state.filetree_focused = false;
                }
                if shell_out.settings_opened
                    || state.shell_state.settings.open
                    || shell_out.login_opened
                    || state.shell_state.login.open
                {
                    // 打开期间键盘/IME 恒归 egui（覆盖层之下终端的 PTY
                    // 消化与渲染照常进行，只是不收键盘）。
                    state.terminal_focused = false;
                }
                if let Some(security_action) = shell_out.settings_security_action {
                    match security_action {
                        shell::settings_ui::SecurityAction::Enable { password } => {
                            state.spawn_lock_crypto(
                                app_lock::CryptoRequest::enable(password),
                                false,
                            );
                        }
                        shell::settings_ui::SecurityAction::Disable {
                            mut current_password,
                        } => {
                            if !state.app_lock.retry_remaining(Instant::now()).is_zero() {
                                use zeroize::Zeroize as _;
                                current_password.zeroize();
                            } else if let Some(verifier) = state.app_lock.protected_verifier() {
                                state.spawn_lock_crypto(
                                    app_lock::CryptoRequest::disable(current_password, verifier),
                                    false,
                                );
                            } else {
                                use zeroize::Zeroize as _;
                                current_password.zeroize();
                                state.shell_state.settings.security_operation_failed();
                            }
                        }
                        shell::settings_ui::SecurityAction::ChangePassword {
                            mut current_password,
                            mut new_password,
                        } => {
                            if !state.app_lock.retry_remaining(Instant::now()).is_zero() {
                                use zeroize::Zeroize as _;
                                current_password.zeroize();
                                new_password.zeroize();
                            } else if let Some(verifier) = state.app_lock.protected_verifier() {
                                state.spawn_lock_crypto(
                                    app_lock::CryptoRequest::change(
                                        current_password,
                                        new_password,
                                        verifier,
                                    ),
                                    false,
                                );
                            } else {
                                use zeroize::Zeroize as _;
                                current_password.zeroize();
                                new_password.zeroize();
                                state.shell_state.settings.security_operation_failed();
                            }
                        }
                        shell::settings_ui::SecurityAction::UpdatePreferences(prefs) => {
                            if let Err(e) = state.app_lock.apply_preferences(prefs) {
                                log::error!("应用锁偏好写盘失败: {e}");
                                state.shell_state.settings.security_operation_failed();
                            }
                            state.window.request_redraw();
                        }
                        shell::settings_ui::SecurityAction::LockNow => {
                            // 本帧 surface 尚未 present；直接结束普通 UI 帧，
                            // 下一次 redraw 只绘制不透明锁屏，避免提交一帧旧业务内容。
                            if state.lock_now() {
                                return;
                            }
                        }
                    }
                }
                if shell_out.settings_font_changed {
                    // 字体/字号即时生效：重建字体度量（行排版缓存随之
                    // 失效）；cell 尺寸变化引发的行列数重算与全会话
                    // resize 在下方矩形检查统一处理（同一帧内完成）。
                    let ap = &state.settings.appearance;
                    let actual = state
                        .renderer
                        .reconfigure_font(&ap.font_family, ap.font_size);
                    state.shell_state.settings.font_hint = (!ap.font_family.is_empty()
                        && !actual.eq_ignore_ascii_case(&ap.font_family))
                    .then(|| {
                        i18n::fmt2(
                            i18n::strings().toast_font_fallback_fmt,
                            &ap.font_family,
                            &actual,
                        )
                    });
                    state.shell_state.text_editor.invalidate_visual_cache();
                }
                if shell_out.settings_theme_changed {
                    // 主题即时生效（P12 画廊点选/槽位变更/Sync 开关
                    // 共用）：按生效主题 id 切终端配色 + 外壳样式。
                    state.apply_theme();
                }
                if shell_out.settings_background_image_changed {
                    // 路径变更/清除/开关：重载纹理，renderer 透明状态同步。
                    state.apply_background_image();
                }
                if shell_out.settings_background_params_changed {
                    // 仅 opacity/dim 变更：不需重载纹理，直接更新透明状态。
                    let enabled =
                        state.settings.appearance.background.enabled && state.bg_texture.is_some();
                    state.renderer.set_transparent_background(enabled);
                }
                // 问题7：顶栏① 会话栏显隐——写入 settings 并触发存盘。
                let sidebar_changed = if let Some(v) = shell_out.toggle_sidebar {
                    state.settings.layout.sidebar_visible = v;
                    true
                } else {
                    false
                };
                // 远程设备栏显隐：按钮只在远程视图出现，状态独立持久化。
                let remote_list_changed = if let Some(v) = shell_out.toggle_remote_list {
                    state.settings.layout.remote_list_visible = v;
                    true
                } else {
                    false
                };
                // SSH 服务器栏显隐：与远程设备栏交互一致，但状态独立持久化。
                let ssh_server_list_changed = if let Some(v) = shell_out.toggle_ssh_server_list {
                    state.settings.layout.ssh_server_list_visible = v;
                    true
                } else {
                    false
                };
                // 第十九轮：顶栏② 文件树显隐——写入 settings 并触发存盘。
                // shell/mod.rs 已在 toggle_filetree 信号路径同步更新
                // ShellState::filetree.visible（两入口共享同一状态源）；
                // 此处只需同步 settings 字段并将 filetree_changed 并入
                // need_save，Ctrl+B 路径自行落盘不走此分支。
                let filetree_changed = if let Some(v) = shell_out.toggle_filetree {
                    state.settings.layout.filetree_visible = v;
                    true
                } else {
                    false
                };
                // SSH 监控面板/卡片显隐：shell 层四入口已更新 SshUiState，
                // 此处把当前状态写进 settings 并触发存盘（重启恢复）。
                let ssh_monitor_prefs_changed = if shell_out.ssh_monitor_prefs_changed {
                    state.settings.layout.ssh_monitor_collapsed =
                        state.shell_state.ssh_ui.monitor_collapsed();
                    state.settings.layout.ssh_monitor_cards_collapsed =
                        state.shell_state.ssh_ui.collapsed_monitor_cards_vec();
                    true
                } else {
                    false
                };
                // 三种工作模式切换 → 写 settings 并触发存盘。
                let view_mode_changed = if let Some(v) = shell_out.toggle_view_mode {
                    state.switch_view_mode(v)
                } else {
                    false
                };
                let need_save = shell_out.settings_font_changed
                    || shell_out.settings_theme_changed
                    || shell_out.settings_background_image_changed
                    || shell_out.settings_background_params_changed
                    || shell_out.settings_language_changed
                    || shell_out.settings_update_changed
                    || shell_out.settings_proxy_changed
                    || shell_out.settings_server_url_changed
                    || shell_out.settings_shortcuts_changed
                    || sidebar_changed
                    || remote_list_changed
                    || ssh_server_list_changed
                    || filetree_changed
                    || ssh_monitor_prefs_changed
                    || view_mode_changed;
                // F3：auto_check 开关改动 → 同步给定时检查线程的原子镜像。
                if shell_out.settings_update_changed {
                    state
                        .update_auto_check
                        .store(state.settings.update.auto_check, Ordering::Relaxed);
                }
                // 网络代理改动 → 刷新生效代理镜像（定时检查线程下轮即用；
                // 手动检查/下载每次从 settings 取，不依赖此镜像）。
                if shell_out.settings_proxy_changed {
                    if let Ok(mut g) = state.update_proxy.lock() {
                        *g = state.settings.proxy.effective_url().map(str::to_owned);
                    }
                }
                // 服务端 origin 改动后旧 token 不可复用：先清登录态和账号
                // worker，再发布新全局地址。重新登录取得新 origin 的 token 后
                // 才会重新加载账号库存并启动 SSH 同步。
                if shell_out.settings_server_url_changed {
                    let previous_origin = cloud::server_url();
                    let next_origin =
                        cloud::canonical_server_origin(&state.settings.server_url).ok();
                    if next_origin.as_deref() != Some(previous_origin.as_str()) {
                        state.invalidate_account_for_server_url_change();
                    }
                    cloud::set_server_url(&state.settings.server_url);
                }
                // F3：设置页「检查更新」按钮 → 手动检查（无更新/失败也回 toast）。
                // 手动检查清掉「已消解/已知版本」状态：用户主动检查即视为想重新
                // 看到更新（绕过 drain 的同版本去重），故之前点过「稍后」/下载失败
                // 消解的弹窗能再次弹出（重试入口）。
                if shell_out.update_check_now {
                    state.update_available = None;
                    state.update_dismissed = false;
                    // 清掉可能残留的就绪包（从干净态重新检查；旧包删除，重新
                    // 检查若仍是该版本会重下覆盖）——避免 ready 残留而 available
                    // 被清致弹窗门恒真但内容缺失、且留孤儿临时文件。
                    if let Some(p) = state.update_ready.take() {
                        let _ = std::fs::remove_file(p);
                    }
                    state.shell_state.toast.push(
                        shell::toast::ToastKind::Info,
                        i18n::strings().update_toast_checking.to_owned(),
                    );
                    state.spawn_update_check(true);
                }
                // 头像菜单「更新到 vX」：重新显示已就绪的更新弹窗（清 dismissed）。
                if shell_out.open_update {
                    state.update_dismissed = false;
                    state.window.request_redraw();
                }
                // 头像菜单资源组：打开 GitHub 页（复用 links::open / 系统默认浏览器）。
                if shell_out.open_whats_new {
                    links::open(&links::LinkTarget::Url(format!(
                        "https://github.com/{}/releases",
                        update::GITHUB_REPO
                    )));
                }
                if shell_out.open_documentation {
                    links::open(&links::LinkTarget::Url(format!(
                        "https://github.com/{}#readme",
                        update::GITHUB_REPO
                    )));
                }
                if shell_out.open_feedback {
                    links::open(&links::LinkTarget::Url(format!(
                        "https://github.com/{}/issues",
                        update::GITHUB_REPO
                    )));
                }
                if need_save {
                    // 变更即写盘（写临时文件后改名，防半写损坏）。失败
                    // 弹 toast：用户以为改完即存，静默丢失重启才发现。
                    if let Some(err) = state.settings.save() {
                        state.shell_state.toast.push(
                            shell::toast::ToastKind::Error,
                            i18n::fmt1(i18n::strings().toast_settings_save_failed_fmt, &err),
                        );
                        // push 发生在本帧 egui 布局之后：请求下一帧立即显示。
                        state.window.request_redraw();
                    }
                }

                // —— 登录/登出动作：state.profile 是唯一数据源，更新后
                // 顶栏头像、头像菜单、设置页 Account 三处下一帧即联动 ——
                if let Some(p) = shell_out.logged_in {
                    state.stop_account_bound_workers();
                    // 登录成功：原子写盘（重启保持登录态）+ 更新内存态。
                    p.save();
                    info!("登录成功：{} <{}>", p.display_name, p.email);
                    state.shell_state.toast.push(
                        shell::toast::ToastKind::Info,
                        i18n::fmt1(i18n::strings().toast_logged_in_fmt, &p.display_name),
                    );
                    // push 发生在本帧 egui 布局之后：请求下一帧立即显示。
                    state.window.request_redraw();
                    state.profile = Some(p);
                    state.reload_ssh_account_context();
                }
                if shell_out.logged_out {
                    // 登出：删 profile.json，三处 UI 即时回未登录态。
                    state.stop_account_bound_workers();
                    profile::Profile::delete();
                    info!("已登出（profile.json 已删除）");
                    state.profile = None;
                    state.reload_ssh_account_context();
                    // 片6：登出后被控端不可达，清掉系统剪贴板里指向它的虚拟文件，免得粘贴空等失败。
                    if let Some(svc) = state.clipboard_svc.as_ref() {
                        svc.clear();
                    }
                }

                // —— 文件树动作：双击目录 cd / 双击文件系统默认程序
                // 打开（注入目标 = 焦点窗格）——
                // part3c-2：控制端远程树交互（只读渲染收集，此处以 &mut 施加）——
                // 目录点击 → 翻转展开（纯本地，未缓存则发 ListDir）；显示隐藏项 → 重列根。
                for id in shell_out.remote_dir_clicks {
                    state.remote_ws.remote_dir_clicked(id);
                }
                if let Some(id) = shell_out.remote_refresh_dir {
                    state.remote_ws.remote_refresh_dir(id);
                }
                if let Some(id) = shell_out.remote_select {
                    state.remote_ws.set_remote_selected(id);
                }
                if shell_out.remote_clear_select {
                    state.remote_ws.clear_remote_selected();
                }
                if let Some(show) = shell_out.remote_toggle_hidden {
                    state.remote_ws.set_remote_show_hidden(show);
                }
                // #5：双击远程文件 → 起 Fetch（传到控制端临时文件 → 本地默认程序打开）。
                if let Some(path) = shell_out.remote_fetch_open {
                    state.remote_ws.start_fetch_open(path);
                }
                // 右键“编辑”始终进入 Lumen 内置编辑器；双击仍是下载本地副本后
                // 交给系统默认编辑器，两条保存链路不会混用。
                if let Some((path, _name, _size)) = shell_out.remote_edit_file {
                    state.open_text_editor(shell::text_editor::TextFileSource::Remote {
                        generation: state.remote_ws.edit_generation(),
                        path,
                    });
                }
                // 复制：本地文件 → **系统剪贴板**（CF_HDROP，与资源管理器及任意应用互通，海风哥
                // 反馈核心）；远程文件路径在被控端、进不了系统剪贴板 → 存 Lumen 内部（仅供下载到本地）。
                if let Some((side, path, name, is_dir, size)) = shell_out.file_copy {
                    state.ssh_file_clipboard = None;
                    log::info!(
                        "[文件剪贴板] 复制: side={side:?} is_dir={is_dir} size={size} path={path}"
                    );
                    match side {
                        remote_ws::ClipSide::Local => {
                            let ok =
                                clipboard_files::copy_files(&[std::path::PathBuf::from(&path)]);
                            // 清 Lumen 内部远程剪贴板：避免随后本地粘贴误走「下载」分支（系统优先）。
                            state.remote_ws.clear_file_clipboard();
                            // 片6：本地复制已抢占系统剪贴板（CF_HDROP），让 OLE 线程释放可能残留的
                            // 我方虚拟文件对象引用，避免后续判属/泄漏。
                            if let Some(svc) = state.clipboard_svc.as_ref() {
                                svc.clear();
                            }
                            state.shell_state.toast.push(
                                if ok {
                                    shell::toast::ToastKind::Info
                                } else {
                                    shell::toast::ToastKind::Error
                                },
                                if ok {
                                    i18n::fmt1(i18n::strings().remote_copied_fmt, 1)
                                } else {
                                    i18n::strings().local_copy_clipboard_failed.to_string()
                                },
                            );
                        }
                        remote_ws::ClipSide::Remote => {
                            // 片6/8：单文件 → 即时系统剪贴板虚拟文件；目录 → 先 clear 关竞态、起
                            // 递归枚举，枚举完成（ClipDirReady）才 set 多文件 descriptor。
                            if let Some(svc) = state.clipboard_svc.as_ref() {
                                svc.clear();
                            }
                            if is_dir {
                                state.remote_ws.start_clip_dir(path.clone(), name.clone());
                            } else {
                                state.remote_ws.cancel_clip_dir();
                                if let Some(svc) = state.clipboard_svc.as_ref() {
                                    svc.set_remote_file(path.clone(), name.clone(), size);
                                }
                            }
                            state.remote_ws.set_file_clipboard(
                                side,
                                vec![remote_ws::ClipItem { path, name, is_dir }],
                            );
                            let msg = if is_dir {
                                i18n::strings().remote_clip_dir_preparing.to_string()
                            } else {
                                i18n::fmt1(i18n::strings().remote_copied_fmt, 1)
                            };
                            state
                                .shell_state
                                .toast
                                .push(shell::toast::ToastKind::Info, msg);
                        }
                    }
                }
                // 粘贴：按目标侧定方向。
                // - 粘贴到本地目录：Lumen 内部有远程项 → 下载；否则系统剪贴板文件 → 本机复制。
                // - 粘贴到远程目录（上传）：系统剪贴板本地文件 → 上传到被控端。
                if let Some((target_side, dir)) = shell_out.file_paste {
                    state.do_file_paste(target_side, dir);
                }
                // #7 / 片5：覆盖模态选择 → 续传 / 跳过 / 取消。先看待决下载（pending_paste），
                // 否则路由到待决上传冲突（resolve_upload_conflict）。两者互斥（同一模态）。
                if let Some(choice) = shell_out.overwrite_choice {
                    if let Some(p) = state.pending_paste.take() {
                        // 按方向路由：本机复制 → start_local_copy；下载 → start_download。
                        let is_local = p.local;
                        match choice {
                            shell::OverwriteChoice::Overwrite => {
                                if is_local {
                                    state.start_local_copy(p.items, p.dest_dir, true);
                                } else {
                                    state.remote_ws.start_download(p.items, p.dest_dir, true);
                                }
                            }
                            shell::OverwriteChoice::Skip => {
                                if is_local {
                                    state.start_local_copy(p.items, p.dest_dir, false);
                                } else {
                                    state.remote_ws.start_download(p.items, p.dest_dir, false);
                                }
                            }
                            shell::OverwriteChoice::Cancel => {}
                        }
                    } else if let Some(pending) = state.pending_ssh_download.take() {
                        if choice == shell::OverwriteChoice::Overwrite {
                            state.start_ssh_download_to_local(
                                pending.item,
                                pending.destination,
                                true,
                            );
                        }
                    } else {
                        state.remote_ws.resolve_upload_conflict(choice);
                    }
                }
                if let Some(dir) = shell_out.cd_dir {
                    // UI 已按 shell 空闲闸门过滤，这里直接注入。
                    let cmd = shell::filetree::cd_command(&dir);
                    let s = state.focused_pane_mut();
                    s.term.grid_mut().scroll_to_bottom();
                    if let Err(e) = s.write_user_input(&cmd) {
                        error!("写入 PTY 失败: {e:#}");
                    }
                    // cd 后把键盘/IME 焦点交还终端，用户可直接继续敲命令。
                    state.terminal_focused = true;
                }
                if let Some(file) = shell_out.open_file {
                    shell::filetree::open_with_default(&file);
                }
                // —— 文件树拖放：把路径文本插入**落点所在窗格**的命令
                // 行（不带回车；F5 批2 拍板：拖放目标 = 鼠标落点窗格，
                // 落点不在任何窗格时 shell 侧已过滤为 None）。先聚焦落
                // 点窗格——插入后接着编辑命令行的就是它。转义与 cd 注
                // 入同一套设施（弯引号同形字/控制字符防御见
                // filetree::path_insert_text；空字节串 = 路径被拒绝）。
                //
                // 第二十一轮分流（与 d9444c6 Ctrl+V 分流同构）：
                // Compose 态 → dispatch Edit(InsertText) 进 footer 编辑器；
                // Running / AltScreen / Fallback → 原写 PTY 路径不变。
                // dispatch 内实时查 effective_mode，防落点窗格聚焦瞬间
                // 与执行时刻的模式漂移。
                if let Some((pi, path)) = shell_out.insert_path {
                    state.insert_path_into_pane(pi, &path);
                }
                // —— 文件树右键菜单：复制绝对/相对路径到剪贴板 ——
                if let Some(text) = shell_out.copy_text {
                    let ok = matches!(
                        state.clipboard.as_mut().map(|c| c.set_text(text.clone())),
                        Some(Ok(()))
                    );
                    if ok {
                        state.shell_state.toast.push(
                            shell::toast::ToastKind::Info,
                            i18n::fmt1(i18n::strings().toast_copied_fmt, &text),
                        );
                    } else {
                        error!("写剪贴板失败（复制路径）");
                        state.shell_state.toast.push(
                            shell::toast::ToastKind::Error,
                            i18n::strings().toast_copy_failed,
                        );
                    }
                    // push 发生在本帧 egui 布局之后：请求下一帧立即显示。
                    state.window.request_redraw();
                }
                // 远程菜单：新建文件夹/文件确认 → 发协议给被控端（结果回来刷新目录）。
                if let Some((dir, name, is_dir)) = shell_out.remote_create {
                    if is_dir {
                        state.remote_ws.remote_make_dir(dir, name);
                    } else {
                        state.remote_ws.remote_make_file(dir, name);
                    }
                    state.window.request_redraw();
                }
                // 远程菜单：重命名确认 → 发协议（被控端同目录改名，结果回来刷新父目录）。
                if let Some((path, new_name)) = shell_out.remote_rename {
                    state.remote_ws.remote_rename(path, new_name);
                    state.window.request_redraw();
                }
                // 远程菜单：删除确认 → 发协议（被控端移入回收站，结果回来刷新父目录）。
                if let Some((path, is_dir)) = shell_out.remote_delete {
                    state.remote_ws.remote_delete(path, is_dir);
                    state.window.request_redraw();
                }
                if let Some((session_id, dir, name, is_dir)) = shell_out.ssh_create {
                    if let Err(error) = state
                        .ssh_runtime
                        .create_entry(session_id, dir, &name, is_dir)
                    {
                        state
                            .shell_state
                            .toast
                            .push(shell::toast::ToastKind::Error, error);
                    }
                    state.window.request_redraw();
                }
                if let Some((session_id, path, is_dir)) = shell_out.ssh_delete {
                    if let Err(error) = state.ssh_runtime.delete_entry(session_id, path, is_dir) {
                        state
                            .shell_state
                            .toast
                            .push(shell::toast::ToastKind::Error, error);
                    }
                    state.window.request_redraw();
                }
                // SSH 菜单：重命名确认 → SFTP rename（同目录；撞名由服务端回 Conflict）。
                if let Some((session_id, path, new_name, is_dir)) = shell_out.ssh_rename {
                    if let Err(error) = state
                        .ssh_runtime
                        .rename_entry(session_id, path, is_dir, &new_name)
                    {
                        state
                            .shell_state
                            .toast
                            .push(shell::toast::ToastKind::Error, error);
                    }
                    state.window.request_redraw();
                }
                // 远程菜单「进入文件夹」= 命令行 cd：注入 `cd '<被控端路径>'` 到远程会话
                // （与本地一致，走 send_input → InputWithId → 被控端 PTY）。未在镜像某个
                // 远程终端时无处注入 → 提示而非静默无效。控制字符路径被 cd_command_raw 拒。
                if let Some(dir) = shell_out.remote_cd {
                    let cmd = shell::filetree::cd_command_raw(&dir);
                    if !cmd.is_empty() {
                        if state.remote_ws.send_input(&cmd) {
                            state.terminal_focused = true;
                        } else {
                            state.shell_state.toast.push(
                                shell::toast::ToastKind::Info,
                                i18n::strings().remote_cd_no_terminal.to_string(),
                            );
                        }
                        state.window.request_redraw();
                    }
                }
                if let Some(request) = shell_out.text_editor_load {
                    state.start_text_editor_load(request);
                }
                if let Some(request) = shell_out.text_editor_save {
                    state.start_text_editor_save(request);
                }

                // —— 窗格矩形（物理像素）变化 → 逐窗格重建离屏 + resize ——
                // 对各边按 epaint 同款语义取整后求宽高：分数 DPI（如
                // 125%）下布局矩形的物理尺寸可为分数，纹理尺寸若单独
                // round 会与呈现 quad 差出 0.5px——Nearest 采样在区中部
                // 复制/丢一行 texel（1px 接缝游走）。shell 侧已把矩形
                // round_to_pixels（见 shell/mod.rs），三者同源后纹理与
                // 屏上 quad 像素数严格相等（1:1 映射），pane_rects_px
                // （鼠标/IME 映射）的 ±0.5px 系统偏差也一并消除。
                //
                // 本帧布局的矩形对应 run_ui 时的窗格列表；上方动作（关
                // tab/增删窗格/切 tab）可能已改变结构——结构变了就跳过
                // 矩形应用与终端渲染（egui 呈现旧画面一帧，与 activate
                // 的「先切再补帧」同款瞬态），请求下一帧按新结构重来。
                let ppp = full_output.pixels_per_point;
                if state.settings.layout.view_mode.is_ssh() {
                    state.ssh_rect_px = shell_out.ssh_terminal_rect.and_then(|rect| {
                        let (width, height) = (rect.width(), rect.height());
                        (width.is_finite() && height.is_finite() && width >= 1.0 && height >= 1.0)
                            .then(|| {
                                let x0 = (rect.min.x * ppp).round();
                                let y0 = (rect.min.y * ppp).round();
                                let x1 = (rect.max.x * ppp).round();
                                let y1 = (rect.max.y * ppp).round();
                                (x0, y0, x1 - x0, y1 - y0)
                            })
                    });
                    if let Some((_, _, width, height)) = state.ssh_rect_px {
                        let texture_width = width.max(1.0) as u32;
                        let texture_height = height.max(1.0) as u32;
                        if state.renderer.ensure_offscreen(
                            SSH_OFFSCREEN_ID,
                            texture_width,
                            texture_height,
                        ) {
                            if let (Some(view), Some(texture)) = (
                                state.renderer.offscreen_view(SSH_OFFSCREEN_ID),
                                state.ssh_texture,
                            ) {
                                state.egui_renderer.update_egui_texture_from_wgpu_texture(
                                    state.renderer.device(),
                                    view,
                                    wgpu::FilterMode::Nearest,
                                    texture,
                                );
                            }
                        }
                        let (rows, columns) =
                            state.renderer.grid_size_for(texture_width, texture_height);
                        state.ssh_runtime.resize_active(rows, columns);
                    }
                } else {
                    state.ssh_rect_px = None;
                }
                // 面板拖宽手柄命中区（P10）：raw 鼠标让位判定用
                // （mouse_on_panel_resize）。与窗格结构无关，无条件
                // 按本帧布局更新（文件树收起时本帧为空 = 不让位）。
                state.panel_resize_rects_px.clear();
                for r in &shell_out.panel_resize_rects {
                    state.panel_resize_rects_px.push((
                        r.min.x * ppp,
                        r.min.y * ppp,
                        r.width() * ppp,
                        r.height() * ppp,
                    ));
                }
                // —— 侧栏宽度持久化（P10）：egui 面板自管宽度（本帧
                // 实际值经 shell_out 报回），这里只负责落盘——指针
                // 松开（拖动结束）且与已存值差 ≥1px 才写（判定抽
                // width_worth_persisting，B1 单测覆盖）；窗口过窄被
                // 临时压缩到范围之外的瞬态宽度不写（重启还原用户最后
                // 一次主动调整的值）。
                if !state.egui_ctx.input(|i| i.pointer.any_down()) {
                    let lay = &mut state.settings.layout;
                    let mut width_changed = false;
                    let sw = shell_out.sidebar_width;
                    if width_worth_persisting(
                        sw,
                        lay.sidebar_width,
                        settings::SIDEBAR_WIDTH_MIN,
                        settings::SIDEBAR_WIDTH_MAX,
                    ) {
                        log::debug!("侧栏宽度落盘：{} → {sw}", lay.sidebar_width);
                        lay.sidebar_width = sw;
                        width_changed = true;
                    }
                    if let Some(fw) = shell_out.filetree_width {
                        if width_worth_persisting(
                            fw,
                            lay.filetree_width,
                            settings::FILETREE_WIDTH_MIN,
                            settings::FILETREE_WIDTH_MAX,
                        ) {
                            log::debug!("文件树宽度落盘：{} → {fw}", lay.filetree_width);
                            lay.filetree_width = fw;
                            width_changed = true;
                        }
                    }
                    if width_changed {
                        // 失败弹 toast（与字体/主题写盘同款）：用户以为
                        // 拖完即存，静默丢失重启才发现。
                        if let Some(err) = state.settings.save() {
                            state.shell_state.toast.push(
                                shell::toast::ToastKind::Error,
                                // F6：与 L2636 字体/主题/语言写盘失败路径保持一致。
                                i18n::fmt1(i18n::strings().toast_settings_save_failed_fmt, &err),
                            );
                            state.window.request_redraw();
                        }
                    }
                    // 比例写盘兜底（B1 加固）：drag_stopped 在边角场景
                    // 可能错失（拖动中窗口失焦等），拖动改过比例且指针
                    // 已松开就补一次落盘（快照一致时内部自动跳过，
                    // 正常路径无重复写）。
                    if state.layout_dirty {
                        state.layout_dirty = false;
                        log::debug!("比例写盘兜底：指针已松开且布局有未落盘变更");
                        state.persist_sessions();
                    }
                }
                // —— 启动首帧的布局应用值日志（B1 恢复面验收）：加载
                // 日志只证明文件读到了值，这里输出 UI 实际用上的值
                // （egui 面板实际宽度 + 激活 tab 实际权重），一次性。
                if !state.layout_apply_logged {
                    state.layout_apply_logged = true;
                    let t = &state.tabs[state.active_tab];
                    info!(
                        "外壳布局应用：侧栏宽 {:.1}（设置 {:.1}）文件树宽 {:?}（设置 {:.1}）窗格权重 rows={:?} cols={:?} 最大化={:?}",
                        shell_out.sidebar_width,
                        state.settings.layout.sidebar_width,
                        shell_out.filetree_width,
                        state.settings.layout.filetree_width,
                        t.layout.row_weights(),
                        t.layout.col_weights(),
                        t.maximized,
                    );
                }
                let structure_unchanged = state.tabs.get(state.active_tab).is_some_and(|t| {
                    t.panes.len() == layout_pane_ids.len()
                        && t.panes
                            .iter()
                            .zip(&layout_pane_ids)
                            .all(|(p, id)| p.id == *id)
                });
                // 分隔条拖动期间暂缓 term/PTY resize（B2 修复）：旧行为
                // 逐帧 resize 是对 ConPTY 的整批重绘风暴，PSReadLine 的
                // 差量渲染跨 resize 即坐标失步——提示符丢字、回显错位
                // 混叠（症状②）的直接温床，且逐帧触发缩行。拖动中纹理
                // 照常随矩形重建（边界视觉跟手，内容暂按旧行列呈现），
                // 松手（drag_stopped）那一帧本判定即为 false，下方矩形
                // 对照一次性提交 resize。
                //
                // B3-8 修正：整窗 resize（WindowEvent::Resized）必须穿透
                // 此门控——整窗 resize 是 OS 级行为，与分隔条拖动完全
                // 独立；若 egui 指针/拖动状态因窗口失焦或系统接管未被
                // 正常清除，divider_drag 可能持续为 Some 但无法靠
                // drag_stopped 清零，导致整窗 resize 的 term/PTY resize
                // 被永久阻断（海风哥 B3-8 现象：拖过分隔条后放大整窗，
                // 文字仍按旧窄宽折行）。window_just_resized 标志在
                // WindowEvent::Resized 置位，本帧用后清零（单次消耗）。
                let window_resized_this_frame = state.window_just_resized;
                state.window_just_resized = false;
                let divider_resize_held = !window_resized_this_frame
                    && shell_out.divider_drag.is_some()
                    && !shell_out.divider_drag_ended;
                if window_resized_this_frame && shell_out.divider_drag.is_some() {
                    log::debug!(
                        "B3-8：整窗 resize 帧检测到 divider_drag.is_some()（拖动状态滞留），\
                         强制穿透 held 门控，确保 term/PTY resize 提交"
                    );
                }
                if structure_unchanged {
                    // 窗格关闭按钮命中区（F5 批2）：raw 鼠标路由的让位
                    // 判定用（mouse_on_pane_close）。
                    state.pane_close_rects_px.clear();
                    for r in &shell_out.pane_close_rects {
                        state.pane_close_rects_px.push((
                            r.min.x * ppp,
                            r.min.y * ppp,
                            r.width() * ppp,
                            r.height() * ppp,
                        ));
                    }
                    // 分隔条命中区（F7③）：raw 鼠标路由的让位判定用
                    // （mouse_on_pane_divider）。
                    state.divider_rects_px.clear();
                    for r in &shell_out.divider_rects {
                        state.divider_rects_px.push((
                            r.min.x * ppp,
                            r.min.y * ppp,
                            r.width() * ppp,
                            r.height() * ppp,
                        ));
                    }
                    state.pane_rects_px.clear();
                    for (i, r) in shell_out.pane_rects.iter().enumerate() {
                        // 隐藏窗格（P14 最大化）的矩形是退化占位
                        // （egui::Rect::NOTHING，宽高为 -∞）：不重建离屏/不
                        // resize（保持隐藏前的网格，后台输出按原尺寸消化）、
                        // 不进鼠标/IME 路由表。
                        //
                        // 判定**以矩形本身为准、绝不读实时 `maximized` 状态**
                        // ——这是「还原后其余窗格只剩每行首字符」串扰 bug 的
                        // 根因修复：pane_rects 是 egui 按 run_ui 起点的
                        // input.maximized 快照产出的权威几何，最大化态下隐藏
                        // 窗格即 NOTHING。但点标题栏「还原」按钮会在**同一帧
                        // run_ui 内**把 maximized 改回 None（见
                        // shell_out.pane_maximize 处理），此刻本帧 pane_rects
                        // 仍是改前的最大化布局（隐藏窗格仍 NOTHING）——若按
                        // 实时 maximized 判跳过就会漏掉这些窗格，NOTHING 的
                        // 负宽高被下方 .max(1.0) 夹成 1×1，把隐藏窗格的 grid
                        // resize 成 1 列：row.resize(1) 截断每行到首字符、
                        // scrollback 历史右侧内容永久丢失（海风哥实测截图）。
                        // 退化矩形跳过、留待下一帧（maximized 变更必伴
                        // request_redraw）按正确分屏矩形 resize。快捷键
                        // Ctrl+Shift+Enter 改在事件层、下一帧才 run_ui，input
                        // 与判定同源故不触发，仅按钮路径中招。
                        let (rw, rh) = (r.width(), r.height());
                        if !(rw.is_finite() && rh.is_finite() && rw >= 1.0 && rh >= 1.0) {
                            continue;
                        }
                        let x0 = (r.min.x * ppp).round();
                        let y0 = (r.min.y * ppp).round();
                        let x1 = (r.max.x * ppp).round();
                        let y1 = (r.max.y * ppp).round();
                        let sid = state.tabs[state.active_tab].panes[i].id;
                        state.pane_rects_px.push((sid, (x0, y0, x1 - x0, y1 - y0)));
                        let tw = (x1 - x0).max(1.0) as u32;
                        let th = (y1 - y0).max(1.0) as u32;
                        if state.renderer.ensure_offscreen(sid, tw, th) {
                            // 原地换绑：TextureId 不变，本帧 egui pass 即
                            // 采样新视图。
                            if let (Some(view), Some(tex)) = (
                                state.renderer.offscreen_view(sid),
                                state.pane_textures.get(&sid),
                            ) {
                                state.egui_renderer.update_egui_texture_from_wgpu_texture(
                                    state.renderer.device(),
                                    view,
                                    wgpu::FilterMode::Nearest,
                                    *tex,
                                );
                            }
                            // 新建的纹理是空的：本帧必须渲染该窗格，否则
                            // egui 采样到全黑（即使正处同步区间，半成品也
                            // 好过黑屏闪烁）。
                            skip_pane[i] = false;
                        }
                        // 行列数同时受窗格矩形与 cell 尺寸（设置页字体/
                        // 字号）影响，每帧对照网格检测（廉价的整数比较）。
                        // 分屏后各窗格尺寸不同：逐窗格 resize（term +
                        // PTY）。后台 tab 的窗格不在布局里、不在此 resize
                        // ——切换激活的首帧先走到这里 resize 再渲染，旧
                        // 行列的画面不会上屏。设置页改字号即时生效走的就
                        // 是这条链路：cell 尺寸变 → 行列数变 → resize。
                        //
                        // M4.1 批C：footer 扣高（feature = "input-editor"）。
                        // 聚焦窗格按当前模式计算 footer 高度；非聚焦窗格无 footer。
                        // AltScreen / Fallback 隐藏 footer（footer_px=0）→ 一进一出
                        // 各一次 resize，与整窗 resize 走同一路径（window_just_resized
                        // 豁免已覆盖，见 B3-8 注释），不额外处理。
                        // 常驻等高铁律：Compose↔Running footer_px 相同 → 不触发 resize。
                        #[cfg(feature = "input-editor")]
                        let footer_px_for_resize = {
                            let pane_idx = i;
                            let is_focused = state.tabs[state.active_tab].focused == pane_idx;
                            if is_focused {
                                let pane = &state.tabs[state.active_tab].panes[i];
                                let mode = effective_session_mode(pane, state.force_fallback);
                                let mut cv = composer::compose_view_for_mode(
                                    mode,
                                    pane.editor.view(),
                                    pane.preedit.clone(),
                                    pane.exit_badge.clone(),
                                    None, // ghost 仅用于渲染，resize 高度计算不需要
                                );
                                cv.attachment_count = pane.attachments.len();
                                let (cell_w, cell_h) = state.renderer.cell_size();
                                let fp = state.renderer.padding() * 0.4;
                                cv.soft_wrap(
                                    lumen_renderer::composer_view::footer_wrap_columns(
                                        tw as f32,
                                        cell_w,
                                        fp,
                                    ),
                                );
                                let max_h = th as f32 / 3.0;
                                let target_h = lumen_renderer::composer_view::footer_height_px(
                                    Some(&cv),
                                    cell_h,
                                    fp,
                                    max_h,
                                );
                                // M4.1 批D2：增高防抖（100ms）。
                                // 目标高度变化时更新 footer_target_h 和 changed_at。
                                let s = &mut state.tabs[state.active_tab].panes[i];
                                if (target_h - s.footer_target_h).abs() >= 0.5 {
                                    s.footer_target_h = target_h;
                                    s.footer_h_changed_at = Instant::now();
                                }
                                // 纯函数判定：是否允许提交给 renderer/resize。
                                let should_commit = history::footer_height_debounce(
                                    s.footer_committed_h,
                                    s.footer_target_h,
                                    s.footer_h_changed_at,
                                    Instant::now(),
                                );
                                if should_commit {
                                    s.footer_committed_h = s.footer_target_h;
                                }
                                s.footer_committed_h
                            } else {
                                0.0_f32
                            }
                        };
                        #[cfg(not(feature = "input-editor"))]
                        let footer_px_for_resize: f32 = 0.0;
                        // M5.3 SSH 式视口跟随（优先级从高到低）：
                        // ① 被订阅会话的窗格 → 控制端接管的网格（`SubViewport`，按**控制电脑**
                        //    的镜像格像素 + 字号算出）。本机窗口矩形在订阅期间不再改它的网格，
                        //    否则前台 tab 每帧都被本机尺寸夺回、两端抢 resize。
                        // ② 单窗格远程视口（`ViewportResize`，焦点窗格）。
                        // ③ 非被控 / 未接管 → 本机窗格矩形算。
                        let sid_now = state.tabs[state.active_tab].panes[i].id;
                        let owned_dims =
                            state.controller_owned_grid(state.tabs[state.active_tab].id, sid_now);
                        let (rows, cols) = match (owned_dims, state.remote_viewport) {
                            (Some(dims), _) => dims,
                            (None, Some(dims)) if i == state.tabs[state.active_tab].focused => dims,
                            _ => state.renderer.grid_size_for_with_footer(
                                tw,
                                th,
                                footer_px_for_resize,
                            ),
                        };
                        // M4.1 批C 冒烟观测点：首帧可见 footer 占高生效。
                        // 日志示例：「footer 占高 32px，网格 {rows}x{cols}
                        //            （无 footer 时多 1-2 行）」
                        if footer_px_for_resize > 0.0 {
                            log::debug!(
                                "M4.1 批C：窗格 id={} footer 占高 {:.0}px \
                                 → 网格 {rows}x{cols}（窗格 {tw}x{th}）",
                                state.tabs[state.active_tab].panes[i].id,
                                footer_px_for_resize
                            );
                        }
                        let s = &mut state.tabs[state.active_tab].panes[i];
                        let (old_rows, old_cols) = {
                            let g = s.term.grid();
                            (g.rows(), g.cols())
                        };
                        if divider_resize_held && (rows, cols) != (old_rows, old_cols) {
                            // B3-8 诊断：分隔条拖动中暂缓 resize，记录被挡
                            // 的尺寸变化，帮助取证 held 是否误触。
                            log::debug!(
                                "B3-8 诊断：窗格 id={} 网格变化 {old_rows}x{old_cols} → \
                                 {rows}x{cols} 因 divider_resize_held=true 暂缓",
                                s.id
                            );
                        }
                        if !divider_resize_held && (rows, cols) != (old_rows, old_cols) {
                            // 观测点（B2）：幅度可核对恢复路径估算的精度
                            // ——估算到位时首帧 resize 应为 ±1 级微调。
                            log::debug!(
                                "窗格 id={} 网格 {old_rows}x{old_cols} → {rows}x{cols}",
                                s.id
                            );
                            s.term.resize(rows, cols);
                            // resize 失败 = term 与 ConPTY 几何失步（丢字
                            // /错位的温床），必须可观测（B2 修复：不再
                            // `let _ =` 静默吞掉）。
                            if let Err(e) = s.pty.resize(rows as u16, cols as u16) {
                                log::warn!(
                                    "窗格 id={} 的 PTY resize 到 {rows}x{cols} 失败: {e:#}",
                                    s.id
                                );
                            }
                            // B3-7：已知限制——窄窗格提示符折行后经历宽度变化，
                            // 当前提示符行打字会错位至用户回车自愈。根因为
                            // PSReadLine 上游缺陷（锚点不随解折行重测，WT #2432/#15042
                            // 同款），终端侧无非侵入手段，接受现状。
                            // resize 后注入 \r 的方案（B3-5/B3-5b/B3-6）经海风哥实测
                            // 否决：会产生多余提示符行，已全部拆除。
                            // 尺寸变化会夹紧光标位置，立即同步绘制态。
                            let g = s.term.grid();
                            s.cursor_displayed = (g.cursor.row, g.cursor.col, g.cursor.visible);
                            // 网格已重排（字号变更等可不伴随纹理重建）：
                            // 旧帧内容与新行列数不匹配，本帧必须渲染。
                            skip_pane[i] = false;
                        }
                    }
                } else {
                    state.window.request_redraw();
                }

                #[cfg(feature = "input-editor")]
                for (i, pane) in state.tabs[state.active_tab].panes.iter().enumerate() {
                    if !pane.slash_probe.shadow.is_empty() {
                        // 原生 CLI 菜单仅作为命令数据源：探测前缀写入到
                        // Ctrl+U 擦除完成之间保留上一张稳定终端纹理，避免
                        // 原生菜单和 Lumen 弹层同时出现在屏幕上。
                        skip_pane[i] = true;
                    }
                }

                // —— 终端管线渲染到各窗格离屏纹理（damage/行缓存机制
                // 原样，行缓存按会话 id 隔离）——同步区间门控跳过的窗
                // 格不渲染：其纹理保留上一完整帧，egui pass 照常采样
                // 合成（渲染计划在途，ESU 后补画）。
                let mut rendered = 0usize;
                if structure_unchanged {
                    // M4.1 批C：按当前有效模式组装 ComposerView（feature = "input-editor"）。
                    // 节拍纪律（设计稿 §7.4）：编辑器重绘直接 request_redraw，
                    // 不挂 PTY debounce。此处仅按模式组装视图数据，无副作用。
                    #[cfg(feature = "input-editor")]
                    let footer_view = {
                        // M4.1 批3：ghost text 缓存（revision 变化时重算）。
                        // 先独立更新缓存（不持有 focused 借用），再组装视图。
                        {
                            let ti2 = state.active_tab;
                            let pi2 = state.tabs[ti2].focused;
                            let rev = state.tabs[ti2].panes[pi2].editor.revision();
                            if state.ghost_cache.0 != rev {
                                let text =
                                    state.tabs[ti2].panes[pi2].editor.view().text().to_owned();
                                let ghost = if text.contains('\n') || text.is_empty() {
                                    log::debug!(
                                        "[ghost_cache] 跳过：text 为空或多行 len={} has_nl={}",
                                        text.len(),
                                        text.contains('\n')
                                    );
                                    None
                                } else {
                                    let g = state.history.find_ghost_prefix(&text);
                                    log::debug!(
                                        "[ghost_cache] rev={rev} text={:?} ghost={:?}",
                                        text,
                                        g
                                    );
                                    g
                                };
                                state.ghost_cache = (rev, ghost);
                            }
                        }
                        let ghost = state.ghost_cache.1.clone();
                        let footer_wrap_cols = {
                            let pane_width = state
                                .focused_pane_rect_px()
                                .map_or(state.window.inner_size().width as f32, |(_, _, w, _)| w);
                            let (cell_width, _) = state.renderer.cell_size();
                            let footer_padding = state.renderer.padding() * 0.4;
                            lumen_renderer::composer_view::footer_wrap_columns(
                                pane_width,
                                cell_width,
                                footer_padding,
                            )
                        };
                        let focused = state.focused_pane();
                        let mode = effective_session_mode(focused, state.force_fallback);
                        let mut view = composer::compose_view_for_mode(
                            mode,
                            focused.editor.view(),
                            focused.preedit.clone(),
                            focused.exit_badge.clone(),
                            ghost,
                        );
                        view.attachment_count = focused.attachments.len();
                        // LLM CLI 的输入是自然语言/Markdown，不是 PowerShell。
                        // 尤其图片标签 `[#1]` 会让 shell lexer 把 `#1]...`
                        // 误判为注释并染成很暗的 ansi[8]，导致正文近乎不可读。
                        if focused
                            .llm_cli
                            .or_else(|| llm_cli::detect(None, &focused.term))
                            .is_some()
                        {
                            view.highlight.clear();
                        }
                        view.soft_wrap(footer_wrap_cols);
                        view
                    };

                    for (i, skip) in skip_pane.iter().enumerate() {
                        if *skip {
                            continue;
                        }
                        let s = &mut state.tabs[state.active_tab].panes[i];
                        s.term_frame_due_since = None;
                        let s = &state.tabs[state.active_tab].panes[i];
                        // 防抖光标态整组传入：不可见时行号仍是运行中块
                        // 状态条的下边界（与光标同源防抖，块条几何帧间
                        // 连续）。
                        // M4.1 批C：feature = "input-editor" 开启时用
                        // render_with_composer 传入 footer 视图；flag 剔除时用 render。
                        // 只有聚焦窗格显示 footer；非聚焦窗格传 None = 无 footer。
                        // 批D 起按各窗格独立模式组装（多窗格各自有 footer）。
                        // F10：本窗格命中的链接 hover 区段（abs 行, 起列, 止列）。
                        let link_hover = state
                            .hovered_link
                            .as_ref()
                            .filter(|h| h.pane_id == s.id)
                            .map(|h| (h.line, h.start_col, h.end_col));
                        #[cfg(feature = "input-editor")]
                        let render_result = {
                            let composer_view = if state.tabs[state.active_tab].focused == i {
                                Some(&footer_view)
                            } else {
                                None
                            };
                            state.renderer.render_with_composer(
                                s.id,
                                &s.term,
                                s.selection.as_ref(),
                                s.cursor_displayed,
                                s.selected_block,
                                link_hover,
                                composer_view,
                            )
                        };
                        #[cfg(not(feature = "input-editor"))]
                        let render_result = state.renderer.render(
                            s.id,
                            &s.term,
                            s.selection.as_ref(),
                            s.cursor_displayed,
                            s.selected_block,
                            link_hover,
                        );
                        if let Err(e) = render_result {
                            error!("渲染失败: {e:#}");
                        }
                        rendered += 1;
                    }
                }
                if state.settings.layout.view_mode.is_ssh()
                    && state.ssh_texture.is_some()
                    && state.ssh_rect_px.is_some()
                {
                    if let Some(terminal) = state.ssh_runtime.active_terminal_mut() {
                        terminal.grid_mut().take_dirty();
                    }
                    let cursor = state.ssh_runtime.active_cursor().unwrap_or((0, 0, false));
                    let (renderer, runtime) = (&mut state.renderer, &state.ssh_runtime);
                    if let Some(terminal) = runtime.active_terminal() {
                        if let Err(error) =
                            renderer.render(SSH_OFFSCREEN_ID, terminal, None, cursor, None, None)
                        {
                            log::error!("SSH 终端渲染失败: {error:#}");
                        } else {
                            rendered += 1;
                        }
                    }
                }
                if rendered > 0 {
                    // ESU 直渲限频基准（整帧粒度，多窗格共享）。
                    state.last_term_render_at = Some(render_t0);
                }

                // 镜像离屏必须【不透明】：远程屏幕是实心内容，不该透出本地窗格。开了背景图时
                // 全局 transparent_background=true 会让镜像随离屏 Clear 变透明，下层本地窗格的
                // 命令行就透过镜像显出来（海风哥实测：release 配了背景图触发；debug 无背景图不
                // 复现；单窗格+控制中）。故渲染镜像前临时关透明、渲染后恢复——不影响已渲染完的
                // 本地窗格纹理，也不影响无背景图的默认路径。
                let force_opaque_mirror = state.is_mirror_active()
                    && state.settings.appearance.background.enabled
                    && state.bg_texture.is_some();
                if force_opaque_mirror {
                    state.renderer.set_transparent_background(false);
                }

                // —— M5.3 part3b/part3d 控制端镜像渲染：把镜像 Terminal 画进保留 id 的离屏
                // 纹理（wgpu 上色，复用窗格渲染器；控制端主题就地解析颜色）。
                if state.is_mirror_active() {
                    // part3d：渲染源由 RemoteWs 决定——跟随态借 live 镜像 + 真实光标，
                    // 回看态借按需填好的历史窗口 scratch + 隐藏光标。
                    if let Some(frame) = state.remote_ws.mirror_render() {
                        let cur = frame.cursor;
                        let term = frame.term;
                        let sel = frame.selection; // part4b 镜像选区高亮
                                                   // part3d：离屏尺寸取**镜像网格的自然像素**（网格×cell + 四周 padding），
                                                   // 使被控端整屏完整渲染进纹理、不裁底行（被控端屏比控制端大时尤其关键）；
                                                   // shell 端 Image 再缩放铺满终端区（替代已移除的 SSH 视口跟随）。
                        let (cell_w, cell_h) = state.renderer.cell_size();
                        let pad = state.renderer.padding();
                        let (grows, gcols) = {
                            let g = term.grid();
                            (g.rows(), g.cols())
                        };
                        let w = ((gcols as f32 * cell_w + pad * 2.0).round() as u32).max(1);
                        let h = ((grows as f32 * cell_h + pad * 2.0).round() as u32).max(1);
                        // 尺寸变化时重建离屏并重绑 egui 纹理（句柄不变）。
                        if state.renderer.ensure_offscreen(MIRROR_OFFSCREEN_ID, w, h) {
                            if let (Some(view), Some(tex)) = (
                                state.renderer.offscreen_view(MIRROR_OFFSCREEN_ID),
                                state.mirror_texture,
                            ) {
                                state.egui_renderer.update_egui_texture_from_wgpu_texture(
                                    state.renderer.device(),
                                    view,
                                    wgpu::FilterMode::Nearest,
                                    tex,
                                );
                            }
                        }
                        if let Err(e) =
                            state
                                .renderer
                                .render(MIRROR_OFFSCREEN_ID, term, sel, cur, None, None)
                        {
                            error!("镜像渲染失败: {e:#}");
                        }
                    }
                }

                // —— M5.3 part3d Phase 3c 多窗格镜像渲染：每窗格镜像 Terminal 画进各自离屏纹理
                // （离屏尺寸取该窗格网格自然像素；shell 在 pane_rects 处缩放铺放）。光标仅焦点窗格显示。
                if state.is_mirror_active() && !state.remote_ws.mirror_panes().is_empty() {
                    let ppp = state.egui_ctx.pixels_per_point();
                    let n = state.remote_ws.mirror_panes().len();
                    let focused_idx = state.remote_ws.mirror_active_pane_idx();
                    // Phase 4 焦点窗格回看：回看态先按焦点窗格网格拉缺失历史 + 建 hist_term scratch
                    // （之后焦点窗格改渲 hist_term）。非回看态无操作。
                    if state.remote_ws.mirror_pane_in_hist() {
                        if let Some(fi) = focused_idx {
                            let (rows, cols) = {
                                let g = state.remote_ws.mirror_panes()[fi].term.grid();
                                (g.rows(), g.cols())
                            };
                            state.remote_ws.prepare_focused_pane_hist(rows, cols);
                        }
                    }
                    for i in 0..n {
                        // 离屏尺寸 = shell 回传的**该格内容矩形像素**（控制端 cell 大小渲染、1:1 贴图：
                        // 文字恒定清晰不缩放）。被控端网格大于该格 → 渲染时自然裁右/下；小于 → 留白。
                        let rect = shell_out
                            .mirror_pane_rects
                            .get(i)
                            .copied()
                            .unwrap_or(egui::Rect::NOTHING);
                        if !(rect.width() >= 1.0 && rect.height() >= 1.0) {
                            continue; // 隐藏（最大化）/ 无效格不渲染。
                        }
                        let w = ((rect.width() * ppp).round() as u32).max(1);
                        let h = ((rect.height() * ppp).round() as u32).max(1);
                        let oid = mirror_pane_offscreen_id(i);
                        // 焦点窗格在回看态 → 渲染 hist_term scratch（光标隐藏）；否则渲染 live 窗格 term。
                        let in_hist_here =
                            focused_idx == Some(i) && state.remote_ws.mirror_pane_in_hist();
                        let term = match (in_hist_here, state.remote_ws.mirror_hist_term()) {
                            (true, Some(ht)) => ht,
                            _ => &state.remote_ws.mirror_panes()[i].term,
                        };
                        let cur = if in_hist_here {
                            (0, 0, false) // 回看态光标隐藏。
                        } else {
                            let g = term.grid();
                            (g.cursor.row, g.cursor.col, g.cursor.visible)
                        };
                        // per-pane 选区高亮（part4b 多窗格；Selection 为 Copy）。
                        let sel_i = state.remote_ws.mirror_panes()[i].selection;
                        // F10 链接 hover 下划线：回看态（in_hist_here）坐标系不一致不画；
                        // 否则仅给命中窗格（pane_id==该镜像窗格 session_id）传区段。与
                        // 本地渲染循环同法，复用同一条 renderer link_hover 路径。
                        let link_hover_i = if in_hist_here {
                            None
                        } else {
                            let sid_i = state.remote_ws.mirror_panes()[i].session_id;
                            state
                                .hovered_link
                                .as_ref()
                                .filter(|h| h.pane_id == sid_i)
                                .map(|h| (h.line, h.start_col, h.end_col))
                        };
                        if state.renderer.ensure_offscreen(oid, w, h) {
                            if let (Some(view), Some(&tex)) = (
                                state.renderer.offscreen_view(oid),
                                state.mirror_pane_textures.get(i),
                            ) {
                                state.egui_renderer.update_egui_texture_from_wgpu_texture(
                                    state.renderer.device(),
                                    view,
                                    wgpu::FilterMode::Nearest,
                                    tex,
                                );
                            }
                        }
                        if let Err(e) = state.renderer.render(
                            oid,
                            term,
                            sel_i.as_ref(),
                            cur,
                            None,
                            link_hover_i,
                        ) {
                            error!("多窗格镜像渲染失败: {e:#}");
                        }
                    }
                }
                // 恢复透明背景：供下一帧本地窗格离屏 Clear 透出 egui 背景图（仅本帧镜像期间关过）。
                if force_opaque_mirror {
                    state.renderer.set_transparent_background(true);
                }

                // —— egui 平台输出 + IME 强制复位（IME 最大坑对策）——
                // egui 会按自己的文本焦点开关整窗 IME / 挪动候选框；终端
                // 聚焦时必须在 handle_platform_output **之后**强制复位，
                // 并把候选框钉在**焦点窗格**光标所在格子（窗格原点 +
                // cell×行列；首帧矩形未知时跳过本帧定位，允许位仍复位）。
                state
                    .egui_state
                    .handle_platform_output(&state.window, full_output.platform_output);
                // 终端聚焦时强制把 IME 候选框复位到光标处（纠正 egui 本帧对
                // 候选框/整窗 IME 的挪动）。同一逻辑也在焦点回来后首个
                // Ime::Enabled 立即调用一次，修「失焦再回来首字缩最左」。
                state.update_ime_cursor_area(false);

                // —— egui 渲染到 surface（单 pass，Clear 装载）——
                let clipped = state.egui_ctx.tessellate(full_output.shapes, ppp);
                let (sw, sh) = state.renderer.surface_size();
                let screen = egui_wgpu::ScreenDescriptor {
                    size_in_pixels: [sw, sh],
                    pixels_per_point: ppp,
                };
                let device = state.renderer.device();
                let queue = state.renderer.queue();
                for (id, delta) in &full_output.textures_delta.set {
                    state
                        .egui_renderer
                        .update_texture(device, queue, *id, delta);
                }
                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("lumen egui frame"),
                });
                let user_cmds = state.egui_renderer.update_buffers(
                    device,
                    queue,
                    &mut encoder,
                    &clipped,
                    &screen,
                );
                let surface_view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                {
                    let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("lumen egui pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &surface_view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(
                                    state.renderer.theme().background.to_wgpu(),
                                ),
                                store: wgpu::StoreOp::Store,
                            },
                            depth_slice: None,
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    // egui 的 render() 要求 'static 生命周期 pass；
                    // forget_lifetime 之后不得再操作父 encoder。
                    let mut pass = pass.forget_lifetime();
                    state.egui_renderer.render(&mut pass, &clipped, &screen);
                }
                queue.submit(user_cmds.into_iter().chain([encoder.finish()]));
                frame.present();
                for id in &full_output.textures_delta.free {
                    state.egui_renderer.free_texture(id);
                }
                // 已关闭窗格的纹理注销（呈现后才安全：关闭动作发生在
                // run_ui 之后时，本帧 shape 仍引用该纹理 id）。
                for id in state.pending_tex_free.drain(..) {
                    state.egui_renderer.free_texture(&id);
                }

                // —— egui 重绘计划：与终端节拍在 about_to_wait 合流 ——
                let repaint_delay = full_output
                    .viewport_output
                    .get(&egui::ViewportId::ROOT)
                    .map_or(Duration::MAX, |v| v.repaint_delay);
                // 仅记录异常值（动画/立即重绘请求），用于空转监控。
                if repaint_delay < Duration::from_secs(3600) {
                    state.perf_log(format_args!("egui repaint_delay {repaint_delay:?}"));
                }
                state.egui_repaint_at = if repaint_delay == Duration::ZERO {
                    // 动画进行中要求立即重绘——但同样受 8ms 合帧下限
                    // 约束：帧尾直接 request_redraw 会形成「画完即请求
                    // 下一帧」的紧循环（实测启动动画期间每 ~0.4ms 一帧、
                    // 千帧每秒级白占主线程）。改排计划由 about_to_wait
                    // 统一调度，动画以 ~125fps 推进（视觉无差异）。
                    Some(render_t0 + Duration::from_millis(8))
                } else if repaint_delay < Duration::from_secs(3600) {
                    Some(render_t0 + repaint_delay)
                } else {
                    None // 「无限远」：无需主动重绘
                };

                // —— 埋点（沿用 M2 字段，便于打字延迟基线对比）——
                let gap = state
                    .last_render_at
                    .map(|t| render_t0.duration_since(t))
                    .unwrap_or_default();
                state.last_render_at = Some(render_t0);
                let key_to_screen = state
                    .last_key_at
                    .take()
                    .map(|t| format!(" 键→上屏 {:?}", t.elapsed()))
                    .unwrap_or_default();
                let skipped = skip_pane.iter().filter(|s| **s).count();
                let term_mark = if !structure_unchanged {
                    " 终端=跳过(结构变更)".to_owned()
                } else if skipped > 0 {
                    format!(" 终端跳过 {skipped}/{} 窗格(同步区间)", skip_pane.len())
                } else {
                    String::new()
                };
                state.perf_log(format_args!(
                    "render 耗时 {:?} 距上帧 {gap:?}{key_to_screen}{term_mark}",
                    render_t0.elapsed()
                ));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_ssh_account_id, clear_shared_token, clipboard_changed_since,
        controller_owned_pane_grid, drain_order, estimate_restored_pane_px,
        filetree_clipboard_shortcut, load_icon, maximized_overflow, next_ssh_clipboard_generation,
        profile_auth_token, profile_origin_requires_reauth, profile_server_origin,
        scroll_to_bottom_action, should_apply_ssh_sync_event, should_continue_ssh_sync,
        should_trigger_ssh_sync_after_local_change, ssh_clipboard_batch_directory,
        ssh_clipboard_export_is_current, ssh_host_key_confirmation_is_current,
        ssh_private_key_submission_is_valid, ssh_profile_matches_test_target, ssh_sync_identity,
        ssh_test_profile, view_mode_shortcut, width_worth_persisting, FileTreeClipboardShortcut,
        PaneLayout, ScrollToBottomAction,
    };
    use winit::event::ElementState;
    use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};

    #[cfg(feature = "input-editor")]
    use super::{composer_should_insert_newline, llm_cli_native_navigation_passthrough};

    // ── 本机复制粘贴（local→local，海风哥本轮新增）单测 ───────────────────
    use super::{local_copy_item, unique_copy_name, CopyStats};

    #[test]
    fn 订阅会话窗格网格由控制端接管_其余回退本机() {
        let mut sizes = std::collections::HashMap::new();
        sizes.insert(7_u64, (40_usize, 120_usize));
        let owned = (3_u64, sizes);
        // 订阅会话内、控制端上报过的窗格：用控制端算出的网格（前台后台一视同仁）。
        assert_eq!(
            controller_owned_pane_grid(Some(&owned), 3, 7),
            Some((40, 120))
        );
        // 本机其他会话：不受远控接管，按自身窗口矩形算。
        assert_eq!(controller_owned_pane_grid(Some(&owned), 4, 7), None);
        // 订阅后新开、控制端还没上报的窗格：本帧先按本机矩形算。
        assert_eq!(controller_owned_pane_grid(Some(&owned), 3, 8), None);
        // 未被控 / 已断开：接管表为空。
        assert_eq!(controller_owned_pane_grid(None, 3, 7), None);
    }

    #[test]
    fn 工作模式快捷键_精确匹配_ctrl_shift_数字键() {
        let modifiers = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(
            view_mode_shortcut(modifiers, PhysicalKey::Code(KeyCode::Digit1)),
            Some(crate::settings::ViewMode::Local)
        );
        assert_eq!(
            view_mode_shortcut(modifiers, PhysicalKey::Code(KeyCode::Digit2)),
            Some(crate::settings::ViewMode::Remote)
        );
        assert_eq!(
            view_mode_shortcut(modifiers, PhysicalKey::Code(KeyCode::Digit3)),
            Some(crate::settings::ViewMode::Ssh)
        );
    }

    #[test]
    fn 工作模式快捷键_额外修饰键或其他按键不匹配() {
        let modifiers = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(
            view_mode_shortcut(
                modifiers | ModifiersState::ALT,
                PhysicalKey::Code(KeyCode::Digit3)
            ),
            None
        );
        assert_eq!(
            view_mode_shortcut(modifiers, PhysicalKey::Code(KeyCode::KeyS)),
            None
        );
    }

    #[cfg(feature = "input-editor")]
    #[test]
    fn llm空编辑器方向键与esc直通原生选择器() {
        for key in [
            KeyCode::ArrowUp,
            KeyCode::ArrowDown,
            KeyCode::ArrowLeft,
            KeyCode::ArrowRight,
            KeyCode::Escape,
        ] {
            assert!(llm_cli_native_navigation_passthrough(
                true,
                true,
                false,
                ModifiersState::default(),
                PhysicalKey::Code(key),
            ));
        }
    }

    #[cfg(feature = "input-editor")]
    #[test]
    fn llm导航直通不抢本地文本或lumen补全() {
        let up = PhysicalKey::Code(KeyCode::ArrowUp);
        assert!(!llm_cli_native_navigation_passthrough(
            true,
            false,
            false,
            ModifiersState::default(),
            up,
        ));
        assert!(!llm_cli_native_navigation_passthrough(
            true,
            true,
            true,
            ModifiersState::default(),
            up,
        ));
        assert!(!llm_cli_native_navigation_passthrough(
            false,
            true,
            false,
            ModifiersState::default(),
            up,
        ));
    }

    #[cfg(feature = "input-editor")]
    #[test]
    fn llm与图片草稿的enter不触发shell自动续行() {
        assert!(!composer_should_insert_newline(true, false, true));
        assert!(!composer_should_insert_newline(false, true, true));
        assert!(composer_should_insert_newline(false, false, true));
        assert!(!composer_should_insert_newline(false, false, false));
    }

    #[test]
    fn 文件树复制粘贴快捷键在ssh终端路由之前精确匹配() {
        assert_eq!(
            filetree_clipboard_shortcut(
                ElementState::Pressed,
                false,
                ModifiersState::CONTROL,
                PhysicalKey::Code(KeyCode::KeyC),
                true,
            ),
            Some(FileTreeClipboardShortcut::Copy)
        );
        assert_eq!(
            filetree_clipboard_shortcut(
                ElementState::Pressed,
                false,
                ModifiersState::CONTROL,
                PhysicalKey::Code(KeyCode::KeyV),
                true,
            ),
            Some(FileTreeClipboardShortcut::Paste)
        );
    }

    #[test]
    fn 文件树复制粘贴不抢终端组合键和文本输入控件() {
        for (repeat, modifiers, available) in [
            (true, ModifiersState::CONTROL, true),
            (false, ModifiersState::CONTROL | ModifiersState::SHIFT, true),
            (false, ModifiersState::CONTROL, false),
        ] {
            assert_eq!(
                filetree_clipboard_shortcut(
                    ElementState::Pressed,
                    repeat,
                    modifiers,
                    PhysicalKey::Code(KeyCode::KeyC),
                    available,
                ),
                None
            );
        }
        assert_eq!(
            filetree_clipboard_shortcut(
                ElementState::Released,
                false,
                ModifiersState::CONTROL,
                PhysicalKey::Code(KeyCode::KeyV),
                true,
            ),
            None
        );
    }

    #[test]
    fn ssh文件剪贴板代次跳过零且只认最新完成() {
        assert_eq!(next_ssh_clipboard_generation(0), 1);
        assert_eq!(next_ssh_clipboard_generation(u64::MAX), 1);
        assert!(ssh_clipboard_export_is_current(7, 7));
        assert!(!ssh_clipboard_export_is_current(6, 7));
        assert!(!ssh_clipboard_export_is_current(0, 0));
        assert!(!clipboard_changed_since(Some(12), Some(12)));
        assert!(clipboard_changed_since(Some(12), Some(13)));
        assert!(!clipboard_changed_since(None, None));
        assert!(clipboard_changed_since(None, Some(13)));
        assert!(clipboard_changed_since(Some(12), None));
    }

    #[test]
    fn ssh文件剪贴板每批使用独立二级暂存目录() {
        let root = std::path::Path::new(r"C:\Temp\lumen_ssh_clipboard-42");
        let first = ssh_clipboard_batch_directory(root, 3, 9, 100);
        let second = ssh_clipboard_batch_directory(root, 4, 9, 100);
        assert_eq!(first, root.join("3-9-100"));
        assert_eq!(second, root.join("4-9-100"));
        assert_ne!(first, second);
    }

    #[test]
    fn ssh测试连接使用独立profile且凭据回退按结构化目标匹配() {
        let draft = crate::ssh::NewSshProfile {
            name: "test".to_owned(),
            host: "b@c".to_owned(),
            username: "a".to_owned(),
            auth_method: crate::ssh::AuthMethod::Password,
            monitor_enabled: true,
            ..Default::default()
        };
        let saved = ssh_test_profile(41, &draft);
        assert_eq!(saved.id, "ssh_00000000000000000000000000000029");
        assert!(!saved.monitor_enabled, "Probe 不得启动服务器监控");
        assert!(ssh_profile_matches_test_target(&saved, &saved));

        let mut colliding = saved.clone();
        colliding.host = "c".to_owned();
        colliding.username = "a@b".to_owned();
        assert_eq!(
            format!("{}@{}", saved.username, saved.host),
            format!("{}@{}", colliding.username, colliding.host),
            "回归样例必须能碰撞旧展示字符串"
        );
        assert!(!ssh_profile_matches_test_target(&saved, &colliding));
    }

    #[test]
    fn ssh私钥表单提交在写库存前严格校验本机文件与复用目标() {
        let mut draft = crate::ssh::NewSshProfile {
            name: "key-server".to_owned(),
            host: "server.example.test".to_owned(),
            username: "alice".to_owned(),
            auth_method: crate::ssh::AuthMethod::PrivateKey,
            ..Default::default()
        };
        assert!(!ssh_private_key_submission_is_valid(
            &draft, None, None, None
        ));
        assert!(!ssh_private_key_submission_is_valid(
            &draft,
            None,
            None,
            Some(std::path::Path::new("relative-key")),
        ));
        assert!(!ssh_private_key_submission_is_valid(
            &draft,
            None,
            None,
            Some(std::env::temp_dir().as_path()),
        ));

        let key_path =
            std::env::temp_dir().join(format!("lumen-main-key-validation-{}", std::process::id()));
        std::fs::write(&key_path, b"test-key").expect("创建临时私钥");
        assert!(ssh_private_key_submission_is_valid(
            &draft,
            None,
            None,
            Some(&key_path),
        ));

        let saved = ssh_test_profile(73, &draft);
        let binding = crate::ssh::SshLocalBinding {
            profile_id: saved.id.clone(),
            private_key_path: Some(key_path.clone()),
            password_credential_ref: None,
            key_passphrase_credential_ref: None,
        };
        assert!(ssh_private_key_submission_is_valid(
            &draft,
            Some(&saved),
            Some(&binding),
            None,
        ));

        draft.host = "other.example.test".to_owned();
        assert!(!ssh_private_key_submission_is_valid(
            &draft,
            Some(&saved),
            Some(&binding),
            None,
        ));
        let _ = std::fs::remove_file(key_path);
    }

    #[test]
    fn ssh主机密钥并发确认不能覆盖已信任的不同指纹() {
        let trusted = crate::ssh::HostKeyTrust {
            algorithm: "ssh-ed25519".to_owned(),
            fingerprint: "SHA256:first".to_owned(),
        };
        assert!(ssh_host_key_confirmation_is_current(
            None,
            "ssh-ed25519",
            "SHA256:first",
        ));
        assert!(ssh_host_key_confirmation_is_current(
            Some(&trusted),
            "ssh-ed25519",
            "SHA256:first",
        ));
        assert!(!ssh_host_key_confirmation_is_current(
            Some(&trusted),
            "ssh-ed25519",
            "SHA256:second",
        ));
        assert!(!ssh_host_key_confirmation_is_current(
            Some(&trusted),
            "rsa-sha2-512",
            "SHA256:first",
        ));
    }

    #[test]
    fn 登录来源迁移只在明确不一致时要求重新认证() {
        let mut profile = crate::profile::Profile {
            token: Some("jwt-token".to_owned()),
            ..Default::default()
        };
        assert!(
            !profile_origin_requires_reauth(&profile, "https://lumen.example"),
            "旧版缺 auth_origin 应先迁移，不能删除"
        );
        assert!(
            !profile_origin_requires_reauth(&profile, ""),
            "当前配置暂不可用时应保留旧档案"
        );
        assert!(profile.migrate_legacy_auth_origin("https://lumen.example"));
        assert_eq!(
            profile_server_origin(Some(&profile), "https://lumen.example").as_deref(),
            Some("https://lumen.example")
        );
        let migrated_token =
            profile_auth_token(Some(&profile), "https://lumen.example").expect("远程 token");
        assert_eq!(migrated_token.read().unwrap().as_str(), "jwt-token");

        assert!(!profile_origin_requires_reauth(
            &profile,
            "HTTPS://LUMEN.EXAMPLE:443/"
        ));
        assert!(profile_origin_requires_reauth(
            &profile,
            "https://other.example"
        ));

        profile.auth_origin = Some("not-a-valid-origin".to_owned());
        assert!(profile_origin_requires_reauth(
            &profile,
            "https://lumen.example"
        ));
    }

    #[test]
    fn ssh同步身份只接受规范账号_非空token和服务端地址() {
        let mut profile = crate::profile::Profile {
            user_id: Some("550E8400-E29B-41D4-A716-446655440000".to_owned()),
            token: Some("jwt-token".to_owned()),
            auth_origin: Some("https://lumen.example".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            canonical_ssh_account_id(Some(&profile), "https://lumen.example").as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            ssh_sync_identity(Some(&profile), "https://lumen.example"),
            Some((
                "550e8400-e29b-41d4-a716-446655440000".to_owned(),
                "https://lumen.example".to_owned()
            ))
        );

        profile.token = Some("   ".to_owned());
        assert!(ssh_sync_identity(Some(&profile), "https://lumen.example").is_none());
        profile.token = Some("jwt-token".to_owned());
        assert!(ssh_sync_identity(Some(&profile), "   ").is_none());
        assert!(ssh_sync_identity(Some(&profile), "https://other.example").is_none());
        profile.auth_origin = None;
        assert!(ssh_sync_identity(Some(&profile), "https://lumen.example").is_none());
        profile.auth_origin = Some("https://lumen.example".to_owned());
        profile.user_id = Some("not-a-canonical-account".to_owned());
        assert!(canonical_ssh_account_id(Some(&profile), "https://lumen.example").is_none());
        assert!(ssh_sync_identity(Some(&profile), "https://lumen.example").is_none());
    }

    #[test]
    fn ssh本地变更只触发同账号worker() {
        let account = "550e8400-e29b-41d4-a716-446655440000";
        let other = "550e8400-e29b-41d4-a716-446655440001";
        assert!(should_trigger_ssh_sync_after_local_change(
            Some(account),
            Some(account)
        ));
        assert!(!should_trigger_ssh_sync_after_local_change(
            Some(account),
            Some(other)
        ));
        assert!(!should_trigger_ssh_sync_after_local_change(
            Some(account),
            None
        ));
        assert!(!should_trigger_ssh_sync_after_local_change(
            None,
            Some(account)
        ));
    }

    #[test]
    fn ssh同步事件须同时匹配worker与当前库存账号() {
        let account = "550e8400-e29b-41d4-a716-446655440000";
        let stale = "550e8400-e29b-41d4-a716-446655440001";
        assert!(should_apply_ssh_sync_event(
            Some(account),
            Some(account),
            account
        ));
        assert!(!should_apply_ssh_sync_event(
            Some(account),
            Some(account),
            stale
        ));
        assert!(!should_apply_ssh_sync_event(
            Some(account),
            Some(stale),
            account
        ));
        assert!(!should_apply_ssh_sync_event(Some(account), None, account));
    }

    #[test]
    fn ssh同步完成后仅有更多页或待发变更才续触发() {
        assert!(!should_continue_ssh_sync(false, 0));
        assert!(should_continue_ssh_sync(true, 0));
        assert!(should_continue_ssh_sync(false, 1));
        assert!(should_continue_ssh_sync(true, 1));
    }

    #[test]
    fn ssh账号切换会原地清空所有共享token句柄() {
        let token = std::sync::Arc::new(std::sync::RwLock::new("old-token".to_owned()));
        let worker_view = token.clone();
        clear_shared_token(&token);
        assert!(token.read().unwrap().is_empty());
        assert!(worker_view.read().unwrap().is_empty());
    }

    #[test]
    fn 回到底部动作_主屏沿用真实display_offset() {
        assert_eq!(
            scroll_to_bottom_action(25, 24, false, 0),
            Some(ScrollToBottomAction::LocalGrid)
        );
        assert_eq!(scroll_to_bottom_action(24, 24, false, 0), None);
    }

    #[test]
    fn 回到底部动作_备用屏依赖应用滚动距离估算() {
        assert_eq!(
            scroll_to_bottom_action(0, 24, true, 25),
            Some(ScrollToBottomAction::AlternateApp)
        );
        assert_eq!(scroll_to_bottom_action(0, 24, true, 24), None);
        assert_eq!(
            scroll_to_bottom_action(0, 24, false, 25),
            None,
            "1007 关闭、退出备用屏或开启鼠标上报时不能注入 End"
        );
    }

    #[test]
    fn 副本名_不冲突原样_冲突加序号() {
        let base = lc_tmp("uniq");
        // 不存在 → 原样返回。
        assert_eq!(unique_copy_name(&base, "a.txt", false), "a.txt");
        // 存在 → a (1).txt（保留扩展名）。
        std::fs::write(base.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_copy_name(&base, "a.txt", false), "a (1).txt");
        // a (1).txt 也在 → a (2).txt。
        std::fs::write(base.join("a (1).txt"), b"x").unwrap();
        assert_eq!(unique_copy_name(&base, "a.txt", false), "a (2).txt");
        // 目录：整体加后缀（不切扩展名）。
        std::fs::create_dir(base.join("d")).unwrap();
        assert_eq!(unique_copy_name(&base, "d", true), "d (1)");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 建唯一临时根目录（进程 id + 标签区分，各测试互不撞），测完自行清理。
    fn lc_tmp(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("lumen_lc_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("建测试根");
        base
    }

    #[test]
    fn 本机复制_单文件_落地正确() {
        let base = lc_tmp("file");
        let src = base.join("a.txt");
        std::fs::write(&src, b"hello").unwrap();
        let dst_dir = base.join("dst");
        std::fs::create_dir_all(&dst_dir).unwrap();
        let mut st = CopyStats::default();
        local_copy_item(&dst_dir.join("a.txt"), &src, false, true, 0, &mut st);
        assert_eq!((st.done, st.skipped, st.errors), (1, 0, 0));
        assert_eq!(std::fs::read(dst_dir.join("a.txt")).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 本机复制_撞名_不覆盖跳过_覆盖替换() {
        let base = lc_tmp("conflict");
        let src = base.join("src.txt");
        std::fs::write(&src, b"NEW").unwrap();
        let dst = base.join("d").join("src.txt");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::write(&dst, b"OLD").unwrap();
        // 不覆盖 → 跳过，旧内容保留。
        let mut st = CopyStats::default();
        local_copy_item(&dst, &src, false, false, 0, &mut st);
        assert_eq!((st.done, st.skipped, st.errors), (0, 1, 0));
        assert_eq!(std::fs::read(&dst).unwrap(), b"OLD");
        // 覆盖 → 替换为新内容。
        let mut st2 = CopyStats::default();
        local_copy_item(&dst, &src, false, true, 0, &mut st2);
        assert_eq!((st2.done, st2.skipped, st2.errors), (1, 0, 0));
        assert_eq!(std::fs::read(&dst).unwrap(), b"NEW");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 本机复制_目录递归_子项全到() {
        let base = lc_tmp("dir");
        let src = base.join("srcdir");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("f1.txt"), b"1").unwrap();
        std::fs::write(src.join("sub").join("f2.txt"), b"2").unwrap();
        let dst = base.join("dstdir");
        let mut st = CopyStats::default();
        local_copy_item(&dst, &src, true, true, 0, &mut st);
        assert_eq!((st.done, st.errors), (2, 0), "两个文件都复制、无错");
        assert_eq!(std::fs::read(dst.join("f1.txt")).unwrap(), b"1");
        assert_eq!(std::fs::read(dst.join("sub").join("f2.txt")).unwrap(), b"2");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 本机复制_目标在源子树内_拒绝防无限递归() {
        let base = lc_tmp("recur");
        let src = base.join("a");
        std::fs::create_dir_all(src.join("sub")).unwrap(); // a/sub 已存在 → 父目录可 canonical
        std::fs::write(src.join("f.txt"), b"x").unwrap();
        // 复制 a 到 a/sub/a（目标落在源子树内）→ 顶层即被自递归防御拦下、计 error、零复制。
        let dst = src.join("sub").join("a");
        let mut st = CopyStats::default();
        local_copy_item(&dst, &src, true, true, 0, &mut st);
        assert_eq!((st.done, st.errors), (0, 1), "顶层拒绝，无任何文件被复制");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 本机复制_源即目标_跳过防毁源() {
        let base = lc_tmp("self");
        let f = base.join("a.txt");
        std::fs::write(&f, b"keep").unwrap();
        // dest==src（复制到原地，覆盖模式）→ 必须跳过，否则 fs::copy 先 truncate 毁掉源。
        let mut st = CopyStats::default();
        local_copy_item(&f, &f, false, true, 0, &mut st);
        assert_eq!((st.done, st.skipped), (0, 1));
        assert_eq!(std::fs::read(&f).unwrap(), b"keep", "源文件未被毁");
        let _ = std::fs::remove_dir_all(&base);
    }

    // ── 第二十二轮：运行时图标加载单元测试 ─────────────────────────────

    #[test]
    fn 图标加载_32px_解码成功() {
        let bytes = include_bytes!("../../../icons/lumen-icon-32.png");
        let icon = load_icon(bytes);
        assert!(icon.is_some(), "lumen-icon-32.png 应解码成功");
    }

    #[test]
    fn 图标加载_64px_解码成功() {
        let bytes = include_bytes!("../../../icons/lumen-icon-64.png");
        let icon = load_icon(bytes);
        assert!(icon.is_some(), "lumen-icon-64.png 应解码成功");
    }

    #[test]
    fn 图标加载_损坏字节_返回_none() {
        // 非法字节流：load_icon 应返回 None 而非 panic。
        let icon = load_icon(b"\x00\x01\x02\x03_not_a_png");
        assert!(icon.is_none(), "损坏字节流应返回 None");
    }

    /// 估算测试区域：与 layout.rs 测试同款 304x202（宽对 3 列、高对
    /// 2 排整除：上3下2 时上排格 100x100、下排格 151x100）。
    fn est_area() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(304.0, 202.0))
    }

    #[test]
    fn 恢复估算_五格上3下2_扣标题栏() {
        // B2 修复断言：估算必须与 shell 首帧同源——布局矩形扣
        // 24px 窗格标题栏，再乘 DPI 缩放取整。
        let px = estimate_restored_pane_px(est_area(), &PaneLayout::uniform(5), 5, None, 2.0);
        assert_eq!(px.len(), 5);
        // 上排格 100x100 逻辑 → 内容 100x76 → 物理 ×2。
        assert_eq!(px[0], (200, 152));
        assert_eq!(px[2], (200, 152));
        // 下排格 151x100 逻辑。
        assert_eq!(px[3], (302, 152));
        assert_eq!(px[4], (302, 152));
    }

    #[test]
    fn 恢复估算_最大化格按整区其余按布局() {
        let px = estimate_restored_pane_px(est_area(), &PaneLayout::uniform(2), 2, Some(0), 1.0);
        // 最大化格独占整区（304x202 − 24 标题栏）。
        assert_eq!(px[0], (304, 178));
        // 隐藏格按布局矩形（两格左右分 151x202；还原最大化时回到它，
        // 届时 resize 近似无损）。
        assert_eq!(px[1], (151, 178));
    }

    #[test]
    fn 恢复估算_布局形状不符回退均分() {
        // 布局是 3 格形状、实际 2 格（防御路径）：按 2 格均分计算，
        // 不 panic、数量对位。
        let px = estimate_restored_pane_px(est_area(), &PaneLayout::uniform(3), 2, None, 1.0);
        assert_eq!(px.len(), 2);
        assert_eq!(px[0], (151, 178));
    }

    #[test]
    fn 焦点窗格最先_激活tab次之() {
        // 3 个 tab 窗格数 2/3/1，激活 tab=1、焦点窗格=2：焦点最先，
        // 激活 tab 其余窗格次之（可见），后台 tab 按下标序殿后。
        assert_eq!(
            drain_order(&[2, 3, 1], 1, 2),
            vec![(1, 2), (1, 0), (1, 1), (0, 0), (0, 1), (2, 0)]
        );
        assert_eq!(drain_order(&[3], 0, 0), vec![(0, 0), (0, 1), (0, 2)]);
    }

    #[test]
    fn 单窗格与空列表() {
        assert_eq!(drain_order(&[1], 0, 0), vec![(0, 0)]);
        assert!(drain_order(&[], 0, 0).is_empty());
    }

    #[test]
    fn 下标越界时退化为顺序遍历() {
        // 激活 tab 越界：全部按下标序。
        assert_eq!(drain_order(&[2], 7, 0), vec![(0, 0), (0, 1)]);
        // 焦点窗格越界：激活 tab 仍领先，但无焦点优先项。
        assert_eq!(drain_order(&[1, 2], 1, 9), vec![(1, 0), (1, 1), (0, 0)]);
    }

    #[test]
    fn 宽度写盘判定_正常变化才写() {
        // 范围内且差 ≥1px：写。
        assert!(width_worth_persisting(240.0, 180.0, 140.0, 320.0));
        assert!(width_worth_persisting(180.0, 240.0, 140.0, 320.0));
        // 差 <1px（亚像素抖动/无变化）：不写。
        assert!(!width_worth_persisting(180.5, 180.0, 140.0, 320.0));
        assert!(!width_worth_persisting(180.0, 180.0, 140.0, 320.0));
        // 端点 ±1 容差内：写（面板钳到 min/max 是用户主动拖到头）。
        assert!(width_worth_persisting(139.5, 180.0, 140.0, 320.0));
        assert!(width_worth_persisting(320.8, 180.0, 140.0, 320.0));
    }

    #[test]
    fn 宽度写盘判定_瞬态与非法不写() {
        // 窗口过窄被临时压缩到范围之外：不写（重启还原用户值）。
        assert!(!width_worth_persisting(80.0, 180.0, 140.0, 320.0));
        assert!(!width_worth_persisting(500.0, 180.0, 140.0, 320.0));
        // NaN/Inf 防御：不写。
        assert!(!width_worth_persisting(f32::NAN, 180.0, 140.0, 320.0));
        assert!(!width_worth_persisting(f32::INFINITY, 180.0, 140.0, 320.0));
    }

    // ── 第十轮问题1：最大化越界纯函数测试 ──────────────────────────────

    #[test]
    fn 最大化越界_标准8px() {
        // 2560×1440 屏幕（工作区），窗口 rect (-8,-8)~(2568,1400)
        // 实测典型值：四边各 8px 越界
        let win = (-8, -8, 2568, 1400);
        let work = (0, 0, 2560, 1440);
        let (l, t, r, b) = maximized_overflow(win, work);
        assert_eq!(l, 8, "左越界应为 8px");
        assert_eq!(t, 8, "顶越界应为 8px");
        assert_eq!(r, 8, "右越界应为 8px");
        assert_eq!(b, 0, "底未越界应为 0（工作区底在任务栏上方）");
    }

    #[test]
    fn 最大化越界_非最大化时全零() {
        // 正常非最大化窗口在工作区内：所有越界量为 0
        let win = (100, 100, 1100, 740);
        let work = (0, 0, 2560, 1440);
        let (l, t, r, b) = maximized_overflow(win, work);
        assert_eq!((l, t, r, b), (0, 0, 0, 0), "非最大化时无越界");
    }

    #[test]
    fn 最大化越界_跨显示器负坐标() {
        // 副显示器在主屏左侧（工作区 x=-1920..0，y=0..1080）
        // 最大化时窗口 rect (-1928,-8)~(8,1072)
        let win = (-1928, -8, 8, 1072);
        let work = (-1920, 0, 0, 1080);
        let (l, t, r, b) = maximized_overflow(win, work);
        assert_eq!(l, 8, "副显示器左越界应为 8px");
        assert_eq!(t, 8, "副显示器顶越界应为 8px");
        assert_eq!(r, 8, "副显示器右越界应为 8px");
        assert_eq!(b, 0, "副显示器底未越界");
    }

    #[test]
    fn 最大化越界_底部也有越界() {
        // 部分配置下底部也越界（任务栏很高时）
        let win = (-8, -8, 2568, 1448);
        let work = (0, 0, 2560, 1400);
        let (l, t, r, b) = maximized_overflow(win, work);
        assert_eq!(l, 8);
        assert_eq!(t, 8);
        assert_eq!(r, 8);
        assert_eq!(b, 48, "底部越界量应正确计算");
    }

    // ── M4.1 批D1：提交编码纯函数测试（设计稿 §3.2 步骤 2）─────────

    #[cfg(feature = "input-editor")]
    mod submit_encoding {
        use super::super::{encode_llm_submit, encode_submit};
        use crate::llm_cli::LlmCliKind;

        #[test]
        fn 单行文本_末尾加_cr() {
            let payload = encode_submit("echo hello");
            assert_eq!(payload, b"echo hello\r", "单行提交应为 text + CR");
        }

        #[test]
        fn 空文本_仍加_cr() {
            let payload = encode_submit("");
            assert_eq!(payload, b"\r", "空文本提交应为单个 CR");
        }

        #[test]
        fn 多行文本_括号粘贴协议包裹() {
            let text = "line1\nline2";
            let payload = encode_submit(text);
            assert!(
                payload.starts_with(b"\x1b[200~"),
                "多行提交应以 ESC[200~ 开头"
            );
            assert!(
                payload.ends_with(b"\x1b[201~\r"),
                "多行提交应以 ESC[201~CR 结尾"
            );
            let inner = &payload[6..payload.len() - 7];
            assert_eq!(inner, text.as_bytes(), "多行提交括号内容应为原始文本");
        }

        #[test]
        fn 两行判定阈值_仅两行走括号粘贴() {
            // 恰好两行（含一个 \n）→ 括号粘贴
            let payload = encode_submit("a\nb");
            assert!(
                payload.starts_with(b"\x1b[200~"),
                "两行文本应走括号粘贴协议"
            );
        }

        #[test]
        fn kimi_单行斜杠命令使用括号粘贴避免二次补全() {
            let payload = encode_llm_submit("/yolo", Some(LlmCliKind::Kimi), false);
            assert_eq!(
                payload, b"\x1b[200~/yolo\x1b[201~\r",
                "Kimi 单行命令必须原子粘贴后再提交"
            );
        }

        #[test]
        fn 其他_cli_单行仍按普通输入提交() {
            for kind in [
                None,
                Some(LlmCliKind::Claude),
                Some(LlmCliKind::Codex),
                Some(LlmCliKind::Gemini),
            ] {
                assert_eq!(encode_llm_submit("/help", kind, false), b"/help\r");
            }
        }

        #[test]
        fn codex_win32输入模式一次enter直接提交() {
            assert_eq!(
                encode_llm_submit("hello", Some(LlmCliKind::Codex), true),
                b"hello\x1b[13;0;13;1;0;1_\x1b[13;0;13;0;0;1_"
            );
        }
    }
}
