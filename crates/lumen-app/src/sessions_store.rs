//! 会话列表持久化（F4）：`%LOCALAPPDATA%/Lumen/sessions.json`。
//!
//! 保存各会话的自定义名与最后上报的 cwd（OSC 9;9）以及激活下标；
//! 启动时按条目逐个重开 shell（初始工作目录用保存的 cwd，已失效则
//! 回退默认目录并提示）。屏幕内容/滚动历史不持久化——重启是新 shell，
//! 这是预期行为。
//!
//! 写盘时机（main.rs）：结构性变更（新建/关闭/重命名/切换激活）即写；
//! cwd 随提示符上报变化时与上次快照比对后按需写（写频≈用户 cd 频率）。
//! 原子写盘模式与 settings.rs 一致（同目录临时文件 + rename 覆盖）。
//! 缺失/损坏 → 启动回退单默认会话，损坏记日志警告、不 panic。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 单个会话的持久化条目。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionEntry {
    /// 用户重命名的标题（None = 跟随默认标题规则：cwd > OSC 标题）。
    pub custom_title: Option<String>,
    /// 最后上报的 cwd（OSC 9;9）；恢复时作为 shell 初始工作目录。
    pub cwd: Option<PathBuf>,
}

impl SessionEntry {
    /// 恢复时可用的初始 cwd：仅当保存的路径仍是存在的目录。失效
    /// （目录被删/重命名/网络盘离线）返回 None，调用方回退默认
    /// 目录并 toast 提示。
    pub fn usable_cwd(&self) -> Option<&Path> {
        self.cwd.as_deref().filter(|p| p.is_dir())
    }
}

/// sessions.json 根结构。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionsFile {
    /// 会话条目（侧栏自上而下的顺序）。
    pub entries: Vec<SessionEntry>,
    /// 激活会话下标（加载时已夹紧到合法范围）。
    pub active: usize,
}

