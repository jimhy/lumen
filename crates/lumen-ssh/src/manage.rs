//! Bounded Linux process management over an SSH exec channel.
//!
//! Three fixed-shape operations are supported: killing a process by PID,
//! finding listeners of a TCP/UDP port, and searching processes by name.
//! Everything runs on short-lived exec channels (the same pattern as the
//! directory lane), never through the interactive shell, so terminal state
//! and input are untouched. User-supplied text (the process query) travels
//! as an ASCII-only octal argument exactly like directory paths do; the
//! remote shell only ever expands it into a quoted variable.

use std::fmt::Write as _;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::ChannelMsg;
use tokio::time::timeout;

use super::StrictHostKeyHandler;

pub const MAX_CONCURRENT_REQUESTS: usize = 4;
pub const MAX_QUERY_CHARS: usize = 64;
/// 名称搜索的结果上限（弹窗全量列表放宽到 [`MAX_ALL_PROCESSES`]）。
const MAX_SEARCH_RESULTS: usize = 40;
/// 详情弹窗全量进程列表的条目上限（按 CPU 降序截断）。
const MAX_ALL_PROCESSES: usize = 200;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_OUTPUT_BYTES: usize = 96 * 1024;
const MAX_RESULTS: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManageError {
    InvalidRequest,
    Busy,
    OpenFailed,
    ExecFailed,
    TimedOut,
    OutputTooLarge,
    CommandFailed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManageRequest {
    pub token: u64,
    pub action: ManageAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManageAction {
    /// SIGTERM (`force: false`) or SIGKILL (`force: true`) one process.
    Kill { pid: u32, force: bool },
    /// List the processes listening on one TCP/UDP port.
    QueryPort { port: u16 },
    /// Search processes whose command line contains `query` (case-insensitive).
    QueryProcess { query: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ManageOutcome {
    Kill(KillOutcome),
    Ports(Vec<PortEntry>),
    Processes(Vec<ProcessEntry>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KillOutcome {
    pub pid: u32,
    pub status: KillStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillStatus {
    /// The signal was delivered.
    Signalled,
    /// The process exists but the session user may not signal it.
    PermissionDenied,
    /// The process was already gone.
    NoSuchProcess,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortEntry {
    pub protocol: String,
    pub local_address: String,
    /// `None` when the remote tools hide other users' processes from us.
    pub pid: Option<u32>,
    pub command: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcessEntry {
    pub pid: u32,
    pub cpu_percent: f32,
    pub memory_percent: f32,
    pub command: String,
}

pub fn validate(request: &ManageRequest) -> Result<(), ManageError> {
    if request.token == 0 {
        return Err(ManageError::InvalidRequest);
    }
    match &request.action {
        ManageAction::Kill { pid, .. } => {
            // PID 0 would signal the whole process group of the remote shell.
            if *pid == 0 {
                return Err(ManageError::InvalidRequest);
            }
        }
        ManageAction::QueryPort { port } => {
            if *port == 0 {
                return Err(ManageError::InvalidRequest);
            }
        }
        ManageAction::QueryProcess { query } => {
            // 空 query 是合法的：表示详情弹窗的「全量进程列表」。
            if query.chars().count() > MAX_QUERY_CHARS
                || query.chars().any(char::is_control)
            {
                return Err(ManageError::InvalidRequest);
            }
        }
    }
    Ok(())
}

/// `kill` first; on failure `kill -0` distinguishes "no permission" from
/// "already gone" so the UI can say which one happened.
const KILL_SCRIPT: &str =
    "if kill -$LUMEN_SIG $LUMEN_PID 2>/dev/null; then printf 'signalled\\n'; \
elif kill -0 $LUMEN_PID 2>/dev/null; then printf 'denied\\n'; else printf 'missing\\n'; fi";

/// `ss` is universal on iproute2 systems; `netstat` is the fallback. Both
/// emit `proto<TAB>local_address<TAB>process` so the parser sees one shape.
/// Without root, process columns of other users come back empty — that is
/// reported as `pid: None`, not as an error.
const PORT_SCRIPT: &str = "if command -v ss >/dev/null 2>&1; then \
ss -H -lntup 2>/dev/null | grep -E \"[:.]${LUMEN_PORT}[[:space:]]\" | \
awk '{printf \"%s\\t%s\\t%s\\n\", $1, $5, $7}'; \
elif command -v netstat >/dev/null 2>&1; then \
netstat -tulnp 2>/dev/null | grep -E \"[:.]${LUMEN_PORT}[[:space:]]\" | \
awk '{printf \"%s\\t%s\\t%s\\n\", $1, $4, $7}'; fi";

/// The query arrives octal-encoded in `$1` (see the directory lane). An empty
/// query skips the grep stage entirely and yields the full process list (the
/// details window). The `grep -v grep` stage drops the pipeline's own
/// processes, whose command lines literally contain the word "grep".
const PROCESS_SCRIPT: &str = "q=$(printf '%bX' \"$1\"); q=${q%X}; \
if [ -n \"$q\" ]; then \
ps -eo pid=,pcpu=,pmem=,args= 2>/dev/null | grep -i -F -- \"$q\" | grep -v grep; \
else ps -eo pid=,pcpu=,pmem=,args= 2>/dev/null; fi";

pub(super) fn build_command(request: &ManageRequest) -> Result<String, ManageError> {
    validate(request)?;
    let command = match &request.action {
        ManageAction::Kill { pid, force } => {
            let signal = if *force { "KILL" } else { "TERM" };
            format!(
                "LC_ALL=C LUMEN_SIG={signal} LUMEN_PID={pid} /bin/sh -c {}",
                shell_quote(KILL_SCRIPT)
            )
        }
        ManageAction::QueryPort { port } => {
            format!(
                "LC_ALL=C LUMEN_PORT={port} /bin/sh -c {} | head -n {MAX_RESULTS}",
                shell_quote(PORT_SCRIPT)
            )
        }
        ManageAction::QueryProcess { query } => {
            let encoded = encode_octal(query.as_bytes());
            // 全量列表预取给 CPU/MEM 双维度并集截断留余量（最长命令行的
            // 极端环境由 96KB 输出上限兜底截断，parse 逐行容错）。
            let head = if query.is_empty() {
                MAX_ALL_PROCESSES * 3
            } else {
                MAX_SEARCH_RESULTS
            };
            format!(
                "LC_ALL=C /bin/sh -c {} lumen-manage '{encoded}' | head -n {head}",
                shell_quote(PROCESS_SCRIPT)
            )
        }
    };
    Ok(command)
}

fn encode_octal(input: &[u8]) -> String {
    let mut encoded = String::with_capacity(input.len().saturating_mul(4));
    for byte in input {
        let _ = write!(encoded, "\\{byte:03o}");
    }
    encoded
}

fn shell_quote(input: &str) -> String {
    let mut quoted = String::with_capacity(input.len().saturating_add(2));
    quoted.push('\'');
    for (index, part) in input.split('\'').enumerate() {
        if index > 0 {
            quoted.push_str("'\"'\"'");
        }
        quoted.push_str(part);
    }
    quoted.push('\'');
    quoted
}

/// First half of a request: validate, build the command, open the exec
/// channel and start the script. Runs inside the connection loop (borrowing
/// the session handle); the returned channel then moves into a detached
/// collection task.
pub(super) async fn open_and_exec(
    session: &client::Handle<StrictHostKeyHandler>,
    request: &ManageRequest,
    connection_cancelled: Arc<AtomicBool>,
) -> Result<russh::Channel<client::Msg>, ManageError> {
    let command = build_command(request)?;
    let open = unless_cancelled(
        timeout(REQUEST_TIMEOUT, session.channel_open_session()),
        &connection_cancelled,
    )
    .await;
    let channel = match open {
        None => return Err(ManageError::Cancelled),
        Some(Err(_)) => return Err(ManageError::TimedOut),
        Some(Ok(Err(_))) => return Err(ManageError::OpenFailed),
        Some(Ok(Ok(channel))) => channel,
    };
    let exec = unless_cancelled(
        timeout(REQUEST_TIMEOUT, channel.exec(true, command)),
        &connection_cancelled,
    )
    .await;
    match exec {
        None => Err(ManageError::Cancelled),
        Some(Err(_)) => Err(ManageError::TimedOut),
        Some(Ok(Err(_))) => Err(ManageError::ExecFailed),
        Some(Ok(Ok(()))) => Ok(channel),
    }
}

/// Second half: drain the bounded output and parse it into an outcome.
pub(super) async fn finish(
    channel: russh::Channel<client::Msg>,
    action: &ManageAction,
) -> Result<ManageOutcome, ManageError> {
    match timeout(REQUEST_TIMEOUT, collect_output(channel)).await {
        Err(_) => Err(ManageError::TimedOut),
        Ok(Err(error)) => Err(error),
        Ok(Ok(output)) => parse_outcome(action, &output),
    }
}

async fn unless_cancelled<Fut: std::future::Future>(
    future: Fut,
    cancelled: &Arc<AtomicBool>,
) -> Option<Fut::Output> {
    tokio::pin!(future);
    loop {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        match timeout(Duration::from_millis(10), &mut future).await {
            Ok(output) => return Some(output),
            Err(_) => continue,
        }
    }
}

async fn collect_output(mut channel: russh::Channel<client::Msg>) -> Result<String, ManageError> {
    let mut output = Vec::new();
    let mut exit_status = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                if output.len().saturating_add(data.len()) > MAX_OUTPUT_BYTES {
                    return Err(ManageError::OutputTooLarge);
                }
                output.extend_from_slice(&data);
            }
            ChannelMsg::ExitStatus {
                exit_status: status,
            } => exit_status = Some(status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    if exit_status.is_some_and(|status| status != 0) {
        return Err(ManageError::CommandFailed);
    }
    String::from_utf8(output).map_err(|_| ManageError::CommandFailed)
}

pub fn parse_outcome(action: &ManageAction, output: &str) -> Result<ManageOutcome, ManageError> {
    match action {
        ManageAction::Kill { pid, .. } => {
            let status = match output.lines().next().map(str::trim) {
                Some("signalled") => KillStatus::Signalled,
                Some("denied") => KillStatus::PermissionDenied,
                Some("missing") => KillStatus::NoSuchProcess,
                _ => return Err(ManageError::CommandFailed),
            };
            Ok(ManageOutcome::Kill(KillOutcome { pid: *pid, status }))
        }
        ManageAction::QueryPort { .. } => Ok(ManageOutcome::Ports(parse_port_entries(output))),
        ManageAction::QueryProcess { query } => {
            let mut entries = parse_process_entries(output);
            if query.is_empty() {
                // 全量列表（详情弹窗）：取 CPU top 与内存 top 的并集——UI 按
                // 两个维度排序截 Top10，单按 CPU 截断会漏掉低 CPU 高内存进程。
                let mut by_cpu: Vec<usize> = (0..entries.len()).collect();
                by_cpu.sort_by(|&a, &b| {
                    entries[b]
                        .cpu_percent
                        .partial_cmp(&entries[a].cpu_percent)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                let mut by_memory = by_cpu.clone();
                by_memory.sort_by(|&a, &b| {
                    entries[b]
                        .memory_percent
                        .partial_cmp(&entries[a].memory_percent)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                let mut keep = std::collections::HashSet::new();
                keep.extend(by_cpu.into_iter().take(MAX_ALL_PROCESSES));
                keep.extend(by_memory.into_iter().take(MAX_ALL_PROCESSES));
                let mut union: Vec<ProcessEntry> = keep
                    .into_iter()
                    .map(|index| entries[index].clone())
                    .collect();
                union.sort_by(|left, right| {
                    right
                        .cpu_percent
                        .partial_cmp(&left.cpu_percent)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                entries = union;
            }
            Ok(ManageOutcome::Processes(entries))
        }
    }
}

fn parse_port_entries(output: &str) -> Vec<PortEntry> {
    output
        .lines()
        .take(MAX_RESULTS)
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let protocol = fields.next()?.trim().to_owned();
            let local_address = fields.next()?.trim().to_owned();
            let raw = fields.next().unwrap_or("").trim();
            if protocol.is_empty() || local_address.is_empty() {
                return None;
            }
            let (pid, command) = parse_process_field(raw);
            Some(PortEntry {
                protocol,
                local_address,
                pid,
                command,
            })
        })
        .collect()
}

/// Both `ss` (`users:(("sshd",pid=901,fd=3))`) and `netstat` (`901/sshd`)
/// process columns land here; `-`/empty means the kernel hides the owner.
fn parse_process_field(raw: &str) -> (Option<u32>, String) {
    if raw.is_empty() || raw == "-" {
        return (None, String::new());
    }
    if let Some(position) = raw.find("pid=") {
        let pid = raw[position + 4..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok();
        let command = raw
            .find("((\"")
            .and_then(|start| {
                let rest = &raw[start + 3..];
                rest.find('"').map(|end| rest[..end].to_owned())
            })
            .unwrap_or_default();
        return (pid, command);
    }
    if let Some((pid_text, command)) = raw.split_once('/') {
        if let Ok(pid) = pid_text.parse::<u32>() {
            return (Some(pid), command.trim().to_owned());
        }
    }
    (None, raw.to_owned())
}

fn parse_process_entries(output: &str) -> Vec<ProcessEntry> {
    output
        .lines()
        .take(MAX_ALL_PROCESSES * 3)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let cpu_percent = fields.next()?.parse::<f32>().ok()?;
            let memory_percent = fields.next()?.parse::<f32>().ok()?;
            let command = fields.collect::<Vec<_>>().join(" ");
            if command.is_empty() {
                return None;
            }
            Some(ProcessEntry {
                pid,
                cpu_percent,
                memory_percent,
                command: command.chars().take(160).collect(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(action: ManageAction) -> ManageRequest {
        ManageRequest { token: 1, action }
    }

    #[test]
    fn kill_rejects_pid_zero() {
        let kill = request(ManageAction::Kill {
            pid: 0,
            force: false,
        });
        assert_eq!(validate(&kill), Err(ManageError::InvalidRequest));
    }

    #[test]
    fn port_rejects_zero() {
        let query = request(ManageAction::QueryPort { port: 0 });
        assert_eq!(validate(&query), Err(ManageError::InvalidRequest));
    }

    #[test]
    fn process_query_rejects_control_and_long_but_allows_empty() {
        // 空串 = 详情弹窗的全量进程列表，合法。
        let all = request(ManageAction::QueryProcess {
            query: String::new(),
        });
        assert_eq!(validate(&all), Ok(()));
        for query in ["bad\nname", &"x".repeat(MAX_QUERY_CHARS + 1)] {
            let search = request(ManageAction::QueryProcess {
                query: query.to_owned(),
            });
            assert_eq!(validate(&search), Err(ManageError::InvalidRequest));
        }
    }

    #[test]
    fn kill_command_marks_signal_and_pid_without_shell_words() {
        let term = build_command(&request(ManageAction::Kill {
            pid: 42,
            force: false,
        }))
        .unwrap();
        let kill = build_command(&request(ManageAction::Kill {
            pid: 42,
            force: true,
        }))
        .unwrap();
        assert!(term.contains("LUMEN_SIG=TERM"));
        assert!(kill.contains("LUMEN_SIG=KILL"));
        assert!(term.contains("LUMEN_PID=42"));
        assert!(!term.contains("sudo"));
    }

    #[test]
    fn process_query_is_octal_encoded() {
        let command = build_command(&request(ManageAction::QueryProcess {
            query: "ng';nix $(rm -rf /)".to_owned(),
        }))
        .unwrap();
        // 查询只能以八进制转义形式出现，原文不得进入命令行。
        assert!(!command.contains("nginx $(rm -rf /)"));
        assert!(command.contains("\\156\\147"));
    }

    #[test]
    fn kill_outcome_parses_all_statuses() {
        let action = ManageAction::Kill {
            pid: 7,
            force: false,
        };
        assert_eq!(
            parse_outcome(&action, "signalled\n"),
            Ok(ManageOutcome::Kill(KillOutcome {
                pid: 7,
                status: KillStatus::Signalled
            }))
        );
        assert_eq!(
            parse_outcome(&action, "denied\n"),
            Ok(ManageOutcome::Kill(KillOutcome {
                pid: 7,
                status: KillStatus::PermissionDenied
            }))
        );
        assert_eq!(
            parse_outcome(&action, "missing\n"),
            Ok(ManageOutcome::Kill(KillOutcome {
                pid: 7,
                status: KillStatus::NoSuchProcess
            }))
        );
        assert!(parse_outcome(&action, "garbage").is_err());
    }

    #[test]
    fn port_entries_parse_ss_and_netstat_rows() {
        let ss_row = "tcp\t0.0.0.0:22\tusers:((\"sshd\",pid=901,fd=3))\n";
        let netstat_row = "udp\t127.0.0.53:53\t412/systemd-resolve\n";
        let hidden_row = "tcp\t[::]:8080\t-\n";
        let action = ManageAction::QueryPort { port: 22 };
        let outcome =
            parse_outcome(&action, &format!("{ss_row}{netstat_row}{hidden_row}")).unwrap();
        let ManageOutcome::Ports(entries) = outcome else {
            panic!("port query must yield port entries");
        };
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].pid, Some(901));
        assert_eq!(entries[0].command, "sshd");
        assert_eq!(entries[0].local_address, "0.0.0.0:22");
        assert_eq!(entries[1].pid, Some(412));
        assert_eq!(entries[1].command, "systemd-resolve");
        assert_eq!(entries[2].pid, None);
        assert_eq!(entries[2].command, "");
    }

    #[test]
    fn process_entries_parse_ps_rows() {
        let output = "  901 0.3 1.2 /usr/sbin/sshd -D\n  42 150.0 12.5 worker --flag=a,b\n";
        let action = ManageAction::QueryProcess {
            query: "sshd".to_owned(),
        };
        let outcome = parse_outcome(&action, output).unwrap();
        let ManageOutcome::Processes(entries) = outcome else {
            panic!("process query must yield process entries");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].pid, 901);
        assert_eq!(entries[0].command, "/usr/sbin/sshd -D");
        assert_eq!(entries[1].cpu_percent, 150.0);
        assert_eq!(entries[1].command, "worker --flag=a,b");
    }
}
