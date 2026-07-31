//! LLM CLI HUD 的后台数据采集。
//!
//! HUD 的基础数据来自终端画面；本模块补充 claude-hud 同类的会话、
//! transcript、工具、任务、token、额度和 Git 信息。所有文件与子进程
//! 访问都由 HUD 的后台线程调用，绝不阻塞 winit/egui 主线程。

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::llm_cli::LlmCliKind;

const MAX_ACTIVITY_NAME: usize = 48;
const MAX_ACTIVITY_TARGET: usize = 72;
const KIMI_USAGE_REFRESH_MS: u64 = 60_000;
const KIMI_USAGE_MAX_RESPONSE_BYTES: usize = 256 * 1024;
const KIMI_USAGE_URL: &str = "https://api.kimi.com/coding/v1/usages";

#[derive(Debug, Clone)]
pub struct CollectRequest {
    pub session_id: u64,
    pub kind: LlmCliKind,
    pub cwd: Option<PathBuf>,
    pub foreground_pid: Option<u32>,
    pub previous: Option<HudDetails>,
}

#[derive(Debug, Clone, Default)]
pub struct HudDetails {
    pub session_id: u64,
    pub source_path: Option<PathBuf>,
    pub session_name: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cli_version: Option<String>,
    pub session_started_ms: Option<u64>,
    pub last_response_ms: Option<u64>,
    pub tokens: Option<TokenUsage>,
    pub context_window: Option<u64>,
    pub context_tokens: Option<u64>,
    pub usage_windows: Vec<UsageWindow>,
    pub tools: Vec<ToolSummary>,
    pub agents: Vec<AgentSummary>,
    pub todos: TodoSummary,
    pub compactions: u32,
    pub git: Option<GitStatus>,
    pub config: Option<ConfigCounts>,
    pub native: Option<CliNativeDetails>,
    kimi_usage_checked_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl TokenUsage {
    pub fn total(self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cached)
            .saturating_add(self.cache_write)
    }

    fn is_empty(self) -> bool {
        self.total() == 0 && self.reasoning == 0
    }
}