impl SessionsFile {
    /// 持久化路径：`%LOCALAPPDATA%/Lumen/sessions.json`。
    /// 环境变量缺失（极端定制环境）返回 None，本次运行不持久化。
    pub fn path() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA").map(|d| Path::new(&d).join("Lumen").join("sessions.json"))
    }

    /// 启动加载：缺失/损坏/空条目返回 None（回退单默认会话），损坏
    /// 记警告日志，绝不 panic。
    pub fn load() -> Option<Self> {
        match Self::path() {
            Some(p) => Self::load_from(&p),
            None => {
                log::warn!("LOCALAPPDATA 未设置，会话列表不持久化");
                None
            }
        }
    }

    /// 从指定路径加载（拆出来供单测注入临时路径）。
    pub fn load_from(path: &Path) -> Option<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                log::info!("会话列表文件不存在，按单默认会话启动: {}", path.display());
                return None;
            }
            Err(e) => {
                log::warn!("读会话列表失败，按单默认会话启动 {}: {e}", path.display());
                return None;
            }
        };
        // 与 settings.rs 同款 BOM 防御（用户用 PowerShell 重定向手改
        // 文件时的常见产物）。
        let text = text.trim_start_matches('\u{feff}');
        let mut file = match serde_json::from_str::<Self>(text) {
            Ok(f) => f,
            Err(e) => {
                log::warn!(
                    "会话列表解析失败，按单默认会话启动（原文件保留，下次写盘才覆盖）{}: {e}",
                    path.display()
                );
                return None;
            }
        };
        if file.entries.is_empty() {
            // 空列表 = 上次退出前关掉了全部 tab：与缺失同义。
            return None;
        }
        // 激活下标夹紧（手改文件/旧版本残留的越界值）。
        file.active = file.active.min(file.entries.len() - 1);
        // 空白自定义名视同未命名（重命名路径不会写空名，防手改）。
        for entry in &mut file.entries {
            if entry
                .custom_title
                .as_ref()
                .is_some_and(|t| t.trim().is_empty())
            {
                entry.custom_title = None;
            }
        }
        Some(file)
    }

    /// 写盘（结构性变更/cwd 上报变化时由 main 调用）。失败只记日志
    /// ——会话簿记不应打扰终端使用；无持久化路径时静默跳过。
    pub fn save(&self) {
        let Some(p) = Self::path() else {
            return;
        };
        if let Err(e) = self.save_to(&p) {
            log::error!("写会话列表失败 {}: {e:#}", p.display());
        }
    }

    /// 原子写盘：先写同目录临时文件再改名覆盖，防半写损坏
    /// （settings.rs 同款模式）。
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("会话列表路径无父目录")?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建持久化目录失败: {}", dir.display()))?;
        let json = serde_json::to_string_pretty(self).context("序列化会话列表失败")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)
            .with_context(|| format!("写会话列表临时文件失败: {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("替换会话列表文件失败: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用独立文件名，避免并行测试互踩。
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lumen_sessions_test_{}_{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn 序列化往返() {
        let f = SessionsFile {
            entries: vec![
                SessionEntry {
                    custom_title: Some("构建机".to_owned()),
                    cwd: Some(PathBuf::from(r"C:\proj\lumen")),
                },
                SessionEntry {
                    custom_title: None,
                    cwd: Some(PathBuf::from(r"D:\work 空格\中文目录")),
                },
                SessionEntry {
                    custom_title: None,
                    cwd: None,
                },
            ],
            active: 1,
        };
        let p = temp_path("roundtrip");
        f.save_to(&p).expect("写盘失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded, f);
    }

    #[test]
    fn 损坏文件降级() {
        let p = temp_path("corrupt");
        std::fs::write(&p, "{ 这不是 json !!!").expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert!(loaded.is_none(), "损坏文件应降级 None（单默认会话）");
    }

    #[test]
    fn 缺失文件降级() {
        let p = temp_path("missing");
        let _ = std::fs::remove_file(&p);
        assert!(SessionsFile::load_from(&p).is_none());
    }

    #[test]
    fn 空条目视同缺失() {
        let p = temp_path("empty");
        std::fs::write(&p, r#"{ "entries": [], "active": 0 }"#).expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert!(loaded.is_none(), "空列表应回退单默认会话");
    }

    #[test]
    fn 激活下标越界夹紧() {
        let p = temp_path("clamp");
        std::fs::write(
            &p,
            r#"{ "entries": [ { "cwd": "C:\\a" }, { "cwd": "C:\\b" } ], "active": 9 }"#,
        )
        .expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.active, 1);
    }

    #[test]
    fn 空白自定义名视同未命名() {
        let p = temp_path("blank_title");
        std::fs::write(
            &p,
            r#"{ "entries": [ { "custom_title": "   ", "cwd": "C:\\a" } ], "active": 0 }"#,
        )
        .expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert!(loaded.entries[0].custom_title.is_none());
    }

    #[test]
    fn 缺字段平滑加载() {
        // 旧版本/手改文件缺字段：serde(default) 补默认值。
        let p = temp_path("partial");
        std::fs::write(&p, r#"{ "entries": [ {} ] }"#).expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.entries.len(), 1);
        assert!(loaded.entries[0].custom_title.is_none());
        assert!(loaded.entries[0].cwd.is_none());
        assert_eq!(loaded.active, 0);
    }

    #[test]
    fn cwd失效回退() {
        // 存在的目录 → 可用；不存在的目录/指向文件的路径 → None。
        let dir = std::env::temp_dir();
        let ok = SessionEntry {
            custom_title: None,
            cwd: Some(dir.clone()),
        };
        assert_eq!(ok.usable_cwd(), Some(dir.as_path()));

        let gone = SessionEntry {
            custom_title: None,
            cwd: Some(PathBuf::from(r"C:\lumen_不存在的目录_单测专用")),
        };
        assert!(gone.usable_cwd().is_none(), "失效目录应回退 None");

        let file_path = dir.join(format!("lumen_sessions_cwd_{}.txt", std::process::id()));
        std::fs::write(&file_path, b"x").expect("写测试文件失败");
        let not_dir = SessionEntry {
            custom_title: None,
            cwd: Some(file_path.clone()),
        };
        let usable = not_dir.usable_cwd().is_none();
        let _ = std::fs::remove_file(&file_path);
        assert!(usable, "指向文件的 cwd 不可用");

        let none = SessionEntry {
            custom_title: None,
            cwd: None,
        };
        assert!(none.usable_cwd().is_none());
    }
}
