//! 应用日志落盘：stderr + `<数据目录>/lumen.log` 双写。
//!
//! # 为什么必须落盘
//! release 构建带 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`，**没有
//! 控制台窗口**——`env_logger` 默认写 stderr 的每一行都直接丢进虚空。于是只在真实网络现场
//! 才复现的问题（P2P 打洞卡在哪个候选、WS 因何重连、STUN 是否超时）在安装版上**完全不可
//! 定位**，只能靠猜。M6 P2P「远程一直走中继」的排查就卡死在这一点上：代码逻辑通读没问题，
//! 但两端到底发生了什么一行证据都没有。
//!
//! # 两阶段初始化（[`init`] → [`attach_file`]）——不是绕弯路，是必须的
//! [`init`] 只装 logger（纯 stderr），**不碰任何文件**；确认本进程是要继续运行的实例之后，
//! 才由 [`attach_file`] 打开日志文件并开始双写。
//!
//! 一步到位地在 `main` 开头就打开文件会毁数据：release 的单实例语义是「第二个实例通知主实例
//! 前台化后自己静默退出」，而这正是**用户双击图标的正常路径**。那个短命进程若也执行轮转，
//! 会把主实例正在写的 `lumen.log` 改名成 `.old`——`rename` 对已打开的句柄照样成功（Windows 上
//! std 的 `OpenOptions` 默认 share mode 含 `FILE_SHARE_DELETE`，走 `MoveFileEx`；Linux 上无条件
//! 成功），句柄跟着文件对象走，**主实例毫不知情地继续往 `.old` 里追加**，下次轮转再把它覆盖。
//! 攒了几天的现场就这么没了，而磁盘上那个 `lumen.log` 只剩短命进程写的三行。
//!
//! # 轮转策略
//! 超过 [`MAX_LOG_BYTES`] 即把当前日志整个重命名为 `lumen.log.old`（覆盖上一份），新日志从空
//! 文件写起。**启动时查一次，运行期也持续计数**——只在启动时判定的话，`MAX_LOG_BYTES` 名为
//! 「大小上限」实为「启动时继承到的阈值」，一个长跑进程可以把磁盘写满都不轮转（P2P 的
//! accept 失败日志由未鉴权的远端驱动，正是这种能被外部灌爆的量）。
//! 只留「本次运行」和「上一次运行」两份：归档多了反而找不到该看哪个。
//!
//! # 这份文件里有什么个人信息
//! 落盘把原先「写进虚空的 stderr」变成了**用户会直接外发的文件**（本功能的预期用法就是
//! 「把 lumen.log 发我看看」），所以有必要把它包含的个人信息列清楚：
//! - **网络身份**：本端与对端的公网 IP:端口（STUN 映射、P2P 候选表、握手对端地址）、本端 LAN IP。
//!   跨地域连接时这等于暴露两处物理位置的大致归属。
//! - **账号**：登录态加载 / 登录成功那两行带显示名与邮箱。
//! - **路径**：下载目录、上传目录等本机路径（Windows 下常含真实姓名的用户名）。
//!
//! 不包含：证书 DER（只打字节数）、鉴权 token（URL 从不入日志）、设备 id、用户输入或 PTY 字节。
//!
//! # 降级
//! 数据目录不可用或文件打不开时退化为**纯 stderr**，日志功能本身绝不导致启动失败。

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 单份日志大小上限（启动时查一次，运行期持续计数，超过即轮转）。
/// 8 MiB ≈ 数十万行，够覆盖一次长会话的全部诊断。
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// 当前运行日志文件名。
const LOG_NAME: &str = "lumen.log";
/// 上一次运行的日志文件名（轮转目标，每次轮转覆盖）。
const LOG_OLD_NAME: &str = "lumen.log.old";

/// 日志文件落地端：句柄 + 路径 + 本次运行已写字节数（运行期轮转判据）。
struct LogSink {
    /// 日志文件路径（轮转时据此推 `.old` 路径并重开）。
    path: PathBuf,
    /// 当前句柄；轮转期间短暂为 `None`（先关句柄再改名，不依赖平台的
    /// 「已打开文件可否重命名」语义）。打不开则保持 `None`，退化为纯 stderr。
    file: Option<File>,
    /// 当前这份日志已写入的字节数（**含**打开时已有的大小）。
    written: u64,
}