#[derive(Debug, Clone)]
pub struct UsageWindow {
    pub label: String,
    pub used_percent: f32,
    pub resets_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct ToolSummary {
    pub name: String,
    pub completed: u32,
    pub errors: u32,
    pub running: u32,
    pub active_target: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentSummary {
    pub name: String,
    pub model: Option<String>,
    pub description: Option<String>,
    pub running: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TodoSummary {
    pub completed: u32,
    pub total: u32,
    pub active: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GitStatus {
    pub branch: String,
    pub dirty: bool,
    pub ahead: u32,
    pub behind: u32,
    pub modified: u32,
    pub added: u32,
    pub deleted: u32,
    pub untracked: u32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ConfigCounts {
    pub instructions: u32,
    pub rules: u32,
    pub mcp: u32,
    pub hooks: u32,
    pub skills: u32,
    pub plugins: u32,
    pub extensions: u32,
}

#[derive(Debug, Clone)]
pub enum CliNativeDetails {
    Claude(ClaudeNativeDetails),
    Codex(CodexNativeDetails),
    Kimi(KimiNativeDetails),
    Gemini(GeminiNativeDetails),
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeNativeDetails {
    pub user_turns: u32,
    pub assistant_messages: u32,
    pub attachments: u32,
    pub queue_operations: u32,
    pub server_tool_calls: u32,
    pub permission_mode: Option<String>,
    pub service_tier: Option<String>,
    pub speed: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CodexNativeDetails {
    pub provider: Option<String>,
    pub originator: Option<String>,
    pub thread_source: Option<String>,
    pub approval_policy: Option<String>,
    pub sandbox_policy: Option<String>,
    pub recent_messages: u32,
    pub recent_reasoning: u32,
}

#[derive(Debug, Clone, Default)]
pub struct KimiNativeDetails {
    pub provider: Option<String>,
    pub profile: Option<String>,
    pub permission_mode: Option<String>,
    pub protocol_version: Option<String>,
    pub turns: u32,
    pub steps: u32,
    pub thinking_blocks: u32,
    pub active_tools: u32,
    pub approvals: u32,
    pub background_tasks: u32,
    pub running_tasks: u32,
    pub yolo: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct GeminiNativeDetails {
    pub user_turns: u32,
    pub responses: u32,
    pub info_messages: u32,
    pub errors: u32,
    pub thought_tokens: u64,
    pub tool_tokens: u64,
    pub has_summary: bool,
}

#[derive(Debug, Clone)]
struct ToolState {
    name: String,
    target: Option<String>,
    status: ToolStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolStatus {
    Running,
    Completed,
    Error,
}

#[derive(Debug, Clone)]
struct TodoItem {
    content: String,
    status: String,
}

/// 收集一个 LLM 会话的丰富 HUD 数据。调用方必须在后台线程执行。
pub fn collect(request: CollectRequest) -> HudDetails {
    let git = request.cwd.as_deref().and_then(collect_git);
    let source = match request.kind {
        LlmCliKind::Claude => find_claude_transcript(&request),
        LlmCliKind::Codex => find_codex_transcript(request.cwd.as_deref()),
        LlmCliKind::Kimi => find_kimi_transcript(request.cwd.as_deref()),
        LlmCliKind::Gemini => find_gemini_transcript(request.cwd.as_deref()),
    };
    let source_modified = source
        .as_deref()
        .and_then(|path| source_modified_ms(request.kind, path));
    let previous_kimi_usage = request.previous.as_ref().map(|previous| {
        (
            previous.kimi_usage_checked_ms,
            previous.usage_windows.clone(),
        )
    });
    if let Some(mut previous) = request.previous {
        let unchanged = previous.source_path == source
            && source_modified.is_some()
            && previous.last_response_ms == source_modified;
        if unchanged {
            previous.session_id = request.session_id;
            previous.git = git;
            if request.kind == LlmCliKind::Kimi {
                refresh_kimi_usage(&mut previous);
            }
            return previous;
        }
    }

    let mut details = HudDetails {
        session_id: request.session_id,
        git,
        ..HudDetails::default()
    };
    if request.kind == LlmCliKind::Claude {
        details.session_started_ms = claude_started_at(request.foreground_pid);
    }
    details.source_path.clone_from(&source);

    if let Some(path) = source.as_deref() {
        if details.session_started_ms.is_none() {
            details.session_started_ms = file_created_ms(path);
        }
        match request.kind {
            LlmCliKind::Claude => parse_claude(path, &mut details),
            LlmCliKind::Codex => parse_codex(path, &mut details),
            LlmCliKind::Kimi => parse_kimi(path, &mut details),
            LlmCliKind::Gemini => parse_gemini(path, &mut details),
        }
        details.last_response_ms = source_modified;
    }
    details.config = Some(collect_cli_config(request.kind, request.cwd.as_deref()));
    if request.kind == LlmCliKind::Kimi {
        if let Some((checked, windows)) = previous_kimi_usage {
            details.kimi_usage_checked_ms = checked;
            details.usage_windows = windows;
        }
        refresh_kimi_usage(&mut details);
    }
    details
}

/// Kimi Code 的本地 transcript 只有本次会话 token；5 小时与周额度由官方
/// managed-account `/usages` 接口提供。读取 Kimi 自己的短期 access token，绝不
/// 记录或外传，且每分钟最多查询一次。请求失败时保留上次成功结果。
fn refresh_kimi_usage(details: &mut HudDetails) {
    let now = now_ms();
    if details
        .kimi_usage_checked_ms
        .is_some_and(|checked| now.saturating_sub(checked) < KIMI_USAGE_REFRESH_MS)
    {
        return;
    }
    details.kimi_usage_checked_ms = Some(now);
    if let Some(windows) = fetch_kimi_usage() {
        details.usage_windows = windows;
    }
}

#[derive(Deserialize)]
struct KimiCredential {
    access_token: String,
}

fn fetch_kimi_usage() -> Option<Vec<UsageWindow>> {
    let root = kimi_code_root()?;
    let raw =
        Zeroizing::new(fs::read_to_string(root.join("credentials").join("kimi-code.json")).ok()?);
    let credential = serde_json::from_str::<KimiCredential>(&raw).ok()?;
    let token = Zeroizing::new(credential.access_token);
    if token.is_empty() {
        return None;
    }
    let authorization = Zeroizing::new(format!("Bearer {}", token.as_str()));
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(3))
        .timeout_read(Duration::from_secs(5))
        .build();
    let response = agent
        .get(KIMI_USAGE_URL)
        .set("Authorization", authorization.as_str())
        .set("Accept", "application/json")
        .call()
        .ok()?;
    let mut body = String::new();
    response
        .into_reader()
        .take(KIMI_USAGE_MAX_RESPONSE_BYTES.saturating_add(1) as u64)
        .read_to_string(&mut body)
        .ok()?;
    if body.len() > KIMI_USAGE_MAX_RESPONSE_BYTES {
        return None;
    }
    let value = serde_json::from_str::<Value>(&body).ok()?;
    Some(parse_kimi_usage_windows(&value))
}

fn parse_kimi_usage_windows(value: &Value) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    if let Some(summary) = value.get("usage") {
        if let Some(window) = kimi_usage_window("7d".to_owned(), summary) {
            windows.push(window);
        }
    }
    if let Some(limits) = value.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let detail = limit.get("detail").unwrap_or(&Value::Null);
            let label = limit
                .get("window")
                .and_then(format_kimi_usage_window)
                .or_else(|| text_at(limit, "/name"))
                .unwrap_or_else(|| "limit".to_owned());
            if let Some(window) = kimi_usage_window(label, detail) {
                windows.push(window);
            }
        }
    }
    // 与 Codex/Claude HUD 保持阅读顺序：短周期在前，周周期在后。
    windows.sort_by_key(|window| if window.label == "7d" { 1 } else { 0 });
    windows
}

fn kimi_usage_window(label: String, value: &Value) -> Option<UsageWindow> {
    let used = flexible_u64(value.get("used")?)?;
    let limit = flexible_u64(value.get("limit")?)?;
    if limit == 0 {
        return None;
    }
    Some(UsageWindow {
        label,
        used_percent: (used as f64 * 100.0 / limit as f64).clamp(0.0, 100.0) as f32,
        resets_at_ms: value
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms),
    })
}

fn flexible_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse::<u64>().ok())
}

fn format_kimi_usage_window(window: &Value) -> Option<String> {
    let duration = flexible_u64(window.get("duration")?)?;
    match window.get("timeUnit").and_then(Value::as_str)? {
        "TIME_UNIT_MINUTE" if duration.is_multiple_of(60) => Some(format!("{}h", duration / 60)),
        "TIME_UNIT_MINUTE" => Some(format!("{duration}m")),
        "TIME_UNIT_HOUR" => Some(format!("{duration}h")),
        "TIME_UNIT_DAY" => Some(format!("{duration}d")),
        "TIME_UNIT_WEEK" => Some(format!("{}d", duration.saturating_mul(7))),
        _ => None,
    }
}

fn parse_rfc3339_ms(value: &str) -> Option<u64> {
    let millis = chrono::DateTime::parse_from_rfc3339(value)
        .ok()?
        .timestamp_millis();
    u64::try_from(millis).ok()
}

fn claude_started_at(foreground_pid: Option<u32>) -> Option<u64> {
    let pid = foreground_pid?;
    let session = home_dir()?
        .join(".claude")
        .join("sessions")
        .join(format!("{pid}.json"));
    read_json(&session)
        .ok()?
        .get("startedAt")
        .and_then(Value::as_u64)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

fn find_claude_transcript(request: &CollectRequest) -> Option<PathBuf> {
    let root = home_dir()?.join(".claude");
    if let Some(pid) = request.foreground_pid {
        let session_file = root.join("sessions").join(format!("{pid}.json"));
        if let Ok(value) = read_json(&session_file) {
            if let Some(started) = value.get("startedAt").and_then(Value::as_u64) {
                let _ = started;
            }
            if let Some(id) = value.get("sessionId").and_then(Value::as_str) {
                if let Some(path) =
                    find_named_file(&root.join("projects"), &format!("{id}.jsonl"), 2)
                {
                    return Some(path);
                }
            }
        }
    }
    let cwd = request.cwd.as_deref()?;
    let encoded = claude_project_dir_name(cwd);
    newest_matching_file(&root.join("projects").join(encoded), |path| {
        path.extension().is_some_and(|ext| ext == "jsonl")
    })
}

fn claude_project_dir_name(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|ch| match ch {
            ':' | '\\' | '/' => '-',
            other => other,
        })
        .collect()
}

fn find_codex_transcript(cwd: Option<&Path>) -> Option<PathBuf> {
    let root = home_dir()?.join(".codex").join("sessions");
    newest_session_matching_cwd(&root, cwd, "jsonl", 4)
}

fn find_kimi_transcript(cwd: Option<&Path>) -> Option<PathBuf> {
    [
        find_kimi_code_transcript(cwd),
        find_legacy_kimi_transcript(cwd),
    ]
    .into_iter()
    .flatten()
    .max_by_key(|path| kimi_source_modified_ms(path).unwrap_or(0))
}

fn find_kimi_code_transcript(cwd: Option<&Path>) -> Option<PathBuf> {
    let cwd = cwd?;
    let root = kimi_code_root()?;
    find_kimi_code_transcript_in(&root, cwd)
}

fn find_kimi_code_transcript_in(root: &Path, cwd: &Path) -> Option<PathBuf> {
    let index = root.join("session_index.jsonl");
    let file = File::open(index).ok()?;
    let mut best: Option<(u64, PathBuf)> = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(work_dir) = entry
            .get("workDir")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
        else {
            continue;
        };
        if !paths_equal(Path::new(work_dir), cwd) {
            continue;
        }
        let Some(session_dir) = entry
            .get("sessionDir")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
        else {
            continue;
        };
        let session_dir = PathBuf::from(session_dir);
        let wire = session_dir.join("agents").join("main").join("wire.jsonl");
        if !wire.is_file() {
            continue;
        }
        let modified = kimi_source_modified_ms(&wire).unwrap_or(0);
        if best
            .as_ref()
            .is_none_or(|(best_modified, _)| modified > *best_modified)
        {
            best = Some((modified, wire));
        }
    }
    best.map(|(_, path)| path)
}

fn find_legacy_kimi_transcript(cwd: Option<&Path>) -> Option<PathBuf> {
    let cwd = cwd?;
    let project_hash = format!("{:x}", md5::compute(cwd.to_string_lossy().as_bytes()));
    let root = home_dir()?
        .join(".kimi")
        .join("sessions")
        .join(project_hash);
    newest_matching_file(&root, |path| {
        path.file_name()
            .is_some_and(|name| matches!(name.to_str(), Some("wire.jsonl" | "context.jsonl")))
    })
}

fn find_gemini_transcript(cwd: Option<&Path>) -> Option<PathBuf> {
    let cwd = cwd?;
    let root = home_dir()?.join(".gemini").join("tmp");
    let mut best: Option<(u64, PathBuf)> = None;
    let Ok(entries) = fs::read_dir(root) else {
        return None;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(project_root) = fs::read_to_string(dir.join(".project_root")) else {
            continue;
        };
        if !paths_equal(Path::new(project_root.trim()), cwd) {
            continue;
        }
        if let Some(path) = newest_matching_file(&dir.join("chats"), |path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("session-")
                        && (name.ends_with(".json") || name.ends_with(".jsonl"))
                })
        }) {
            let modified = file_modified_ms(&path).unwrap_or(0);
            if best.as_ref().is_none_or(|(stamp, _)| modified > *stamp) {
                best = Some((modified, path));
            }
        }
    }
    best.map(|(_, path)| path)
}

fn newest_session_matching_cwd(
    root: &Path,
    cwd: Option<&Path>,
    extension: &str,
    depth: usize,
) -> Option<PathBuf> {
    let mut files = Vec::new();
    collect_files(root, depth, &mut files);
    files.retain(|path| path.extension().is_some_and(|ext| ext == extension));
    files.sort_by_key(|path| std::cmp::Reverse(file_modified_ms(path).unwrap_or(0)));
    files.into_iter().find(|path| {
        cwd.is_none()
            || cwd.is_some_and(|cwd| {
                first_json_line_cwd(path).is_some_and(|value| paths_equal(&value, cwd))
            })
    })
}

