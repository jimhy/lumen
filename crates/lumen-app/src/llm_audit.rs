//! M7 片 8：远程 LLM 会话的**结构化审计日志**（蓝图 §6.8.4）。
//!
//! # 为什么这个文件不可省
//!
//! ⑫ 拍板「桌面端不做会话 UI」之后，手机远程让 PC 跑 Bash 这件事在桌面上**没有任何
//! 可见记录**——图标是瞬时的，关掉 Lumen 就没了。这个 jsonl 是唯一能回答
//! 「昨天下午它到底跑了什么」的东西。
//!
//! 落点 `<数据目录>/audit/llm-YYYY-MM-DD.jsonl`，一行一事件，保留
//! [`AUDIT_RETENTION_DAYS`] 天。
//!
//! # ★ 脱敏取舍：**参数首 N 字符明文 + 全量 sha256 + 字节数**
//!
//! 这是本模块最容易被做错的一处，三种做法的账（蓝图 §6.8.4）：
//!
//! | 做法 | 问题 |
//! |---|---|
//! | 只记 sha256 | 事后**无法回答「它跑了什么」**，而那正是审计的全部意义 |
//! | 全量明文 | 违反 §5.10——对话正文是密码 / 密钥的重灾区 |
//! | **首 N 字符 + 摘要** | ✅ `rm -rf /` 这类看得见；长 heredoc 里的密钥看不见 |
//!
//! N 由设置项 `audit_arg_head_len` 控制，默认 [`DEFAULT_ARG_HEAD_LEN`]；
//! **设 0 即完全关闭明文**（只剩哈希）。那是用户自己的取舍，设置项旁边写清了后果。
//!
//! # ★ 与 `applog` 的分工：这里**只记谁在我的机器上做了什么**
//!
//! `applog`（`lumen.log`）是排障日志，什么都记、用户会整份发出去。审计是**安全记录**，
//! 只记远程会话的动作，且**刻意不记对话正文**（哪怕一行摘要）——弹层不显示正文，
//! 这里也不写。工具名、cwd、参数首段、对端设备名，到此为止。
//!
//! # 失败一律降级为「不记这一条」
//!
//! 磁盘满、目录被占、权限不足都不该影响远程会话本身。**但每一次失败都会
//! `log::warn!`**：一个静默失效的审计日志比没有审计更糟——它会让人以为有据可查。

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 审计文件保留天数（蓝图 §6.8.4）。
///
/// 14 天覆盖「上周那次是怎么回事」，又不至于让一个长期开着的工作站攒下无界的历史。
pub const AUDIT_RETENTION_DAYS: i64 = 14;

/// 参数明文首段的默认长度（字符，不是字节）。
///
/// 200 字符能完整看见绝大多数命令行（`cargo build -p lumen-app`、`rm -rf /tmp/x`），
/// 而长 heredoc / base64 blob 里的密钥落在这个窗口之外。
pub const DEFAULT_ARG_HEAD_LEN: usize = 200;

/// 一条审计事件。
///
/// **刻意是一个扁平结构而不是 enum**：审计行要能被 `jq` 之类的工具按固定字段筛，
/// 而 enum 会让不同事件长出不同形状的 JSON，事后查起来每种都要单独写一遍表达式。
#[derive(Debug, Clone, Default)]
pub struct AuditEvent {
    /// 事件种类（`ToolUse` / `RunnerStarted` / `SessionOpened` …）。
    pub ev: &'static str,
    /// 隐藏会话 id。0 = 与具体会话无关（如 runner 启停）。
    pub sid: u64,
    /// 对端设备显示名。
    pub peer: String,
    /// 对端 device_id。
    pub peer_dev: String,
    /// 对话 id。0 = 无关。
    pub conv: u64,
    /// 工具名（仅 `ToolUse`）。
    pub tool: String,
    /// 工作目录。
    pub cwd: String,
    /// 附加说明（退出原因、协议错误类型、未识别事件计数…）。
    pub note: String,
}

impl AuditEvent {
    /// 造一条只带种类的事件。
    pub fn new(ev: &'static str) -> Self {
        Self {
            ev,
            ..Self::default()
        }
    }
}

