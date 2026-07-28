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
/// 详情弹窗全量进程列表的条目上限（CPU/MEM top 并集各取此数）。
const MAX_ALL_PROCESSES: usize = 200;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);
/// 600 行进程（含长命令行）+ 300 行监听 socket 的预算，留一倍余量。
const MAX_OUTPUT_BYTES: usize = 192 * 1024;
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
    /// 父进程 PID（任务管理器式父子分组；孤儿进程与 pid 1 的 ppid 为 0）。
    pub ppid: u32,
    pub cpu_percent: f32,
    pub memory_percent: f32,
    pub command: String,
    /// 该进程监听的 TCP/UDP 端口（升序去重；无权限/无监听时为空）。
    pub ports: Vec<u16>,
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

/// `ss` 优先（不用 `-H`，兼容老 iproute2），`netstat` 兜底，BusyBox
/// `netstat` 不支持 `-p` 时二次兜底为无进程列。输出统一为
/// `proto<TAB>local_address<TAB>process`。末尾 sentinel 让客户端区分
/// 「执行成功但无匹配」与「工具链失败」——此前所有失败都静默映射成
/// 「未发现监听」，用户无法分辨（海风哥：Linux 端搜端口无效）。
const PORT_SCRIPT: &str = "if command -v ss >/dev/null 2>&1; then \
ss -lntup 2>/dev/null | awk -v p=\"${LUMEN_PORT}\" \
'$0 ~ (\"[:.]\" p \"[[:space:]]\") { printf \"%s\\t%s\\t%s\\n\", $1, $5, (NF >= 7 ? $7 : \"-\") }'; \
elif command -v netstat >/dev/null 2>&1; then \
out=$(netstat -tulnp 2>/dev/null) || out=$(netstat -tuln 2>/dev/null); \
printf '%s\\n' \"$out\" | awk -v p=\"${LUMEN_PORT}\" \
'$0 ~ (\"[:.]\" p \"[[:space:]]\") { printf \"%s\\t%s\\t%s\\n\", $1, $4, (NF >= 7 ? $7 : \"-\") }'; \
else printf 'LUMEN_NO_TOOL\\n'; fi; \
printf 'LUMEN_END\\n'";

/// The query arrives octal-encoded in `$1` (see the directory lane). An empty
/// query skips the grep stage entirely and yields the full process list (the
/// details window). The `grep -v grep` stage drops the pipeline's own
/// processes, whose command lines literally contain the word "grep".
/// 进程字段含 ppid（任务管理器式父子分组）；`===LUMEN_SS===` 段附全量
/// 监听 socket（详情弹窗「端口号」列的数据源，无 ss 的系统该段为空）。
/// 两段各自独立限行——此前整段统一 head，进程行一多 ss 段就被挤出配额
/// 之外，端口列大面积显示「-」（海风哥 docker 服务器实测复现）。
const PROCESS_SCRIPT: &str = "q=$(printf '%bX' \"$1\"); q=${q%X}; \
echo '===LUMEN_PS==='; \
if [ -n \"$q\" ]; then \
ps -eo pid=,ppid=,pcpu=,pmem=,args= 2>/dev/null | grep -i -F -- \"$q\" | grep -v grep | head -n 40; \
else ps -eo pid=,ppid=,pcpu=,pmem=,args= 2>/dev/null | head -n 600; fi; \
echo '===LUMEN_SS==='; \
ss -lntup 2>/dev/null | head -n 300";

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
            // 行数配额已移入脚本（ps/ss 两段各自限行，见 PROCESS_SCRIPT 注释）。
            format!(
                "LC_ALL=C /bin/sh -c {} lumen-manage '{encoded}'",
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
    // 输出在字节上限处可能正好切断多字节 UTF-8 序列（长命令行环境），
    // 容错解码替换半个字符，绝不让一行坏数据灭掉整批结果。
    Ok(String::from_utf8_lossy(&output).into_owned())
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
        ManageAction::QueryPort { .. } => {
            // sentinel 缺席 = 工具链失败（ss/netstat 全缺或脚本未执行完），
            // 与「执行成功但无匹配」严格区分。
            if output.lines().any(|line| line.trim() == "LUMEN_NO_TOOL")
                || !output.lines().any(|line| line.trim() == "LUMEN_END")
            {
                return Err(ManageError::CommandFailed);
            }
            Ok(ManageOutcome::Ports(parse_port_entries(output)))
        }
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
    if raw.contains("pid=") {
        let command = raw
            .find("((\"")
            .and_then(|start| {
                let rest = &raw[start + 3..];
                rest.find('"').map(|end| rest[..end].to_owned())
            })
            .unwrap_or_default();
        return (extract_pid(raw), command);
    }
    if let Some((pid_text, command)) = raw.split_once('/') {
        if let Ok(pid) = pid_text.parse::<u32>() {
            return (Some(pid), command.trim().to_owned());
        }
    }
    (None, raw.to_owned())
}