fn first_json_line_cwd(path: &Path) -> Option<PathBuf> {
    let file = File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    let value: Value = serde_json::from_str(&line).ok()?;
    value
        .pointer("/payload/cwd")
        .or_else(|| value.get("cwd"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

fn find_named_file(root: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().is_some_and(|candidate| candidate == name) {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = find_named_file(&path, name, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

fn newest_matching_file(root: &Path, predicate: impl Fn(&Path) -> bool + Copy) -> Option<PathBuf> {
    let mut files = Vec::new();
    collect_files(root, 3, &mut files);
    files
        .into_iter()
        .filter(|path| predicate(path))
        .max_by_key(|path| file_modified_ms(path).unwrap_or(0))
}

fn collect_files(root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, depth - 1, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(right.to_string_lossy().as_ref())
    } else {
        left == right
    }
}

fn parse_claude(path: &Path, details: &mut HudDetails) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let mut tools = HashMap::<String, ToolState>::new();
    let mut agents = HashMap::<String, AgentSummary>::new();
    let mut todos = Vec::<TodoItem>::new();
    let mut seen_messages = HashSet::<String>::new();
    let mut tokens = TokenUsage::default();
    let mut native = ClaudeNativeDetails::default();

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("user") => {
                native.user_turns = native.user_turns.saturating_add(1);
                if let Some(mode) = text_at(&entry, "/permissionMode") {
                    native.permission_mode = Some(mode);
                }
            }
            Some("assistant") => {
                native.assistant_messages = native.assistant_messages.saturating_add(1);
            }
            Some("attachment") => {
                native.attachments = native.attachments.saturating_add(1);
            }
            Some("queue-operation") => {
                native.queue_operations = native.queue_operations.saturating_add(1);
            }
            _ => {}
        }
        if details.cli_version.is_none() {
            details.cli_version = text_at(&entry, "/version");
        }
        if let Some(model) = text_at(&entry, "/message/model") {
            details.model = Some(model);
        }
        if let Some(effort) =
            text_at(&entry, "/effort").or_else(|| text_at(&entry, "/effort/level"))
        {
            details.effort = Some(effort);
        }
        if let Some(title) = text_at(&entry, "/customTitle").or_else(|| text_at(&entry, "/slug")) {
            details.session_name = Some(title);
        }
        if entry.get("type").and_then(Value::as_str) == Some("system")
            && entry.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        {
            details.compactions = details.compactions.saturating_add(1);
        }

        if entry.get("type").and_then(Value::as_str) == Some("assistant") {
            if let Some(usage) = entry.pointer("/message/usage") {
                if let Some(tier) = text_at(usage, "/service_tier") {
                    native.service_tier = Some(tier);
                }
                if let Some(speed) = text_at(usage, "/speed") {
                    native.speed = Some(speed);
                }
                native.server_tool_calls = native
                    .server_tool_calls
                    .saturating_add(sum_numeric_object(usage.get("server_tool_use")));
                let message_id = entry
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let count = message_id
                    .as_ref()
                    .is_none_or(|id| seen_messages.insert(id.clone()));
                if count {
                    tokens.input = tokens.input.saturating_add(uint_at(usage, "/input_tokens"));
                    tokens.output = tokens
                        .output
                        .saturating_add(uint_at(usage, "/output_tokens"));
                    tokens.cached = tokens
                        .cached
                        .saturating_add(uint_at(usage, "/cache_read_input_tokens"));
                    tokens.cache_write = tokens
                        .cache_write
                        .saturating_add(uint_at(usage, "/cache_creation_input_tokens"));
                }
            }
        }

        let Some(content) = entry.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let Some(id) = block.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(name) = block.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let input = block.get("input").unwrap_or(&Value::Null);
                    match name {
                        "Task" | "Agent" => {
                            agents.insert(
                                id.to_owned(),
                                AgentSummary {
                                    name: text_at(input, "/subagent_type")
                                        .unwrap_or_else(|| "agent".to_owned()),
                                    model: text_at(input, "/model"),
                                    description: text_at(input, "/description")
                                        .map(|value| truncate(&value, MAX_ACTIVITY_TARGET)),
                                    running: true,
                                },
                            );
                        }
                        "TodoWrite" => update_claude_todos(input, &mut todos),
                        "TaskCreate" => create_claude_todo(input, id, &mut todos),
                        "TaskUpdate" => update_claude_todo(input, &mut todos),
                        _ => {
                            tools.insert(
                                id.to_owned(),
                                ToolState {
                                    name: sanitize_name(name),
                                    target: tool_target(name, input),
                                    status: ToolStatus::Running,
                                },
                            );
                        }
                    }
                }
                Some("tool_result") => {
                    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let status = if block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Completed
                    };
                    if let Some(tool) = tools.get_mut(id) {
                        tool.status = status;
                    }
                    if let Some(agent) = agents.get_mut(id) {
                        agent.running = false;
                    }
                }
                _ => {}
            }
        }
    }
    if !tokens.is_empty() {
        details.tokens = Some(tokens);
    }
    details.tools = summarize_tools(tools.into_values());
    details.agents = agents.into_values().take(4).collect();
    details.todos = summarize_todos(&todos);
    details.native = Some(CliNativeDetails::Claude(native));
}

fn parse_codex(path: &Path, details: &mut HudDetails) {
    // Codex rollout 会很快长到几十 MB。最新 token_count 本身携带累计
    // totals，当前工具/计划也都在文件尾，因此只解析末 4 MiB；首行
    // session_meta 单独读取，避免 HUD 每两秒重扫整份历史。
    let mut native = CodexNativeDetails::default();
    if let Some(first) = first_json_value(path) {
        let payload = first.get("payload").unwrap_or(&Value::Null);
        details.cli_version = text_at(payload, "/cli_version");
        details.session_name = text_at(payload, "/thread_name")
            .or_else(|| text_at(payload, "/session_id"))
            .or_else(|| text_at(payload, "/id"));
        details.context_window = payload.get("context_window").and_then(Value::as_u64);
        native.provider = text_at(payload, "/model_provider");
        native.originator = text_at(payload, "/originator");
        native.thread_source = text_at(payload, "/thread_source");
    }
    let Some(reader) = tail_reader(path, 4 * 1024 * 1024) else {
        return;
    };
    let mut tools = HashMap::<String, ToolState>::new();
    let mut todos = Vec::<TodoItem>::new();
    for line in reader.lines().map_while(Result::ok) {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let outer = entry
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = entry.get("payload").unwrap_or(&Value::Null);
        let kind = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if outer == "turn_context" {
            if let Some(model) = text_at(payload, "/model") {
                details.model = Some(model);
            }
            if let Some(effort) =
                text_at(payload, "/effort").or_else(|| text_at(payload, "/reasoning_effort"))
            {
                details.effort = Some(effort);
            }
            if let Some(policy) = text_at(payload, "/approval_policy") {
                native.approval_policy = Some(policy);
            }
            if let Some(policy) = text_at(payload, "/sandbox_policy/type")
                .or_else(|| text_at(payload, "/sandbox_policy"))
            {
                native.sandbox_policy = Some(policy);
            }
        } else if outer == "event_msg" && kind == "token_count" {
            if let Some(info) = payload.get("info") {
                let usage = info.get("total_token_usage").unwrap_or(&Value::Null);
                let tokens = TokenUsage {
                    input: uint_at(usage, "/input_tokens"),
                    output: uint_at(usage, "/output_tokens"),
                    cached: uint_at(usage, "/cached_input_tokens"),
                    cache_write: uint_at(usage, "/cache_write_input_tokens"),
                    reasoning: uint_at(usage, "/reasoning_output_tokens"),
                };
                if !tokens.is_empty() {
                    details.tokens = Some(tokens);
                }
                details.context_window = info.get("model_context_window").and_then(Value::as_u64);
                details.context_tokens = info
                    .pointer("/last_token_usage/total_tokens")
                    .and_then(Value::as_u64);
            }
            details.usage_windows = codex_usage_windows(payload.get("rate_limits"));
        } else if outer == "event_msg" && kind == "context_compacted" {
            details.compactions = details.compactions.saturating_add(1);
        } else if outer == "event_msg" && kind == "agent_message" {
            native.recent_messages = native.recent_messages.saturating_add(1);
            details.last_response_ms = file_modified_ms(path);
        } else if outer == "response_item" && kind == "reasoning" {
            native.recent_reasoning = native.recent_reasoning.saturating_add(1);
        } else if outer == "response_item" && matches!(kind, "function_call" | "custom_tool_call") {
            let id = text_at(payload, "/call_id")
                .or_else(|| text_at(payload, "/id"))
                .unwrap_or_default();
            let name = text_at(payload, "/name").unwrap_or_else(|| "tool".to_owned());
            if name == "update_plan" {
                let args = text_at(payload, "/arguments")
                    .or_else(|| text_at(payload, "/input"))
                    .unwrap_or_default();
                update_codex_plan(&args, &mut todos);
            } else if !id.is_empty() {
                tools.insert(
                    id,
                    ToolState {
                        name: sanitize_name(&name),
                        target: codex_tool_target(payload),
                        status: if payload.get("status").and_then(Value::as_str)
                            == Some("completed")
                        {
                            ToolStatus::Completed
                        } else {
                            ToolStatus::Running
                        },
                    },
                );
            }
        } else if outer == "response_item"
            && matches!(kind, "function_call_output" | "custom_tool_call_output")
        {
            if let Some(id) = text_at(payload, "/call_id").or_else(|| text_at(payload, "/id")) {
                if let Some(tool) = tools.get_mut(&id) {
                    tool.status = ToolStatus::Completed;
                }
            }
        }
    }
    details.tools = summarize_tools(tools.into_values());
    details.todos = summarize_todos(&todos);
    details.native = Some(CliNativeDetails::Codex(native));
}