/// 把工具参数按脱敏规则拆成三件套：首段明文、全量 sha256、字节数。
///
/// `head_len == 0` ⇒ 明文段为空串（只留哈希与长度）。
///
/// # 为什么按**字符**截而不是字节
/// 按字节截会切出半个多字节字符，写进 jsonl 就是一个坏 JSON 字符串（或替换符），
/// 而这个文件的读者是 `jq` 与人眼，两者都会被那半个字符绊住。
#[must_use]
pub fn redact_arg(arg: &str, head_len: usize) -> (String, String, usize) {
    let head: String = arg.chars().take(head_len).collect();
    let mut hasher = Sha256::new();
    hasher.update(arg.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(hex, "{b:02x}");
    }
    (head, hex, arg.len())
}

/// 把一条事件与已脱敏的参数渲染成一行 JSON。
///
/// 手写而不是 `serde_json::to_string`：这一行的**字段顺序是给人读的**（时间、会话、
/// 事件、工具、参数依次排开），而 serde 的输出顺序取决于结构体定义，日后加字段会
/// 让老行与新行的列序对不上，`jq` 之外的肉眼比对就废了。
#[must_use]
pub fn render_line(now_secs: i64, ev: &AuditEvent, arg: Option<(&str, &str, usize)>) -> String {
    let mut out = String::with_capacity(256);
    out.push('{');
    let _ = write!(out, "\"ts\":{now_secs}");
    let _ = write!(out, ",\"ev\":{}", json_str(ev.ev));
    if ev.sid != 0 {
        let _ = write!(out, ",\"sid\":{}", ev.sid);
    }
    if ev.conv != 0 {
        let _ = write!(out, ",\"conv\":{}", ev.conv);
    }
    if !ev.peer.is_empty() {
        let _ = write!(out, ",\"peer\":{}", json_str(&ev.peer));
    }
    if !ev.peer_dev.is_empty() {
        let _ = write!(out, ",\"peer_dev\":{}", json_str(&ev.peer_dev));
    }
    if !ev.tool.is_empty() {
        let _ = write!(out, ",\"tool\":{}", json_str(&ev.tool));
    }
    if let Some((head, sha, bytes)) = arg {
        // 明文段为空（head_len = 0）时**仍然写这个键**：让「关掉了明文」与
        // 「参数本来就是空的」在事后可区分——前者 arg_bytes > 0，后者 = 0。
        let _ = write!(out, ",\"arg_head\":{}", json_str(head));
        let _ = write!(out, ",\"arg_sha256\":{}", json_str(sha));
        let _ = write!(out, ",\"arg_bytes\":{bytes}");
    }
    if !ev.cwd.is_empty() {
        let _ = write!(out, ",\"cwd\":{}", json_str(&ev.cwd));
    }
    if !ev.note.is_empty() {
        let _ = write!(out, ",\"note\":{}", json_str(&ev.note));
    }
    out.push('}');
    out
}

/// 最小 JSON 字符串转义。
///
/// 不引 serde 是因为这里只需要一个字符串字面量的转义，而**控制字符必须转成 `\uXXXX`**
/// ——工具参数里出现裸 `\n` / `\t` / 终端转义序列是常态，直接写进去会把一行 jsonl
/// 撕成好几行，整个文件从此没法按行解析。
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 审计日志写入器。
///
/// 按**日期**切文件（不是按大小）：审计的检索方式是「昨天下午」，按日期分文件之后
/// 那就是「打开昨天那个文件」，不需要任何索引。
#[derive(Debug, Default)]
pub struct AuditLog {
    /// 当前打开的文件与它对应的日期串。
    open: Option<(String, File)>,
    /// 目录不可用时置位，之后不再反复尝试（也不再反复刷 warn）。
    disabled: bool,
}

impl AuditLog {
    /// 新建（**不立刻建目录**，第一次真的要写时才建）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 写一条。`date` 形如 `2026-08-12`，由调用方给（本模块不引时钟，便于测试）。
    ///
    /// 失败只记 warn 并置 `disabled`——审计写不进去绝不能影响远程会话本身，
    /// 但**必须说出来**：静默失效的审计比没有审计更糟。
    pub fn write_line(&mut self, dir: &Path, date: &str, line: &str) {
        if self.disabled {
            return;
        }
        if !self.ensure_open(dir, date) {
            return;
        }
        let Some((_, file)) = self.open.as_mut() else {
            return;
        };
        if let Err(e) = writeln!(file, "{line}") {
            log::warn!("审计日志写入失败，本次运行不再记录：{e}");
            self.disabled = true;
            self.open = None;
        }
    }

