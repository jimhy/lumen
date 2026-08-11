//! 应用日志落盘：stderr + `<数据目录>/lumen.log` 双写。
//!
//! # 为什么必须落盘
//! release 构建带 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`，**没有
//! 控制台窗口**——`env_logger` 默认写 stderr 的每一行都直接丢进虚空。于是只在真实网络现场
//! 才复现的问题（P2P 打洞卡在哪个候选、WS 因何重连、STUN 是否超时）在安装版上**完全不可
//! 定位**，只能靠猜。M6 P2P「远程一直走中继」的排查就卡死在这一点上：代码逻辑通读没问题，
//! 但两端到底发生了什么一行证据都没有。
//!
//! # 轮转策略（刻意极简）
//! 启动时若现存日志超过 [`MAX_LOG_BYTES`]，整个重命名为 `lumen.log.old`（覆盖上一份），新日志
//! 从空文件写起。**不做**按大小切片 / 多份归档：诊断真正需要的只有「本次运行」和「上一次运行」
//! 两份，归档多了反而找不到该看哪个。轮转失败（文件被占用等）不阻断启动，继续追加即可。
//!
//! # 降级
//! 数据目录不可用或文件打不开时退化为**纯 stderr**，日志功能本身绝不导致启动失败。

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// 单份日志大小上限（启动时超过即轮转）。8 MiB ≈ 数十万行，够覆盖一次长会话的全部诊断。
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// 当前运行日志文件名。
const LOG_NAME: &str = "lumen.log";
/// 上一次运行的日志文件名（轮转目标，每次轮转覆盖）。
const LOG_OLD_NAME: &str = "lumen.log.old";

/// stderr + 文件双写的 `env_logger` sink。
///
/// 两路各自忽略自己的写入错误：GUI 构建下 stderr 本就无处可去（写失败是常态），
/// 不能让它连累文件落盘；反之文件被删/磁盘满也不该让终端里看不到日志。
struct Tee {
    /// 日志文件句柄；`None` = 落盘不可用，退化为纯 stderr。
    file: Option<File>,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        if let Some(f) = &mut self.file {
            let _ = f.write_all(buf);
        }
        // 恒报「全部写入」：任一路失败都不该让 log 宏层面观察到 io 错误而重试/丢行。
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(f) = &mut self.file {
            let _ = f.flush();
        }
        Ok(())
    }
}

/// 按需轮转后以追加模式打开日志文件。目录缺失自动创建；任一步失败返回 `None`（降级纯 stderr）。
fn open_rotating(path: &Path) -> Option<File> {
    if let Some(dir) = path.parent() {
        // 目录已存在时 create_dir_all 返回 Ok，无需先判存在。
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        // 轮转失败（上一份被其它进程占用等）不是错误：继续往原文件追加即可。
        let _ = std::fs::rename(path, path.with_file_name(LOG_OLD_NAME));
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// 初始化全局 logger（stderr + 文件双写）。返回实际落盘路径；`None` = 本次运行仅 stderr。
///
/// 过滤级别沿用 `RUST_LOG` 环境变量，缺省 `info`。**只能调用一次**（`env_logger` 全局
/// 初始化），重复调用会 panic —— 故仅在 `main` 开头调用。
pub fn init() -> Option<PathBuf> {
    let path = crate::paths::data_file(LOG_NAME);
    let file = path.as_deref().and_then(open_rotating);
    let landed = file.is_some().then(|| path.clone()).flatten();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(Tee { file })))
        .init();
    landed
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
        let f = open_rotating(&path).expect("打开日志");
        drop(f);
        assert_eq!(
            std::fs::read(&path).expect("读日志"),
            b"existing",
            "未超阈值不应轮转"
        );
        assert!(!dir.join(LOG_OLD_NAME).exists(), "未超阈值不应产生 .old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 超阈值的文件被整个移到 `.old`，新日志从空开始。
    #[test]
    fn rotates_oversized_log() {
        let dir = std::env::temp_dir().join("lumen_applog_rotate");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(LOG_NAME);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        std::fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).expect("写超限日志");
        let f = open_rotating(&path).expect("打开日志");
        drop(f);
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

    /// 目录不存在时自动创建（首次启动路径）。
    #[test]
    fn creates_missing_dir() {
        let dir = std::env::temp_dir().join("lumen_applog_mkdir");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(LOG_NAME);
        let f = open_rotating(&path).expect("打开日志（应自动建目录）");
        drop(f);
        assert!(path.exists(), "日志文件应已创建");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