fn parse_kimi(path: &Path, details: &mut HudDetails) {
    let first = first_json_value(path);
    if first
        .as_ref()
        .and_then(|entry| entry.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            matches!(
                kind,
                "metadata"
                    | "config.update"
                    | "tools.set_active_tools"
                    | "permission.set_mode"
                    | "turn.prompt"
                    | "llm.request"
                    | "context.append_message"
                    | "context.append_loop_event"
                    | "usage.record"
            )
        })
    {
        parse_kimi_code_wire(path, details);
    } else if first
        .as_ref()
        .is_some_and(|entry| entry.get("message").is_some())
    {
        parse_legacy_kimi_wire(path, details);
    } else {
        parse_legacy_kimi_context(path, details);
    }
}

fn parse_kimi_code_wire(path: &Path, details: &mut HudDetails) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let mut tools = HashMap::<String, ToolState>::new();
    let mut tokens = TokenUsage::default();
    let mut native = KimiNativeDetails::default();
    let mut max_context = 0_u64;
    let mut remaining_context = None;

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("metadata") => {
                native.protocol_version = text_at(&entry, "/protocol_version");
            }
            Some("config.update") => {
                if let Some(model) = text_at(&entry, "/modelAlias") {
                    details.model = Some(model);
                }
                if let Some(effort) = text_at(&entry, "/thinkingEffort") {
                    details.effort = Some(effort);
                }
                if let Some(profile) = text_at(&entry, "/profileName") {
                    native.profile = Some(profile);
                }
            }
            Some("tools.set_active_tools") => {
                native.active_tools = entry
                    .get("names")
                    .and_then(Value::as_array)
                    .map_or(0, |names| names.len().min(u32::MAX as usize) as u32);
            }
            Some("permission.set_mode") => {
                native.permission_mode = text_at(&entry, "/mode");
            }
            Some("turn.prompt") => {
                native.turns = native.turns.saturating_add(1);
            }
            Some("llm.request") => {
                if let Some(model) =
                    text_at(&entry, "/modelAlias").or_else(|| text_at(&entry, "/model"))
                {
                    details.model = Some(model);
                }
                if let Some(effort) = text_at(&entry, "/thinkingEffort") {
                    details.effort = Some(effort);
                }
                if let Some(provider) = text_at(&entry, "/provider") {
                    native.provider = Some(provider);
                }
                if let Some(available) = entry.get("maxTokens").and_then(Value::as_u64) {
                    max_context = max_context.max(available);
                    remaining_context = Some(available);
                }
            }
            Some("usage.record") => {
                add_kimi_usage(entry.get("usage"), &mut tokens);
            }
            Some("context.append_loop_event") => {
                let event = entry.get("event").unwrap_or(&Value::Null);
                match event.get("type").and_then(Value::as_str) {
                    Some("step.begin") => {
                        native.steps = native.steps.saturating_add(1);
                    }
                    Some("content.part")
                        if event.pointer("/part/type").and_then(Value::as_str) == Some("think") =>
                    {
                        native.thinking_blocks = native.thinking_blocks.saturating_add(1);
                    }
                    Some("tool.call") => {
                        let Some(id) = text_at(event, "/toolCallId") else {
                            continue;
                        };
                        let name = text_at(event, "/name").unwrap_or_else(|| "tool".to_owned());
                        let args = event.get("args").unwrap_or(&Value::Null);
                        tools.insert(
                            id,
                            ToolState {
                                name: sanitize_name(&name),
                                target: tool_target(&name, args),
                                status: ToolStatus::Running,
                            },
                        );
                    }
                    Some("tool.result") => {
                        let Some(id) = text_at(event, "/toolCallId") else {
                            continue;
                        };
                        if let Some(tool) = tools.get_mut(&id) {
                            tool.status = if event
                                .pointer("/result/isError")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                            {
                                ToolStatus::Error
                            } else {
                                ToolStatus::Completed
                            };
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if let Some(model) = details.model.as_deref() {
        let config = kimi_model_config(model);
        native.provider = native.provider.or(config.provider);
        max_context = max_context.max(config.context_window.unwrap_or(0));
    }
    if !tokens.is_empty() {
        details.tokens = Some(tokens);
    }
    if max_context > 0 {
        details.context_window = Some(max_context);
        details.context_tokens =
            remaining_context.map(|remaining| max_context.saturating_sub(remaining));
    }
    details.tools = summarize_tools(tools.into_values());
    enrich_kimi_session(path, details, &mut native);
    details.native = Some(CliNativeDetails::Kimi(native));
}

fn parse_legacy_kimi_wire(path: &Path, details: &mut HudDetails) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let mut tools = HashMap::<String, ToolState>::new();
    let mut tokens = TokenUsage::default();
    let mut native = KimiNativeDetails::default();
    let mut context_fraction = None::<f64>;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let message = entry.get("message").unwrap_or(&Value::Null);
        let payload = message.get("payload").unwrap_or(&Value::Null);
        match message.get("type").and_then(Value::as_str) {
            Some("TurnBegin") => native.turns = native.turns.saturating_add(1),
            Some("StepBegin") => native.steps = native.steps.saturating_add(1),
            Some("ContentPart") if payload.get("type").and_then(Value::as_str) == Some("think") => {
                native.thinking_blocks = native.thinking_blocks.saturating_add(1);
            }
            Some("ApprovalRequest") => {
                native.approvals = native.approvals.saturating_add(1);
            }
            Some("ToolCall") => {
                let Some(id) = text_at(payload, "/id") else {
                    continue;
                };
                let name = text_at(payload, "/function/name").unwrap_or_else(|| "tool".to_owned());
                let args = payload
                    .pointer("/function/arguments")
                    .unwrap_or(&Value::Null);
                tools.insert(
                    id,
                    ToolState {
                        name: sanitize_name(&name),
                        target: tool_target(&name, args),
                        status: ToolStatus::Running,
                    },
                );
            }
            Some("ToolResult") => {
                if let Some(id) = text_at(payload, "/tool_call_id") {
                    if let Some(tool) = tools.get_mut(&id) {
                        tool.status = ToolStatus::Completed;
                    }
                }
            }
            Some("StatusUpdate") => {
                add_legacy_kimi_usage(payload.get("token_usage"), &mut tokens);
                context_fraction = payload.get("context_usage").and_then(Value::as_f64);
            }
            _ => {}
        }
    }
    let config = default_kimi_model_config(false);
    details.model.clone_from(&config.alias);
    native.provider = config.provider;
    if !tokens.is_empty() {
        details.tokens = Some(tokens);
    }
    if let Some(window) = config.context_window {
        details.context_window = Some(window);
        details.context_tokens = context_fraction.map(|fraction| {
            (fraction.clamp(0.0, 1.0) * window as f64)
                .round()
                .clamp(0.0, window as f64) as u64
        });
    }
    details.tools = summarize_tools(tools.into_values());
    enrich_kimi_session(path, details, &mut native);
    details.native = Some(CliNativeDetails::Kimi(native));
}

fn parse_legacy_kimi_context(path: &Path, details: &mut HudDetails) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let mut tools = HashMap::<String, ToolState>::new();
    let mut token_count = 0;
    let mut native = KimiNativeDetails::default();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match entry.get("role").and_then(Value::as_str) {
            Some("_usage") => {
                token_count = entry
                    .get("token_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(token_count);
            }
            Some("user") => native.turns = native.turns.saturating_add(1),
            Some("assistant") => {
                if let Some(calls) = entry.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let Some(id) = text_at(call, "/id") else {
                            continue;
                        };
                        let name =
                            text_at(call, "/function/name").unwrap_or_else(|| "tool".to_owned());
                        let args = call.pointer("/function/arguments").unwrap_or(&Value::Null);
                        tools.insert(
                            id,
                            ToolState {
                                name: sanitize_name(&name),
                                target: tool_target(&name, args),
                                status: ToolStatus::Running,
                            },
                        );
                    }
                }
            }
            Some("tool") => {
                if let Some(id) = text_at(&entry, "/tool_call_id") {
                    if let Some(tool) = tools.get_mut(&id) {
                        tool.status = ToolStatus::Completed;
                    }
                }
            }
            _ => {}
        }
    }
    let config = default_kimi_model_config(false);
    details.model.clone_from(&config.alias);
    native.provider = config.provider;
    if token_count > 0 {
        details.tokens = Some(TokenUsage {
            input: token_count,
            ..TokenUsage::default()
        });
    }
    details.context_window = config.context_window;
    details.tools = summarize_tools(tools.into_values());
    enrich_kimi_session(path, details, &mut native);
    details.native = Some(CliNativeDetails::Kimi(native));
}