    /// 确保当天的文件是打开的；跨天会换文件并顺手清理过期的。
    fn ensure_open(&mut self, dir: &Path, date: &str) -> bool {
        if self.open.as_ref().is_some_and(|(d, _)| d == date) {
            return true;
        }
        if let Err(e) = std::fs::create_dir_all(dir) {
            log::warn!("审计目录不可用，本次运行不记录审计：{e}");
            self.disabled = true;
            return false;
        }
        let path = dir.join(format!("llm-{date}.jsonl"));
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                self.open = Some((date.to_string(), f));
                // 换文件是个天然的清理时机（每天至多一次，不需要额外定时器）。
                prune_old(dir, date);
                true
            }
            Err(e) => {
                log::warn!("审计日志打不开（{}），本次运行不记录：{e}", path.display());
                self.disabled = true;
                false
            }
        }
    }
}

/// 删掉超过保留期的审计文件。
///
/// **按文件名里的日期串比较，不看 mtime**：mtime 会被备份 / 同步盘 / 解压改掉，
/// 而文件名是我们自己写的、只可能被人手工改。
fn prune_old(dir: &Path, today: &str) {
    let Some(cutoff) = days_before(today, AUDIT_RETENTION_DAYS) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(date) = audit_file_date(name) else {
            continue;
        };
        // 字典序比较对 `YYYY-MM-DD` 就是时间序，不需要真的解析日期。
        if date.as_str() < cutoff.as_str() {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                log::debug!("清理过期审计文件失败（{name}）：{e}");
            }
        }
    }
}

/// 从 `llm-YYYY-MM-DD.jsonl` 里取出日期串；不匹配返回 `None`。
fn audit_file_date(name: &str) -> Option<String> {
    let rest = name.strip_prefix("llm-")?.strip_suffix(".jsonl")?;
    // 只认严格的 `YYYY-MM-DD`，别把用户手工放进来的别的文件删了。
    let ok = rest.len() == 10
        && rest.as_bytes()[4] == b'-'
        && rest.as_bytes()[7] == b'-'
        && rest.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    ok.then(|| rest.to_string())
}

/// `today` 往前 `days` 天的日期串。解析失败返回 `None`（此时不清理，宁可留着）。
fn days_before(today: &str, days: i64) -> Option<String> {
    let d = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d").ok()?;
    Some(
        d.checked_sub_signed(chrono::Duration::days(days))?
            .format("%Y-%m-%d")
            .to_string(),
    )
}

/// 审计目录（`<数据目录>/audit`）。数据目录不可用时 `None`。
#[must_use]
pub fn audit_dir() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("audit"))
}

/// 当天日期串（本地时区）。
#[must_use]
pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

// ── 全局入口 ─────────────────────────────────────────────────────────────────
//
// 审计日志是**天然的单例**（一个进程一个文件），而事件源散在四处：`llm_runner`
// （启停）、`remote_ws`（隐藏会话建立/结束）、`remote_ws/llm.rs`（工具调用、轮结束、
// 协议错误）。把它做成全局出口而不是往那四条调用链上层层传引用 —— 与 `applog` 同款。
//
// 可测性没有因此打折：判定与渲染全在上面那些纯函数里（已单测），这里只剩「往文件写」。

/// 全局审计日志。
static AUDIT: std::sync::OnceLock<std::sync::Mutex<AuditLog>> = std::sync::OnceLock::new();

/// 参数明文首段长度（由设置项 `audit_arg_head_len` 在启动与改设置时写入）。
static ARG_HEAD_LEN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(DEFAULT_ARG_HEAD_LEN);