impl LogSink {
    /// 写一段日志并按需轮转。所有 io 错误都吞掉——日志写不进去绝不能影响主流程。
    fn write(&mut self, buf: &[u8]) {
        let Some(f) = self.file.as_mut() else {
            return;
        };
        if f.write_all(buf).is_err() {
            return;
        }
        self.written = self.written.saturating_add(buf.len() as u64);
        if self.written > MAX_LOG_BYTES {
            self.rotate();
        }
    }

    /// 就地轮转：关句柄 → 改名为 `.old`（覆盖上一份）→ 重开空文件。
    /// 任一步失败就保持现状继续写（轮转失败不是错误，日志连续性优先）。
    fn rotate(&mut self) {
        self.file = None; // drop 先关句柄，再改名
        let old = self.path.with_file_name(LOG_OLD_NAME);
        if std::fs::rename(&self.path, &old).is_err() {
            // 改名失败（被占用等）：重开原文件继续追加，下次写满再试。
            self.file = open_append(&self.path);
            return;
        }
        self.file = open_append(&self.path);
        self.written = 0;
    }
}

/// 全局日志落地端。[`init`] 之后仍为 `None`（纯 stderr），由 [`attach_file`] 填入。
///
/// `Mutex` 而非无锁：`log` 宏可能被任意线程调用（WS 线程、P2P 线程、PTY 线程都在打日志），
/// 无锁并发 `write_all` 在 Windows 上会让两条日志**在同一行里交错撕裂**，而撕裂的日志和
/// 没有日志一样没用。锁只在真正写文件时短暂持有，日志本就不在热路径上。
static LOG_SINK: Mutex<Option<LogSink>> = Mutex::new(None);

/// stderr + 文件双写的 `env_logger` sink。
///
/// 两路各自忽略自己的写入错误：GUI 构建下 stderr 本就无处可去（写失败是常态），
/// 不能让它连累文件落盘；反之文件被删/磁盘满也不该让终端里看不到日志。
struct Tee;

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        // 锁中毒（某个线程写日志时 panic）不该让日志系统整个哑掉，故忽略 poison。
        if let Ok(mut guard) = LOG_SINK.lock() {
            if let Some(sink) = guard.as_mut() {
                sink.write(buf);
            }
        }
        // 恒报「全部写入」：任一路失败都不该让 log 宏层面观察到 io 错误而重试/丢行。
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Ok(mut guard) = LOG_SINK.lock() {
            if let Some(sink) = guard.as_mut() {
                if let Some(f) = sink.file.as_mut() {
                    let _ = f.flush();
                }
            }
        }
        Ok(())
    }
}

/// 以追加模式打开文件（失败返回 `None`）。
fn open_append(path: &Path) -> Option<File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// 按需轮转后打开日志文件，返回句柄与当前已有字节数。目录缺失自动创建。
fn open_rotating(path: &Path) -> Option<(File, u64)> {
    if let Some(dir) = path.parent() {
        // 目录已存在时 create_dir_all 返回 Ok，无需先判存在。
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        // 轮转失败（上一份被其它进程占用等）不是错误：继续往原文件追加即可。
        let _ = std::fs::rename(path, path.with_file_name(LOG_OLD_NAME));
    }
    let file = open_append(path)?;
    let existing = file.metadata().map(|m| m.len()).unwrap_or(0);
    Some((file, existing))
}

/// 装上全局 logger（**仅 stderr**，不碰任何文件）。必须在任何 `log` 宏之前调用，且只能调一次
/// （`env_logger` 全局初始化，重复调用会 panic）。文件落盘由 [`attach_file`] 在确认实例身份后接上。
///
/// 过滤级别沿用 `RUST_LOG` 环境变量，缺省 `info`。
///
/// **强制关闭颜色**：sink 是 `Target::Pipe`，而 env_logger 的 auto-color 探测只对 Stdout/Stderr
/// 生效，Pipe 下 `Auto` 恒降级为不上色——终端那份颜色本来就拿不回来；反过来若环境里存在
/// `RUST_LOG_STYLE=always`，带 ANSI 转义的字节会被 Tee 原样写进 `lumen.log`，用记事本打开满屏
/// `←[31m`，恰好是最需要把日志发给别人看的时候最难读。显式 `Never` 至少保证文件干净。
pub fn init() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .write_style(env_logger::WriteStyle::Never)
        .target(env_logger::Target::Pipe(Box::new(Tee)))
        .init();
}