/// 解析 `===LUMEN_PS===` / `===LUMEN_SS===` 两段输出：ps 段（含 ppid）
/// 逐行容错；ss 段聚合成 `pid → [监听端口]` 后并入各进程条目（无 ss /
/// 无权限时对应进程端口列自然为空）。
fn parse_process_entries(output: &str) -> Vec<ProcessEntry> {
    let ps_body = output
        .split("===LUMEN_SS===")
        .next()
        .unwrap_or("")
        .split("===LUMEN_PS===")
        .nth(1)
        .unwrap_or("");
    let ss_body = output.split("===LUMEN_SS===").nth(1).unwrap_or("");

    let mut listening: std::collections::HashMap<u32, Vec<u16>> = std::collections::HashMap::new();
    for line in ss_body.lines().take(300) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let local = fields[4];
        let Some(port) = local.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) else {
            continue;
        };
        let Some(pid) = fields.get(6).and_then(|raw| extract_pid(raw)) else {
            continue;
        };
        let ports = listening.entry(pid).or_default();
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    for ports in listening.values_mut() {
        ports.sort_unstable();
    }

    ps_body
        .lines()
        .take(MAX_ALL_PROCESSES * 3)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let ppid = fields.next()?.parse::<u32>().ok()?;
            let cpu_percent = fields.next()?.parse::<f32>().ok()?;
            let memory_percent = fields.next()?.parse::<f32>().ok()?;
            let command = fields.collect::<Vec<_>>().join(" ");
            if command.is_empty() {
                return None;
            }
            let ports = listening.get(&pid).cloned().unwrap_or_default();
            Some(ProcessEntry {
                pid,
                ppid,
                cpu_percent,
                memory_percent,
                command: command.chars().take(160).collect(),
                ports,
            })
        })
        .collect()
}

/// 从 `users:(("sshd",pid=901,fd=3))` 或 `901/sshd` 提取 PID。
fn extract_pid(raw: &str) -> Option<u32> {
    if let Some(position) = raw.find("pid=") {
        return raw[position + 4..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok();
    }
    raw.split('/')
        .next()
        .and_then(|text| text.parse::<u32>().ok())
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
        let outcome = parse_outcome(
            &action,
            &format!("{ss_row}{netstat_row}{hidden_row}LUMEN_END\n"),
        )
        .unwrap();
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
    fn port_query_missing_sentinel_is_failure_not_empty() {
        let action = ManageAction::QueryPort { port: 22 };
        // 无 LUMEN_END（工具链失败/脚本未执行完）必须报错，不能伪装成
        // 「未发现监听该端口的进程」。
        assert!(parse_outcome(&action, "").is_err());
        assert!(parse_outcome(&action, "LUMEN_NO_TOOL\nLUMEN_END\n").is_err());
        let ok = parse_outcome(&action, "LUMEN_END\n").unwrap();
        let ManageOutcome::Ports(entries) = ok else {
            panic!("port query must yield port entries");
        };
        assert!(entries.is_empty());
    }

    #[test]
    fn process_entries_parse_ps_rows_with_ppid_and_ports() {
        let output = "===LUMEN_PS===\n  901 1 0.3 1.2 /usr/sbin/sshd -D\n  42 901 150.0 12.5 worker --flag=a,b\n===LUMEN_SS===\nNetid State Recv-Q Send-Q Local Address:Port Peer Address:Port Process\ntcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=901,fd=3))\ntcp LISTEN 0 511 0.0.0.0:443 0.0.0.0:*\ntcp LISTEN 0 100 [::]:9090 [::]:* users:((\"worker\",pid=42,fd=7))\n";
        let action = ManageAction::QueryProcess {
            query: "sshd".to_owned(),
        };
        let outcome = parse_outcome(&action, output).unwrap();
        let ManageOutcome::Processes(entries) = outcome else {
            panic!("process query must yield process entries");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].pid, 901);
        assert_eq!(entries[0].ppid, 1);
        assert_eq!(entries[0].command, "/usr/sbin/sshd -D");
        assert_eq!(entries[0].ports, vec![22]);
        assert_eq!(entries[1].ppid, 901);
        assert_eq!(entries[1].cpu_percent, 150.0);
        assert_eq!(entries[1].command, "worker --flag=a,b");
        assert_eq!(entries[1].ports, vec![9090]);
    }
}