/// 设置项变更时调用。
pub fn set_arg_head_len(n: usize) {
    ARG_HEAD_LEN.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// 当前的参数明文首段长度。
#[must_use]
pub fn arg_head_len() -> usize {
    ARG_HEAD_LEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// 记一条审计事件。`arg` 是工具参数（会按 [`arg_head_len`] 脱敏）。
///
/// **任何失败都不影响调用方**：审计写不进去不该让远程会话跟着挂掉。
/// 但失败会 `log::warn!` 一次并从此禁用自己（见 [`AuditLog::write_line`]）——
/// 静默失效的审计比没有审计更糟。
pub fn record(ev: &AuditEvent, arg: Option<&str>) {
    let Some(dir) = audit_dir() else {
        return;
    };
    let redacted = arg.map(|a| redact_arg(a, arg_head_len()));
    let line = render_line(
        chrono::Local::now().timestamp(),
        ev,
        redacted
            .as_ref()
            .map(|(h, s, b)| (h.as_str(), s.as_str(), *b)),
    );
    let cell = AUDIT.get_or_init(|| std::sync::Mutex::new(AuditLog::new()));
    // 锁中毒不 panic：审计是旁路，它自己把主流程打挂就本末倒置了。
    let mut guard = match cell.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    guard.write_line(&dir, &today(), &line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 片8_参数脱敏三件套() {
        let (head, sha, bytes) = redact_arg("cargo build -p lumen-app", 200);
        assert_eq!(head, "cargo build -p lumen-app");
        assert_eq!(bytes, 24);
        assert_eq!(sha.len(), 64, "sha256 十六进制是 64 字符");
    }

    #[test]
    fn 片8_长参数只留首段但哈希与长度是全量的() {
        // 这正是脱敏的意义：`rm -rf /` 这类看得见，长 heredoc 里的密钥看不见，
        // 而事后仍能用 sha256 证明「就是这一条」。
        let long = format!("echo start; {}; echo end", "x".repeat(1000));
        let (head, sha, bytes) = redact_arg(&long, 20);
        assert_eq!(head.chars().count(), 20);
        assert_eq!(bytes, long.len(), "字节数是全量的");
        assert_eq!(sha, redact_arg(&long, 0).1, "哈希与截多长无关");
    }

    #[test]
    fn 片8_头长为0即完全关闭明文() {
        let (head, sha, bytes) = redact_arg("secret-token-value", 0);
        assert!(head.is_empty());
        assert!(!sha.is_empty(), "哈希还在，仍可证明同一性");
        assert_eq!(bytes, 18);
    }

    #[test]
    fn 片8_按字符截而不是按字节() {
        // 按字节截会切出半个多字节字符，写进 jsonl 就是坏 JSON。
        let (head, _, bytes) = redact_arg("删除全部文件", 3);
        assert_eq!(head, "删除全");
        assert_eq!(bytes, 18, "6 个中文字符 = 18 字节");
    }

    #[test]
    fn 片8_控制字符必须转义否则一行会被撕成多行() {
        // 工具参数里出现裸换行是常态（heredoc、多行脚本）。不转义的话
        // 一条记录会变成好几行，整个文件从此没法按行解析。
        let line = render_line(
            1_754_640_000,
            &AuditEvent {
                ev: "ToolUse",
                tool: "Bash".into(),
                ..AuditEvent::new("ToolUse")
            },
            Some(("line1\nline2\ttab", "abc", 15)),
        );
        assert_eq!(line.lines().count(), 1, "必须是一行：{line}");
        assert!(line.contains("\\n"), "{line}");
        assert!(line.contains("\\t"), "{line}");
    }

    #[test]
    fn 片8_引号与反斜杠转义() {
        let line = render_line(1, &AuditEvent::new("X"), Some((r#"say "hi"\ok"#, "h", 1)));
        assert!(line.contains(r#"\"hi\""#), "{line}");
        assert!(line.contains(r"\\ok"), "{line}");
    }

    #[test]
    fn 片8_产出的是合法json且字段齐全() {
        let ev = AuditEvent {
            ev: "ToolUse",
            sid: 7,
            peer: "iPhone 15".into(),
            peer_dev: "a3f".into(),
            conv: 3,
            tool: "Bash".into(),
            cwd: "F:\\proj".into(),
            note: String::new(),
        };
        let line = render_line(1_754_640_000, &ev, Some(("cargo build", "9c1a", 11)));
        let v: serde_json::Value = serde_json::from_str(&line).expect("必须是合法 JSON");
        assert_eq!(v["ts"], 1_754_640_000);
        assert_eq!(v["ev"], "ToolUse");
        assert_eq!(v["sid"], 7);
        assert_eq!(v["tool"], "Bash");
        assert_eq!(v["arg_head"], "cargo build");
        assert_eq!(v["arg_sha256"], "9c1a");
        assert_eq!(v["arg_bytes"], 11);
        assert_eq!(v["cwd"], "F:\\proj");
    }

    #[test]
    fn 片8_无关字段不写进去() {
        // 每行都塞一堆空字段会让文件肉眼不可读，也让 jq 的 `has()` 判断失去意义。
        let line = render_line(1, &AuditEvent::new("RunnerStarted"), None);
        assert!(!line.contains("peer"), "{line}");
        assert!(!line.contains("arg_head"), "{line}");
        assert!(!line.contains("sid"), "{line}");
    }

    #[test]
    fn 片8_关掉明文时arg_head键仍然写出来() {
        // 让「关掉了明文」与「参数本来就是空的」在事后可区分：前者 arg_bytes > 0。
        let line = render_line(1, &AuditEvent::new("ToolUse"), Some(("", "abc", 500)));
        let v: serde_json::Value = serde_json::from_str(&line).expect("合法 JSON");
        assert_eq!(v["arg_head"], "");
        assert_eq!(v["arg_bytes"], 500);
    }

    // ── 文件名与清理 ────────────────────────────────────────────────────────

    #[test]
    fn 片8_只认严格的日期文件名() {
        assert_eq!(
            audit_file_date("llm-2026-08-12.jsonl").as_deref(),
            Some("2026-08-12")
        );
        // 别把用户手工放进这个目录的别的东西删了。
        assert!(audit_file_date("llm-2026-8-2.jsonl").is_none());
        assert!(audit_file_date("llm-note.jsonl").is_none());
        assert!(audit_file_date("other-2026-08-12.jsonl").is_none());
        assert!(audit_file_date("llm-2026-08-12.txt").is_none());
    }

    #[test]
    fn 片8_保留期按日期串比较() {
        let cutoff = days_before("2026-08-12", AUDIT_RETENTION_DAYS).expect("日期");
        assert_eq!(cutoff, "2026-07-29");
        // 字典序对 YYYY-MM-DD 就是时间序。
        assert!("2026-07-28" < cutoff.as_str(), "更老的要被清掉");
        assert!("2026-07-29" >= cutoff.as_str(), "刚好在保留期内的要留着");
    }

    #[test]
    fn 片8_跨月跨年的保留期() {
        assert_eq!(days_before("2026-01-05", 14).as_deref(), Some("2025-12-22"));
        assert_eq!(days_before("2026-03-01", 14).as_deref(), Some("2026-02-15"));
    }

    #[test]
    fn 片8_写入与按日期切文件() {
        let dir = std::env::temp_dir().join("lumen_audit_test_写入");
        let _ = std::fs::remove_dir_all(&dir);
        let mut log = AuditLog::new();
        log.write_line(&dir, "2026-08-12", "{\"a\":1}");
        log.write_line(&dir, "2026-08-12", "{\"a\":2}");
        log.write_line(&dir, "2026-08-13", "{\"a\":3}");

        let d1 = std::fs::read_to_string(dir.join("llm-2026-08-12.jsonl")).expect("读 12 日");
        assert_eq!(d1.lines().count(), 2);
        let d2 = std::fs::read_to_string(dir.join("llm-2026-08-13.jsonl")).expect("读 13 日");
        assert_eq!(d2.lines().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 片8_换文件时清掉过期的但留下不认识的() {
        let dir = std::env::temp_dir().join("lumen_audit_test_清理");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        std::fs::write(dir.join("llm-2026-07-01.jsonl"), "old").expect("写");
        std::fs::write(dir.join("llm-2026-08-11.jsonl"), "recent").expect("写");
        std::fs::write(dir.join("我的笔记.txt"), "keep").expect("写");

        let mut log = AuditLog::new();
        log.write_line(&dir, "2026-08-12", "{}");

        assert!(!dir.join("llm-2026-07-01.jsonl").exists(), "超期的要删");
        assert!(dir.join("llm-2026-08-11.jsonl").exists(), "保留期内的要留");
        assert!(dir.join("我的笔记.txt").exists(), "不认识的文件绝不能碰");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 片8_目录不可用时禁用自己而不是反复重试() {
        // 用一个「已存在的文件」当目录路径，create_dir_all 必失败。
        let f = std::env::temp_dir().join("lumen_audit_test_不是目录");
        std::fs::write(&f, "x").expect("写");
        let mut log = AuditLog::new();
        log.write_line(&f, "2026-08-12", "{}");
        assert!(log.disabled, "失败一次就该停手，不再每条日志刷一遍 warn");
        let _ = std::fs::remove_file(&f);
    }
}