/// 接上文件落盘，返回实际路径；`None` = 数据目录不可用或文件打不开（保持纯 stderr）。
///
/// **只应由「会继续运行下去」的实例调用**——第二实例（通知主实例前台化后即退出）绝不能调，
/// 否则会把主实例正在写的日志轮转走，详见模块文档。
pub fn attach_file() -> Option<PathBuf> {
    let path = crate::paths::data_file(LOG_NAME)?;
    let (file, written) = open_rotating(&path)?;
    let sink = LogSink {
        path: path.clone(),
        file: Some(file),
        written,
    };
    // 锁中毒时放弃落盘（保持纯 stderr），不 panic。
    LOG_SINK.lock().ok()?.replace(sink);
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 轮转阈值内的文件保持原样（不误轮转、不清空既有诊断）。
    #[test]
    fn keeps_small_log() {
        let dir = std::env::temp_dir().join("lumen_applog_small");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(LOG_NAME);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        std::fs::write(&path, b"existing").expect("写初始日志");
        let (f, written) = open_rotating(&path).expect("打开日志");
        drop(f);
        assert_eq!(written, 8, "已有字节数须计入运行期轮转判据");
        assert_eq!(
            std::fs::read(&path).expect("读日志"),
            b"existing",
            "未超阈值不应轮转"
        );
        assert!(!dir.join(LOG_OLD_NAME).exists(), "未超阈值不应产生 .old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 超阈值的文件在**打开时**被整个移到 `.old`，新日志从空开始。
    #[test]
    fn rotates_oversized_log_on_open() {
        let dir = std::env::temp_dir().join("lumen_applog_rotate");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(LOG_NAME);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        std::fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).expect("写超限日志");
        let (f, written) = open_rotating(&path).expect("打开日志");
        drop(f);
        assert_eq!(written, 0, "轮转后计数须归零");
        assert_eq!(
            std::fs::metadata(&path).expect("读新日志元信息").len(),
            0,
            "轮转后新日志应为空"
        );
        assert_eq!(
            std::fs::metadata(dir.join(LOG_OLD_NAME))
                .expect("读 .old 元信息")
                .len(),
            MAX_LOG_BYTES + 1,
            ".old 应完整保留上一份"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **运行期**写满也要轮转——只在启动时判一次的话，长跑进程能把磁盘写满都不轮转
    /// （P2P accept 失败日志由未鉴权远端驱动，正是这种能被外部灌爆的量）。
    #[test]
    fn rotates_during_run() {
        let dir = std::env::temp_dir().join("lumen_applog_runtime");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(LOG_NAME);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let (file, written) = open_rotating(&path).expect("打开日志");
        let mut sink = LogSink {
            path: path.clone(),
            file: Some(file),
            written,
        };
        // 分块写到刚好越过上限。
        let chunk = vec![b'y'; 1024 * 1024];
        let rounds = (MAX_LOG_BYTES / chunk.len() as u64) + 1;
        for _ in 0..rounds {
            sink.write(&chunk);
        }
        sink.write(b"after-rotate");
        drop(sink);
        assert!(dir.join(LOG_OLD_NAME).exists(), "运行期超限须产生 .old");
        let now = std::fs::read(&path).expect("读新日志");
        assert_eq!(now, b"after-rotate", "轮转后的新日志只含轮转之后写入的内容");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录不存在时自动创建（首次启动路径）。
    #[test]
    fn creates_missing_dir() {
        let dir = std::env::temp_dir().join("lumen_applog_mkdir");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(LOG_NAME);
        let (f, _) = open_rotating(&path).expect("打开日志（应自动建目录）");
        drop(f);
        assert!(path.exists(), "日志文件应已创建");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