fn parse_gemini(path: &Path, details: &mut HudDetails) {
    let value = read_json(path).ok().or_else(|| first_json_value(path));
    let Some(value) = value else {
        return;
    };
    details.session_name = text_at(&value, "/sessionId");
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut tokens = TokenUsage::default();
    let mut tools = HashMap::<String, ToolState>::new();
    let mut native = GeminiNativeDetails {
        has_summary: value.get("summary").is_some(),
        ..GeminiNativeDetails::default()
    };
    for message in messages {
        match message.get("type").and_then(Value::as_str) {
            Some("user") => {
                native.user_turns = native.user_turns.saturating_add(1);
            }
            Some("info") => {
                native.info_messages = native.info_messages.saturating_add(1);
            }
            Some("error") => {
                native.errors = native.errors.saturating_add(1);
            }
            Some("gemini") => {
                native.responses = native.responses.saturating_add(1);
                if let Some(model) = text_at(&message, "/model") {
                    details.model = Some(model);
                }
                if let Some(value) = message.get("tokens") {
                    tokens.input = tokens.input.saturating_add(uint_at(value, "/input"));
                    tokens.output = tokens.output.saturating_add(uint_at(value, "/output"));
                    tokens.cached = tokens.cached.saturating_add(uint_at(value, "/cached"));
                    tokens.reasoning = tokens.reasoning.saturating_add(uint_at(value, "/thoughts"));
                    native.thought_tokens = native
                        .thought_tokens
                        .saturating_add(uint_at(value, "/thoughts"));
                    native.tool_tokens = native.tool_tokens.saturating_add(uint_at(value, "/tool"));
                }
                if let Some(calls) = message.get("toolCalls").and_then(Value::as_array) {
                    for (index, call) in calls.iter().enumerate() {
                        let id = text_at(call, "/id").unwrap_or_else(|| {
                            format!(
                                "{}-{index}",
                                message.get("id").and_then(Value::as_str).unwrap_or("tool")
                            )
                        });
                        let name = text_at(call, "/name")
                            .or_else(|| text_at(call, "/function/name"))
                            .unwrap_or_else(|| "tool".to_owned());
                        let status = match call.get("status").and_then(Value::as_str) {
                            Some("error" | "failed") => ToolStatus::Error,
                            Some("pending" | "running") => ToolStatus::Running,
                            _ => ToolStatus::Completed,
                        };
                        tools.insert(
                            id,
                            ToolState {
                                name: sanitize_name(&name),
                                target: text_at(call, "/displayName")
                                    .or_else(|| text_at(call, "/args"))
                                    .map(|value| truncate(&value, MAX_ACTIVITY_TARGET)),
                                status,
                            },
                        );
                    }
                }
            }
            _ => {}
        }
    }
    if !tokens.is_empty() {
        details.tokens = Some(tokens);
    }
    details.tools = summarize_tools(tools.into_values());
    details.native = Some(CliNativeDetails::Gemini(native));
}

#[derive(Default)]
struct KimiModelConfig {
    alias: Option<String>,
    provider: Option<String>,
    context_window: Option<u64>,
}

fn add_kimi_usage(value: Option<&Value>, tokens: &mut TokenUsage) {
    let Some(value) = value else {
        return;
    };
    tokens.input = tokens.input.saturating_add(uint_at(value, "/inputOther"));
    tokens.output = tokens.output.saturating_add(uint_at(value, "/output"));
    tokens.cached = tokens
        .cached
        .saturating_add(uint_at(value, "/inputCacheRead"));
    tokens.cache_write = tokens
        .cache_write
        .saturating_add(uint_at(value, "/inputCacheCreation"));
}

fn add_legacy_kimi_usage(value: Option<&Value>, tokens: &mut TokenUsage) {
    let Some(value) = value else {
        return;
    };
    tokens.input = tokens.input.saturating_add(uint_at(value, "/input_other"));
    tokens.output = tokens.output.saturating_add(uint_at(value, "/output"));
    tokens.cached = tokens
        .cached
        .saturating_add(uint_at(value, "/input_cache_read"));
    tokens.cache_write = tokens
        .cache_write
        .saturating_add(uint_at(value, "/input_cache_creation"));
}

fn kimi_model_config(alias: &str) -> KimiModelConfig {
    let mut roots = Vec::new();
    if let Some(root) = kimi_code_root() {
        roots.push(root);
    }
    if let Some(home) = home_dir() {
        roots.push(home.join(".kimi"));
    }
    for root in roots {
        if let Some(config) = parse_kimi_model_config(&root.join("config.toml"), Some(alias)) {
            return config;
        }
    }
    KimiModelConfig {
        alias: Some(alias.to_owned()),
        ..KimiModelConfig::default()
    }
}

fn default_kimi_model_config(prefer_code: bool) -> KimiModelConfig {
    let mut roots = Vec::new();
    if prefer_code {
        if let Some(root) = kimi_code_root() {
            roots.push(root);
        }
    }
    if let Some(home) = home_dir() {
        roots.push(home.join(".kimi"));
    }
    if !prefer_code {
        if let Some(root) = kimi_code_root() {
            roots.push(root);
        }
    }
    for root in roots {
        if let Some(config) = parse_kimi_model_config(&root.join("config.toml"), None) {
            return config;
        }
    }
    KimiModelConfig::default()
}

fn parse_kimi_model_config(path: &Path, requested: Option<&str>) -> Option<KimiModelConfig> {
    let text = fs::read_to_string(path).ok()?;
    let default_alias = text
        .lines()
        .find_map(|line| toml_assignment(line, "default_model"));
    let alias = requested
        .map(str::to_owned)
        .or_else(|| default_alias.clone())?;
    let section = format!("[models.\"{alias}\"]");
    let mut in_model = false;
    let mut provider = None;
    let mut context_window = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_model = trimmed == section;
            continue;
        }
        if !in_model {
            continue;
        }
        provider = provider.or_else(|| toml_assignment(trimmed, "provider"));
        context_window = context_window.or_else(|| {
            toml_assignment(trimmed, "max_context_size")?
                .parse::<u64>()
                .ok()
        });
    }
    Some(KimiModelConfig {
        alias: Some(alias),
        provider,
        context_window,
    })
}

fn toml_assignment(line: &str, key: &str) -> Option<String> {
    let (candidate, value) = line.split_once('=')?;
    if candidate.trim() != key {
        return None;
    }
    let value = value.split('#').next()?.trim();
    Some(value.trim_matches('"').trim_matches('\'').to_owned())
}

fn kimi_code_root() -> Option<PathBuf> {
    std::env::var_os("KIMI_CODE_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".kimi-code")))
}

fn kimi_session_root(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    if parent.file_name().and_then(|name| name.to_str()) == Some("main")
        && parent
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("agents")
    {
        parent.parent()?.parent().map(Path::to_path_buf)
    } else {
        Some(parent.to_path_buf())
    }
}

fn enrich_kimi_session(source: &Path, details: &mut HudDetails, native: &mut KimiNativeDetails) {
    let Some(session_root) = kimi_session_root(source) else {
        return;
    };
    let state_path = session_root.join("state.json");
    if let Ok(state) = read_json(&state_path) {
        if let Some(title) = text_at(&state, "/title") {
            details.session_name = Some(truncate(&title, MAX_ACTIVITY_TARGET));
        }
        native.yolo = state
            .pointer("/approval/yolo")
            .and_then(Value::as_bool)
            .or_else(|| {
                state
                    .pointer("/custom/approval/yolo")
                    .and_then(Value::as_bool)
            });
        if details.session_started_ms.is_none() {
            details.session_started_ms = file_created_ms(&state_path);
        }
    }

    let agents_root = session_root.join("agents");
    let mut agents = Vec::new();
    if let Ok(entries) = fs::read_dir(&agents_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || entry.file_name() == "main" {
                continue;
            }
            let wire = path.join("wire.jsonl");
            let running = file_modified_ms(&wire)
                .is_some_and(|modified| now_ms().saturating_sub(modified) <= 30_000);
            agents.push(AgentSummary {
                name: sanitize_name(&entry.file_name().to_string_lossy()),
                model: None,
                description: None,
                running,
            });
        }
    }
    if !agents.is_empty() {
        details.agents = agents;
    }

    let mut task_files = Vec::new();
    for root in [
        session_root.join("tasks"),
        session_root.join("agents").join("main").join("tasks"),
    ] {
        if let Ok(entries) = fs::read_dir(root) {
            task_files.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
                path.is_file() && path.extension().is_some_and(|ext| ext == "json")
            }));
        }
    }
    native.background_tasks = task_files.len().min(u32::MAX as usize) as u32;
    native.running_tasks = task_files
        .iter()
        .filter(|path| {
            read_json(path).ok().is_some_and(|task| {
                text_at(&task, "/status").is_some_and(|status| {
                    matches!(status.as_str(), "pending" | "running" | "in_progress")
                })
            })
        })
        .count()
        .min(u32::MAX as usize) as u32;
}

fn update_claude_todos(input: &Value, todos: &mut Vec<TodoItem>) {
    let Some(items) = input.get("todos").and_then(Value::as_array) else {
        return;
    };
    todos.clear();
    for item in items {
        if let Some(content) = text_at(item, "/content") {
            todos.push(TodoItem {
                content: truncate(&content, MAX_ACTIVITY_TARGET),
                status: text_at(item, "/status").unwrap_or_else(|| "pending".to_owned()),
            });
        }
    }
}

fn create_claude_todo(input: &Value, _id: &str, todos: &mut Vec<TodoItem>) {
    let content = text_at(input, "/subject")
        .or_else(|| text_at(input, "/description"))
        .unwrap_or_else(|| "Task".to_owned());
    todos.push(TodoItem {
        content: truncate(&content, MAX_ACTIVITY_TARGET),
        status: text_at(input, "/status").unwrap_or_else(|| "pending".to_owned()),
    });
}

fn update_claude_todo(input: &Value, todos: &mut [TodoItem]) {
    let Some(index) = input
        .get("taskId")
        .and_then(Value::as_u64)
        .and_then(|id| usize::try_from(id).ok())
        .and_then(|id| id.checked_sub(1))
    else {
        return;
    };
    let Some(todo) = todos.get_mut(index) else {
        return;
    };
    if let Some(status) = text_at(input, "/status") {
        todo.status = status;
    }
    if let Some(content) = text_at(input, "/subject").or_else(|| text_at(input, "/description")) {
        todo.content = truncate(&content, MAX_ACTIVITY_TARGET);
    }
}

fn update_codex_plan(arguments: &str, todos: &mut Vec<TodoItem>) {
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return;
    };
    let Some(plan) = value.get("plan").and_then(Value::as_array) else {
        return;
    };
    todos.clear();
    for item in plan {
        if let Some(content) = text_at(item, "/step") {
            todos.push(TodoItem {
                content: truncate(&content, MAX_ACTIVITY_TARGET),
                status: text_at(item, "/status").unwrap_or_else(|| "pending".to_owned()),
            });
        }
    }
}

fn summarize_tools(tools: impl IntoIterator<Item = ToolState>) -> Vec<ToolSummary> {
    let mut summaries = HashMap::<String, ToolSummary>::new();
    for tool in tools {
        let summary = summaries
            .entry(tool.name.clone())
            .or_insert_with(|| ToolSummary {
                name: tool.name,
                ..ToolSummary::default()
            });
        match tool.status {
            ToolStatus::Running => {
                summary.running = summary.running.saturating_add(1);
                if tool.target.is_some() {
                    summary.active_target = tool.target;
                }
            }
            ToolStatus::Completed => {
                summary.completed = summary.completed.saturating_add(1);
            }
            ToolStatus::Error => {
                summary.errors = summary.errors.saturating_add(1);
            }
        }
    }
    let mut values = summaries.into_values().collect::<Vec<_>>();
    values.sort_by_key(|tool| {
        (
            std::cmp::Reverse(tool.running),
            std::cmp::Reverse(tool.completed.saturating_add(tool.errors)),
            tool.name.clone(),
        )
    });
    values.truncate(6);
    values
}

fn summarize_todos(todos: &[TodoItem]) -> TodoSummary {
    let completed = todos
        .iter()
        .filter(|todo| todo.status == "completed")
        .count() as u32;
    let active = todos
        .iter()
        .find(|todo| matches!(todo.status.as_str(), "in_progress" | "running"))
        .map(|todo| todo.content.clone());
    TodoSummary {
        completed,
        total: todos.len() as u32,
        active,
    }
}

fn codex_usage_windows(value: Option<&Value>) -> Vec<UsageWindow> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut windows = Vec::new();
    for (key, fallback) in [("primary", "5h"), ("secondary", "7d")] {
        let Some(window) = value.get(key) else {
            continue;
        };
        let Some(percent) = window.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        let label = window
            .get("window_minutes")
            .and_then(Value::as_u64)
            .map(format_window_minutes)
            .unwrap_or_else(|| fallback.to_owned());
        windows.push(UsageWindow {
            label,
            used_percent: percent.clamp(0.0, 100.0) as f32,
            resets_at_ms: window
                .get("resets_at")
                .and_then(Value::as_u64)
                .map(|seconds| seconds.saturating_mul(1000)),
        });
    }
    windows
}

fn format_window_minutes(minutes: u64) -> String {
    if minutes.is_multiple_of(24 * 60) {
        format!("{}d", minutes / (24 * 60))
    } else if minutes.is_multiple_of(60) {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

fn collect_git(cwd: &Path) -> Option<GitStatus> {
    let mut command = Command::new("git");
    command.args([
        "-c",
        "core.quotepath=false",
        "-C",
        cwd.to_string_lossy().as_ref(),
        "status",
        "--porcelain=v1",
        "--branch",
    ]);
    hide_console(&mut command);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_git_status(&String::from_utf8_lossy(&output.stdout))
}

fn parse_git_status(output: &str) -> Option<GitStatus> {
    let mut lines = output.lines();
    let header = lines.next()?.strip_prefix("## ")?;
    let mut status = GitStatus::default();
    let branch_part = header.split("...").next().unwrap_or(header);
    status.branch = branch_part
        .trim_end_matches(" (no branch)")
        .trim()
        .to_owned();
    if let Some(meta) = header
        .split_once('[')
        .and_then(|(_, tail)| tail.split_once(']'))
    {
        for part in meta.0.split(',').map(str::trim) {
            if let Some(value) = part.strip_prefix("ahead ") {
                status.ahead = value.parse().unwrap_or(0);
            } else if let Some(value) = part.strip_prefix("behind ") {
                status.behind = value.parse().unwrap_or(0);
            }
        }
    }
    for line in lines {
        if line.len() < 2 {
            continue;
        }
        status.dirty = true;
        let code = &line[..2];
        if code == "??" {
            status.untracked = status.untracked.saturating_add(1);
        } else {
            if code.contains('A') {
                status.added = status.added.saturating_add(1);
            }
            if code.contains('D') {
                status.deleted = status.deleted.saturating_add(1);
            }
            if code.chars().any(|ch| matches!(ch, 'M' | 'R' | 'C' | 'U')) {
                status.modified = status.modified.saturating_add(1);
            }
        }
    }
    Some(status)
}

fn collect_cli_config(kind: LlmCliKind, cwd: Option<&Path>) -> ConfigCounts {
    match kind {
        LlmCliKind::Claude => collect_claude_config(cwd),
        LlmCliKind::Codex => collect_codex_config(cwd),
        LlmCliKind::Kimi => collect_kimi_config(cwd),
        LlmCliKind::Gemini => collect_gemini_config(cwd),
    }
}

fn collect_claude_config(cwd: Option<&Path>) -> ConfigCounts {
    let mut out = ConfigCounts::default();
    let Some(home) = home_dir() else {
        return out;
    };
    let claude_root = home.join(".claude");
    if claude_root.join("CLAUDE.md").is_file() {
        out.instructions += 1;
    }
    if let Some(mut dir) = cwd.map(Path::to_path_buf) {
        loop {
            if dir.join("CLAUDE.md").is_file() {
                out.instructions = out.instructions.saturating_add(1);
            }
            let rules = dir.join(".claude").join("rules");
            out.rules = out
                .rules
                .saturating_add(count_files_with_extension(&rules, "md"));
            if !dir.pop() {
                break;
            }
        }
    }
    for settings in [
        claude_root.join("settings.json"),
        cwd.unwrap_or(Path::new("."))
            .join(".claude")
            .join("settings.json"),
    ] {
        let Ok(value) = read_json(&settings) else {
            continue;
        };
        out.mcp = out.mcp.saturating_add(
            value
                .get("mcpServers")
                .and_then(Value::as_object)
                .map_or(0, |servers| servers.len() as u32),
        );
        out.hooks = out.hooks.saturating_add(
            value
                .get("hooks")
                .and_then(Value::as_object)
                .map_or(0, |hooks| hooks.len() as u32),
        );
    }
    out
}

fn collect_codex_config(cwd: Option<&Path>) -> ConfigCounts {
    let mut out = ConfigCounts {
        instructions: count_instruction_files(cwd, &["AGENTS.md"]),
        ..ConfigCounts::default()
    };
    let Some(home) = home_dir() else {
        return out;
    };
    let root = home.join(".codex");
    out.skills = count_directories(&root.join("skills"));
    if let Some(cwd) = cwd {
        out.skills = out
            .skills
            .saturating_add(count_directories(&cwd.join(".codex").join("skills")));
    }
    if let Ok(config) = fs::read_to_string(root.join("config.toml")) {
        out.mcp = count_toml_sections(&config, &["mcp_servers.", "mcp.servers."]);
    }
    out
}

fn collect_kimi_config(cwd: Option<&Path>) -> ConfigCounts {
    let mut out = ConfigCounts {
        instructions: count_instruction_files(cwd, &["AGENTS.md", "KIMI.md"]),
        ..ConfigCounts::default()
    };
    let Some(root) = kimi_code_root() else {
        return out;
    };
    out.skills = count_directories(&root.join("skills"));
    out.plugins = count_directories(&root.join("plugins").join("managed"));
    if let Some(cwd) = cwd {
        out.skills = out
            .skills
            .saturating_add(count_directories(&cwd.join(".kimi").join("skills")));
    }
    if let Ok(config) = fs::read_to_string(root.join("config.toml")) {
        out.mcp = count_toml_sections(&config, &["mcp_servers.", "mcp.servers."]);
    }
    out
}

fn collect_gemini_config(cwd: Option<&Path>) -> ConfigCounts {
    let mut out = ConfigCounts {
        instructions: count_instruction_files(cwd, &["GEMINI.md"]),
        ..ConfigCounts::default()
    };
    let Some(home) = home_dir() else {
        return out;
    };
    let root = home.join(".gemini");
    out.skills = count_directories(&root.join("skills"));
    out.extensions = count_directories(&root.join("extensions"));
    for settings in [
        root.join("settings.json"),
        cwd.unwrap_or(Path::new("."))
            .join(".gemini")
            .join("settings.json"),
    ] {
        let Ok(value) = read_json(&settings) else {
            continue;
        };
        out.mcp = out.mcp.saturating_add(
            value
                .get("mcpServers")
                .or_else(|| value.pointer("/mcp/servers"))
                .and_then(Value::as_object)
                .map_or(0, |servers| servers.len().min(u32::MAX as usize) as u32),
        );
    }
    out
}

fn count_instruction_files(cwd: Option<&Path>, names: &[&str]) -> u32 {
    let mut count = 0_u32;
    let Some(mut dir) = cwd.map(Path::to_path_buf) else {
        return count;
    };
    loop {
        for name in names {
            if dir.join(name).is_file() {
                count = count.saturating_add(1);
            }
        }
        if !dir.pop() {
            break;
        }
    }
    count
}

fn count_directories(root: &Path) -> u32 {
    fs::read_dir(root).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .count()
            .min(u32::MAX as usize) as u32
    })
}

fn count_toml_sections(text: &str, prefixes: &[&str]) -> u32 {
    text.lines()
        .filter(|line| {
            let section = line.trim().trim_start_matches('[');
            prefixes.iter().any(|prefix| section.starts_with(prefix))
        })
        .count()
        .min(u32::MAX as usize) as u32
}

fn count_files_with_extension(root: &Path, extension: &str) -> u32 {
    fs::read_dir(root).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|entry| {
                entry.path().is_file()
                    && entry.path().extension().is_some_and(|ext| ext == extension)
            })
            .count() as u32
    })
}

fn tool_target(name: &str, input: &Value) -> Option<String> {
    let pointers: &[&str] = match name {
        "Read" | "Edit" | "Write" => &["/file_path", "/path"],
        "Grep" => &["/pattern", "/path"],
        "Glob" => &["/pattern"],
        "Bash" => &["/command", "/description"],
        "WebFetch" | "WebSearch" => &["/url", "/query"],
        _ => &["/description", "/path", "/command", "/query"],
    };
    pointers
        .iter()
        .find_map(|pointer| text_at(input, pointer))
        .map(|value| truncate(&value, MAX_ACTIVITY_TARGET))
}

fn codex_tool_target(payload: &Value) -> Option<String> {
    text_at(payload, "/arguments")
        .or_else(|| text_at(payload, "/input"))
        .map(|value| {
            serde_json::from_str::<Value>(&value)
                .ok()
                .and_then(|args| {
                    ["/command", "/path", "/query", "/task_name", "/prompt"]
                        .iter()
                        .find_map(|pointer| text_at(&args, pointer))
                })
                .unwrap_or(value)
        })
        .map(|value| truncate(&value, MAX_ACTIVITY_TARGET))
}

fn text_at(value: &Value, pointer: &str) -> Option<String> {
    let value = value.pointer(pointer)?;
    match value {
        Value::String(text) if !text.trim().is_empty() => {
            Some(sanitize_text(text, MAX_ACTIVITY_TARGET))
        }
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Object(_) | Value::Array(_) => {
            Some(truncate(&value.to_string(), MAX_ACTIVITY_TARGET))
        }
        _ => None,
    }
}

fn uint_at(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

fn sum_numeric_object(value: Option<&Value>) -> u32 {
    value
        .and_then(Value::as_object)
        .map(|values| {
            values
                .values()
                .filter_map(Value::as_u64)
                .fold(0_u64, u64::saturating_add)
                .min(u64::from(u32::MAX)) as u32
        })
        .unwrap_or(0)
}

fn sanitize_name(value: &str) -> String {
    sanitize_text(value, MAX_ACTIVITY_NAME)
}

fn sanitize_text(value: &str, max: usize) -> String {
    let clean = value
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();
    truncate(clean.trim(), max)
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let mut out = value
        .chars()
        .take(max.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

fn read_json(path: &Path) -> Result<Value, serde_json::Error> {
    let text = fs::read_to_string(path).map_err(serde_json::Error::io)?;
    serde_json::from_str(&text)
}

fn first_json_value(path: &Path) -> Option<Value> {
    let file = File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn tail_reader(path: &Path, max_bytes: u64) -> Option<BufReader<File>> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut reader = BufReader::new(file);
    if start > 0 {
        // 起点大概率落在一条 JSON 中间，丢弃残行。
        let mut partial = String::new();
        reader.read_line(&mut partial).ok()?;
    }
    Some(reader)
}

fn source_modified_ms(kind: LlmCliKind, path: &Path) -> Option<u64> {
    if kind == LlmCliKind::Kimi {
        kimi_source_modified_ms(path)
    } else {
        file_modified_ms(path)
    }
}

fn kimi_source_modified_ms(path: &Path) -> Option<u64> {
    let mut modified = file_modified_ms(path).unwrap_or(0);
    if let Some(session_root) = kimi_session_root(path) {
        modified = modified.max(file_modified_ms(&session_root.join("state.json")).unwrap_or(0));
        for tasks in [
            session_root.join("tasks"),
            session_root.join("agents").join("main").join("tasks"),
        ] {
            if let Ok(entries) = fs::read_dir(tasks) {
                for entry in entries.flatten() {
                    modified = modified.max(file_modified_ms(&entry.path()).unwrap_or(0));
                }
            }
        }
    }
    (modified > 0).then_some(modified)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u128::from(u64::MAX)) as u64
        })
}

fn file_modified_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
}

fn file_created_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .created()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_jsonl(name: &str, content: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("lumen-hud-{}-{name}.jsonl", std::process::id()));
        fs::write(&path, content).expect("write HUD fixture");
        path
    }

    #[test]
    fn claude_project_path_encoding_matches_cli_layout() {
        assert_eq!(
            claude_project_dir_name(Path::new(r"F:\ClaudeWorkspaces\Lumen")),
            "F--ClaudeWorkspaces-Lumen"
        );
    }

    #[test]
    fn parses_git_branch_dirty_counts_and_tracking() {
        let status = parse_git_status(
            "## feature/hud...origin/feature/hud [ahead 2, behind 1]\n M a.rs\nA  b.rs\n D c.rs\n?? d.rs\n",
        )
        .expect("git status");
        assert_eq!(status.branch, "feature/hud");
        assert!(status.dirty);
        assert_eq!((status.ahead, status.behind), (2, 1));
        assert_eq!(
            (
                status.modified,
                status.added,
                status.deleted,
                status.untracked
            ),
            (1, 1, 1, 1)
        );
    }

    #[test]
    fn tool_summary_prioritizes_running_activity() {
        let values = summarize_tools([
            ToolState {
                name: "Read".into(),
                target: None,
                status: ToolStatus::Completed,
            },
            ToolState {
                name: "Edit".into(),
                target: Some("src/main.rs".into()),
                status: ToolStatus::Running,
            },
        ]);
        assert_eq!(values[0].name, "Edit");
        assert_eq!(values[0].running, 1);
    }

    #[test]
    fn claude_transcript_supplies_model_tokens_tools_todos_and_compactions() {
        let path = temp_jsonl(
            "claude",
            concat!(
                "{\"type\":\"assistant\",\"version\":\"2.1.0\",\"message\":{\"id\":\"m1\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":100,\"output_tokens\":20,\"cache_read_input_tokens\":30,\"cache_creation_input_tokens\":4},\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Read\",\"input\":{\"file_path\":\"src/main.rs\"}},{\"type\":\"tool_use\",\"id\":\"todo\",\"name\":\"TodoWrite\",\"input\":{\"todos\":[{\"content\":\"Fix HUD\",\"status\":\"in_progress\"}]}}]}}\n",
                "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"t1\"}]}}\n",
                "{\"type\":\"system\",\"subtype\":\"compact_boundary\"}\n"
            ),
        );
        let mut details = HudDetails::default();
        parse_claude(&path, &mut details);
        let _ = fs::remove_file(path);

        assert_eq!(details.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(details.tokens.expect("tokens").total(), 154);
        assert_eq!(details.tools[0].completed, 1);
        assert_eq!(details.todos.total, 1);
        assert_eq!(details.todos.active.as_deref(), Some("Fix HUD"));
        assert_eq!(details.compactions, 1);
    }

    #[test]
    fn codex_transcript_supplies_context_usage_limits_and_tool_counts() {
        let path = temp_jsonl(
            "codex",
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"type\":\"session_meta\",\"cli_version\":\"1.2.3\",\"model_provider\":\"openai\"}}\n",
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.6-codex\",\"reasoning_effort\":\"high\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":500,\"output_tokens\":50,\"cached_input_tokens\":100,\"cache_write_input_tokens\":0,\"reasoning_output_tokens\":10},\"last_token_usage\":{\"total_tokens\":650},\"model_context_window\":200000},\"rate_limits\":{\"primary\":{\"used_percent\":25,\"window_minutes\":300,\"resets_at\":2000000000}}}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"shell_command\",\"arguments\":\"{\\\"command\\\":\\\"cargo test\\\"}\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"c1\"}}\n"
            ),
        );
        let mut details = HudDetails::default();
        parse_codex(&path, &mut details);
        let _ = fs::remove_file(path);

        assert_eq!(details.model.as_deref(), Some("gpt-5.6-codex"));
        assert_eq!(details.effort.as_deref(), Some("high"));
        assert_eq!(details.context_window, Some(200_000));
        assert_eq!(details.context_tokens, Some(650));
        assert_eq!(details.usage_windows[0].label, "5h");
        assert_eq!(details.tools[0].completed, 1);
    }

    #[test]
    fn kimi_managed_usage_supplies_five_hour_and_weekly_windows() {
        let payload = serde_json::json!({
            "usage": {
                "used": "85",
                "limit": "100",
                "resetTime": "2026-08-01T07:22:46.643965Z"
            },
            "limits": [{
                "window": {
                    "duration": 300,
                    "timeUnit": "TIME_UNIT_MINUTE"
                },
                "detail": {
                    "used": "1",
                    "limit": "100",
                    "resetTime": "2026-07-31T13:22:46.643965Z"
                }
            }]
        });

        let windows = parse_kimi_usage_windows(&payload);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].used_percent, 1.0);
        assert!(windows[0].resets_at_ms.is_some());
        assert_eq!(windows[1].label, "7d");
        assert_eq!(windows[1].used_percent, 85.0);
        assert!(windows[1].resets_at_ms.is_some());
    }

    #[test]
    fn kimi_code_index_and_wire_supply_native_session_details() {
        let root = std::env::temp_dir().join(format!("lumen-hud-kimi-code-{}", std::process::id()));
        let cwd = root.join("workspace");
        let session = root
            .join("sessions")
            .join("wd_workspace_123")
            .join("session_test");
        let main = session.join("agents").join("main");
        fs::create_dir_all(main.join("tasks")).expect("create Kimi fixture");
        let wire = main.join("wire.jsonl");
        fs::write(
            &wire,
            concat!(
                "{\"type\":\"metadata\",\"protocol_version\":3}\n",
                "{\"type\":\"config.update\",\"modelAlias\":\"kimi-code/k3\",\"thinkingEffort\":\"high\"}\n",
                "{\"type\":\"config.update\",\"profileName\":\"agent\"}\n",
                "{\"type\":\"tools.set_active_tools\",\"names\":[\"Shell\",\"ReadFile\",\"WriteFile\"]}\n",
                "{\"type\":\"permission.set_mode\",\"mode\":\"auto\"}\n",
                "{\"type\":\"turn.prompt\",\"origin\":\"tui\"}\n",
                "{\"type\":\"llm.request\",\"model\":\"k3\",\"modelAlias\":\"kimi-code/k3\",\"provider\":\"managed:kimi-code\",\"maxTokens\":1048576}\n",
                "{\"type\":\"usage.record\",\"usage\":{\"inputOther\":100,\"output\":20,\"inputCacheRead\":30,\"inputCacheCreation\":4}}\n",
                "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"step.begin\"}}\n",
                "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"content.part\",\"part\":{\"type\":\"think\",\"think\":\"fixture\"}}}\n",
                "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"tool.call\",\"toolCallId\":\"t1\",\"name\":\"Shell\",\"args\":{\"command\":\"cargo test\"}}}\n",
                "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"tool.result\",\"toolCallId\":\"t1\",\"result\":{\"output\":\"ok\"}}}\n"
            ),
        )
        .expect("write Kimi wire");
        fs::write(
            session.join("state.json"),
            "{\"title\":\"HUD fixture\",\"workDir\":\"fixture\"}",
        )
        .expect("write Kimi state");
        fs::write(
            main.join("tasks").join("task-1.json"),
            "{\"status\":\"running\"}",
        )
        .expect("write Kimi task");
        fs::write(
            root.join("session_index.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "sessionId": "session_test",
                    "sessionDir": session,
                    "workDir": cwd,
                })
            ),
        )
        .expect("write Kimi index");

        assert_eq!(
            find_kimi_code_transcript_in(&root, &cwd).as_deref(),
            Some(wire.as_path())
        );
        let mut details = HudDetails::default();
        parse_kimi(&wire, &mut details);
        assert_eq!(details.model.as_deref(), Some("kimi-code/k3"));
        assert_eq!(details.effort.as_deref(), Some("high"));
        assert_eq!(details.session_name.as_deref(), Some("HUD fixture"));
        assert_eq!(details.context_window, Some(1_048_576));
        assert_eq!(details.tokens.expect("Kimi tokens").total(), 154);
        assert_eq!(details.tools[0].completed, 1);
        let Some(CliNativeDetails::Kimi(native)) = details.native else {
            panic!("Kimi native details");
        };
        assert_eq!(native.profile.as_deref(), Some("agent"));
        assert_eq!(native.permission_mode.as_deref(), Some("auto"));
        assert_eq!(native.active_tools, 3);
        assert_eq!(
            (native.turns, native.steps, native.thinking_blocks),
            (1, 1, 1)
        );
        assert_eq!((native.background_tasks, native.running_tasks), (1, 1));

        let _ = fs::remove_dir_all(root);
    }
}
