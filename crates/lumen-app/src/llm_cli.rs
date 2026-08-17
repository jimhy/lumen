//! 通用 LLM CLI 识别与原生命令菜单解析。
//!
//! Lumen 只识别“当前前台程序是否为受支持的 LLM CLI”，不推断 CLI
//! 正在思考、输出或等待输入。识别成功后，footer 编辑器始终可用。

use std::path::Path;
use std::time::Instant;

use lumen_term::{CellFlags, Terminal};

/// Lumen 当前内置适配的主流 LLM CLI。
///
/// `Hash` 是给会话图标缓存当键用的（见 `crate::llm_icon`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LlmCliKind {
    Claude,
    Codex,
    Gemini,
    Kimi,
}

impl LlmCliKind {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
            Self::Gemini => "Gemini CLI",
            Self::Kimi => "Kimi CLI",
        }
    }
}

/// 根据前台可执行文件、OSC 标题和可视终端内容识别 LLM CLI。
///
/// Node/Python 包装的 CLI 在进程表里经常只显示为 `node`/`python`，因此
/// 标题和首屏品牌文本也是一等识别信号。
pub fn detect(exe: Option<&Path>, term: &Terminal) -> Option<LlmCliKind> {
    let exe_name = exe
        .and_then(Path::file_stem)
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if let Some(kind) = detect_text(&exe_name) {
        return Some(kind);
    }

    if let Some(command) = term
        .blocks()
        .last()
        .filter(|block| !block.is_closed())
        .and_then(|block| block.cmd_text.as_deref())
    {
        if let Some(kind) = detect_command_line(command) {
            return Some(kind);
        }
    }

    // 前台已明确回到 shell，说明 CLI 已退出。此时不要被仍留在可视区
    // 的品牌文字误判为正在运行。
    if matches!(
        exe_name.as_str(),
        "pwsh" | "powershell" | "cmd" | "bash" | "zsh" | "fish" | "nu"
    ) {
        return None;
    }

    let title = term.title().to_ascii_lowercase();
    if let Some(kind) = detect_text(&title) {
        return Some(kind);
    }

    let screen = visible_text(term).to_ascii_lowercase();
    detect_text(&screen)
}

fn detect_command_line(command: &str) -> Option<LlmCliKind> {
    for token in command.split_whitespace() {
        let token = token
            .trim_matches(['"', '\'', '(', ')'])
            .replace('\\', "/")
            .to_ascii_lowercase();
        let name = token.rsplit('/').next().unwrap_or(&token);
        let name = name
            .strip_suffix(".exe")
            .or_else(|| name.strip_suffix(".cmd"))
            .or_else(|| name.strip_suffix(".ps1"))
            .unwrap_or(name);
        match name {
            "claude" | "claude-code" | "@anthropic-ai/claude-code" => {
                return Some(LlmCliKind::Claude);
            }
            "codex" | "@openai/codex" => return Some(LlmCliKind::Codex),
            "gemini" | "gemini-cli" | "@google/gemini-cli" => {
                return Some(LlmCliKind::Gemini);
            }
            "kimi" | "kimi-cli" => return Some(LlmCliKind::Kimi),
            _ => {}
        }
    }
    None
}

fn detect_text(text: &str) -> Option<LlmCliKind> {
    if text.contains("claude code")
        || text == "claude"
        || text.ends_with("\\claude")
        || text.ends_with("/claude")
    {
        return Some(LlmCliKind::Claude);
    }
    if text.contains("codex cli")
        || text == "codex"
        || text.ends_with("\\codex")
        || text.ends_with("/codex")
    {
        return Some(LlmCliKind::Codex);
    }
    if text.contains("gemini cli")
        || text == "gemini"
        || text.ends_with("\\gemini")
        || text.ends_with("/gemini")
    {
        return Some(LlmCliKind::Gemini);
    }
    if text.contains("kimi cli")
        || text.contains("kimi code")
        || text == "kimi"
        || text.ends_with("\\kimi")
        || text.ends_with("/kimi")
    {
        return Some(LlmCliKind::Kimi);
    }
    None
}

/// 斜杠命令候选。内容来自 CLI 自己画在终端里的菜单，而不是 Lumen
/// 内置命令表，因此安装插件或 CLI 升级后无需同步维护。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub command: String,
    pub description: String,
}

/// CLI 当前画出的斜杠菜单快照。命令列表只包含可见页；扫描器通过
/// `selected_command` 或 `position` 驱动菜单逐项滚动并累计所有页。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashMenuSnapshot {
    pub commands: Vec<SlashCommand>,
    pub selected_command: Option<String>,
    pub position: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashScanDecision {
    Advance,
    Finish,
}

/// 为唤起 CLI 原生命令菜单而临时镜像进 PTY 的斜杠前缀。
#[derive(Debug, Default)]
pub struct SlashProbeState {
    /// 当前临时写入 CLI 的前缀；非空期间终端纹理保持上一稳定帧，
    /// 防止原生菜单与 Lumen 弹层同时出现。
    pub shadow: String,
    /// 已发送 Ctrl+U，正在等待 CLI 擦除原生菜单。
    pub clearing: bool,
    /// Kimi 的 Ctrl+U 只清输入，收到输入已清空的终端帧后再单独发送
    /// Esc 关闭菜单，避免两个控制键被 ConPTY 合并为一次输入。
    pub escape_sent: bool,
    /// 下一阶段清理动作的最早执行时刻。清理不能只靠 PTY 新输出推进：
    /// 有些 CLI 处理 Ctrl+U/Esc 后不会再画一帧，若没有定时兜底会让
    /// `shadow` 永久非空并冻结终端纹理。
    pub clear_at: Option<Instant>,
    /// 清理结束后是否按编辑器的最新前缀恢复缓存候选/重新探测。
    /// 无结果探测必须为 false，否则同一前缀会无限探测。
    pub resume_after_clear: bool,
    /// 等待 CLI 异步生成斜杠候选的截止时间。不能按 PTY 更新次数取消：
    /// Kimi 会先产生多次局部重绘，再异步返回真正的筛选结果。
    pub probe_deadline: Option<Instant>,
    /// 自动遍历原生命令菜单的下一次下移时刻与硬截止时间。
    pub scan_at: Option<Instant>,
    pub scan_deadline: Option<Instant>,
    /// 扫描停止条件：总步数兜底、连续无新增页计数、选中项回环。
    pub scan_steps: u16,
    pub scan_stagnant_steps: u16,
    pub scan_last_command_count: usize,
    pub scan_origin_selected: Option<String>,
    pub scan_saw_other_selected: bool,
    /// 不依赖高亮字符的翻页判定：Codex 等菜单仅以背景色标记选中项，
    /// 但滚动一圈后可见命令页一定会回到首屏。
    pub scan_origin_page: Vec<String>,
    pub scan_saw_other_page: bool,
    /// 本会话从 CLI 原生菜单累计发现的命令。影子输入清除后仍保留，
    /// 后续前缀优先在本地缓存中筛选。
    pub commands: Vec<SlashCommand>,
    /// 已完成全量遍历的前缀。较短前缀覆盖其所有更长前缀：
    /// 完整扫描 `/` 后，`/re` 可直接从完整缓存筛选。
    pub complete_prefixes: Vec<String>,
}

impl SlashProbeState {
    pub fn clear_active(&mut self) {
        self.shadow.clear();
        self.clearing = false;
        self.escape_sent = false;
        self.clear_at = None;
        self.resume_after_clear = false;
        self.probe_deadline = None;
        self.stop_scan();
    }

    pub fn clear(&mut self) {
        self.clear_active();
        self.commands.clear();
        self.complete_prefixes.clear();
    }

    pub fn merge_commands(&mut self, commands: Vec<SlashCommand>) -> bool {
        let mut changed = false;
        for command in commands {
            if let Some(existing) = self
                .commands
                .iter_mut()
                .find(|existing| existing.command == command.command)
            {
                if *existing != command {
                    *existing = command;
                    changed = true;
                }
            } else {
                self.commands.push(command);
                changed = true;
            }
        }
        self.commands
            .sort_by(|left, right| left.command.cmp(&right.command));
        changed
    }

    pub fn begin_probe(&mut self, prefix: String, deadline: Instant) {
        self.shadow = prefix;
        self.probe_deadline = Some(deadline);
        self.stop_scan();
    }

    pub fn probe_timed_out(&self, now: Instant) -> bool {
        !self.clearing
            && !self.shadow.is_empty()
            && self.probe_deadline.is_some_and(|deadline| now >= deadline)
    }

    pub fn scanning(&self) -> bool {
        self.scan_at.is_some()
    }

    pub fn begin_scan(
        &mut self,
        snapshot: &SlashMenuSnapshot,
        command_count: usize,
        next_at: Instant,
        deadline: Instant,
    ) {
        self.probe_deadline = None;
        self.scan_at = Some(next_at);
        self.scan_deadline = Some(deadline);
        self.scan_steps = 0;
        self.scan_stagnant_steps = 0;
        self.scan_last_command_count = command_count;
        self.scan_origin_selected = snapshot.selected_command.clone();
        self.scan_saw_other_selected = false;
        self.scan_origin_page = snapshot
            .commands
            .iter()
            .map(|item| item.command.clone())
            .collect();
        self.scan_saw_other_page = false;
    }

    pub fn observe_scan(
        &mut self,
        snapshot: &SlashMenuSnapshot,
        command_count: usize,
        now: Instant,
        max_steps: u16,
        max_stagnant_steps: u16,
    ) -> SlashScanDecision {
        if command_count > self.scan_last_command_count {
            self.scan_stagnant_steps = 0;
        } else {
            self.scan_stagnant_steps = self.scan_stagnant_steps.saturating_add(1);
        }
        self.scan_last_command_count = command_count;

        let visible_page = snapshot
            .commands
            .iter()
            .map(|item| item.command.as_str())
            .collect::<Vec<_>>();
        if !visible_page.is_empty() {
            let is_origin_page = visible_page
                .iter()
                .copied()
                .eq(self.scan_origin_page.iter().map(String::as_str));
            if is_origin_page && self.scan_saw_other_page {
                return SlashScanDecision::Finish;
            }
            if !is_origin_page {
                self.scan_saw_other_page = true;
            }
        }

        if let Some(selected) = snapshot.selected_command.as_deref() {
            match self.scan_origin_selected.as_deref() {
                None => self.scan_origin_selected = Some(selected.to_owned()),
                Some(origin) if selected != origin => self.scan_saw_other_selected = true,
                Some(_) if self.scan_saw_other_selected => return SlashScanDecision::Finish,
                Some(_) => {}
            }
        }

        // 如果菜单本身小于一屏，可见页不会变化；走过“首屏条数+少量余量”
        // 后即可确认已回环。全局上限仍防御异常超大菜单。
        let page_stagnant_limit = u16::try_from(self.scan_origin_page.len().saturating_add(4))
            .unwrap_or(u16::MAX)
            .max(8)
            .min(max_stagnant_steps);
        if snapshot
            .position
            .is_some_and(|(selected, total)| selected >= total)
            || self.scan_deadline.is_some_and(|deadline| now >= deadline)
            || self.scan_steps >= max_steps
            || self.scan_stagnant_steps >= page_stagnant_limit
        {
            return SlashScanDecision::Finish;
        }

        self.scan_steps = self.scan_steps.saturating_add(1);
        SlashScanDecision::Advance
    }

    pub fn schedule_scan(&mut self, next_at: Instant) {
        self.scan_at = Some(next_at);
    }

    pub fn stop_scan(&mut self) {
        self.scan_at = None;
        self.scan_deadline = None;
        self.scan_steps = 0;
        self.scan_stagnant_steps = 0;
        self.scan_last_command_count = 0;
        self.scan_origin_selected = None;
        self.scan_saw_other_selected = false;
        self.scan_origin_page.clear();
        self.scan_saw_other_page = false;
    }

    pub fn mark_prefix_complete(&mut self, prefix: String) {
        if !self
            .complete_prefixes
            .iter()
            .any(|complete| prefix.starts_with(complete))
        {
            self.complete_prefixes.push(prefix);
            self.complete_prefixes.sort();
        }
    }

    pub fn prefix_complete(&self, prefix: &str) -> bool {
        self.complete_prefixes
            .iter()
            .any(|complete| prefix.starts_with(complete))
    }
}

/// CLI 的主聊天编辑器是否已经可用。
///
/// Kimi 在主 TUI 前可能显示更新、迁移等原生选择器。此时仅凭进程名
/// 强制切到 Lumen 编辑器会吞掉选择器按键，所以必须等它的 `│ > ... │`
/// 编辑行实际出现。其他 CLI 暂沿用既有识别行为。
pub fn composer_ready(term: &Terminal, kind: LlmCliKind) -> bool {
    kind != LlmCliKind::Kimi || kimi_editor_visible(visible_rows(term).iter().map(String::as_str))
}

/// 从终端当前可视网格中提取原生斜杠菜单快照。
pub fn slash_menu(term: &Terminal, prefix: &str, kind: LlmCliKind) -> SlashMenuSnapshot {
    let rows = visible_rows(term);
    let row_refs = || rows.iter().map(String::as_str);
    let commands = match kind {
        LlmCliKind::Kimi => parse_kimi_slash_commands(row_refs(), prefix),
        _ => parse_slash_commands(row_refs(), prefix),
    };
    let selected_command = match kind {
        LlmCliKind::Kimi => kimi_selected_command(row_refs(), prefix),
        _ => selected_slash_command(row_refs(), prefix),
    };
    // Kimi、Gemini 等 TUI 都会在候选末尾画 `(当前位置/总数)`。
    // 取屏幕上最后一个，避免聊天历史里偶然出现的同形文本抢占菜单页码。
    let position = row_refs().filter_map(slash_menu_position).next_back();
    SlashMenuSnapshot {
        commands,
        selected_command,
        position,
    }
}

/// Kimi 的菜单固定围绕选中项显示一个可见窗口。一次批量移动一个
/// 可见页，并在最后一批按 `(当前位置/总数)` 截断，既减少重绘也不越过末项。
pub fn slash_scan_advance_steps(kind: LlmCliKind, snapshot: &SlashMenuSnapshot) -> usize {
    if kind != LlmCliKind::Kimi {
        return 1;
    }
    let page_size = snapshot.commands.len().max(1);
    snapshot.position.map_or(1, |(selected, total)| {
        page_size.min(total.saturating_sub(selected)).max(1)
    })
}

fn visible_rows(term: &Terminal) -> Vec<String> {
    term.grid()
        .visible_rows()
        .map(|row| {
            row.cells()
                .iter()
                .filter(|cell| !cell.flags.contains(CellFlags::WIDE_SPACER))
                .map(|cell| cell.ch)
                .collect::<String>()
        })
        .collect()
}

fn visible_text(term: &Terminal) -> String {
    visible_rows(term).join("\n")
}

fn parse_slash_commands<'a>(
    rows: impl IntoIterator<Item = &'a str>,
    prefix: &str,
) -> Vec<SlashCommand> {
    let mut commands = Vec::new();
    for row in rows {
        let line = row
            .trim()
            .trim_start_matches(|ch: char| {
                matches!(
                    ch,
                    '>' | '❯'
                        | '›'
                        | '●'
                        | '•'
                        | '◆'
                        | '○'
                        | '│'
                        | '┃'
                        | '┆'
                        | '┊'
                        | '╭'
                        | '╰'
                        | '├'
                        | '└'
                        | '─'
                        | ' '
                )
            })
            .trim_start();
        let Some(token) = line.split_whitespace().next() else {
            continue;
        };
        if token.len() <= 1
            || !token.starts_with('/')
            || !token[1..]
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '?' | '.'))
            || !token.starts_with(prefix)
        {
            continue;
        }
        let description = line[token.len()..]
            .trim_start_matches([' ', '\t', '-', '—'])
            .trim();
        // CLI 输入行自身也会出现在网格里；无描述且恰等于探测前缀时
        // 不是菜单候选。
        if token == prefix && description.is_empty() {
            continue;
        }
        if commands
            .iter()
            .any(|candidate: &SlashCommand| candidate.command == token)
        {
            continue;
        }
        commands.push(SlashCommand {
            command: token.to_owned(),
            description: description.to_owned(),
        });
    }
    commands
}

/// Kimi Code 0.29+ 的菜单标签不带 `/`，并画在一个独立的 `│ ... │`
/// 列表内。只解析分页行之前、编辑器下边框之后的菜单区域，避免把欢迎
/// 面板、状态栏或换行后的描述误当成命令。
fn parse_kimi_slash_commands<'a>(
    rows: impl IntoIterator<Item = &'a str>,
    prefix: &str,
) -> Vec<SlashCommand> {
    let rows = rows.into_iter().collect::<Vec<_>>();
    let Some(selected_row) = rows.iter().rposition(|row| kimi_selected_menu_row(row)) else {
        return Vec::new();
    };
    let Some(menu_start) = rows[..selected_row]
        .iter()
        .rposition(|row| row.trim().starts_with('╰'))
        .map(|index| index + 1)
    else {
        return Vec::new();
    };
    let menu_end = rows[selected_row..]
        .iter()
        .position(|row| {
            let line = row.trim();
            slash_menu_position(line).is_some() || !(line.starts_with('│') && line.ends_with('│'))
        })
        .map_or(rows.len(), |offset| selected_row + offset);

    let mut commands = Vec::new();
    for row in &rows[menu_start..menu_end] {
        let line = row.trim();
        let Some(inner) = line
            .strip_prefix('│')
            .and_then(|line| line.strip_suffix('│'))
        else {
            continue;
        };
        let indent = inner.chars().take_while(|ch| ch.is_whitespace()).count();
        let content = inner.trim_start();
        let (selected, content) = content
            .strip_prefix('→')
            .map_or((false, content), |rest| (true, rest.trim_start()));
        // 普通命令位于固定的窄标签列；描述换行的缩进远大于它。
        if !selected && indent > 8 {
            continue;
        }
        let Some(token) = content.split_whitespace().next() else {
            continue;
        };
        if token.is_empty()
            || !token
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '?' | '.'))
        {
            continue;
        }
        let command = format!("/{token}");
        if !command.starts_with(prefix)
            || commands
                .iter()
                .any(|candidate: &SlashCommand| candidate.command == command)
        {
            continue;
        }
        let description = content[token.len()..].trim();
        commands.push(SlashCommand {
            command,
            description: description.to_owned(),
        });
    }
    commands
}

fn kimi_selected_menu_row(row: &str) -> bool {
    row.trim()
        .strip_prefix('│')
        .and_then(|line| line.strip_suffix('│'))
        .is_some_and(|inner| {
            inner
                .trim_start()
                .strip_prefix('→')
                .is_some_and(|rest| !rest.trim_start().is_empty())
        })
}

fn kimi_selected_command<'a>(
    rows: impl IntoIterator<Item = &'a str>,
    prefix: &str,
) -> Option<String> {
    let rows = rows.into_iter().collect::<Vec<_>>();
    rows.into_iter().rev().find_map(|row| {
        let inner = row
            .trim()
            .strip_prefix('│')?
            .strip_suffix('│')?
            .trim_start()
            .strip_prefix('→')?
            .trim_start();
        let token = inner.split_whitespace().next()?;
        let command = format!("/{token}");
        (command.len() > 1 && command.starts_with(prefix)).then_some(command)
    })
}

fn selected_slash_command<'a>(
    rows: impl IntoIterator<Item = &'a str>,
    prefix: &str,
) -> Option<String> {
    let rows = rows.into_iter().collect::<Vec<_>>();
    rows.into_iter().rev().find_map(|row| {
        let line = row
            .trim()
            .trim_start_matches(['│', '┃', '┆', '┊'])
            .trim_start();
        let marker = line.chars().next()?;
        if !matches!(marker, '>' | '❯' | '›' | '→') {
            return None;
        }
        let content = line[marker.len_utf8()..].trim_start();
        let token = content.split_whitespace().next()?;
        (token.len() > 1 && token.starts_with('/') && token != prefix && token.starts_with(prefix))
            .then(|| token.to_owned())
    })
}

fn slash_menu_position(row: &str) -> Option<(usize, usize)> {
    row.split_whitespace().find_map(|part| {
        let part = part.trim_matches(['│', '(', ')']);
        let (selected, total) = part.split_once('/')?;
        let selected = selected.parse().ok()?;
        let total = total.parse().ok()?;
        (selected > 0 && total >= selected).then_some((selected, total))
    })
}

fn kimi_editor_visible<'a>(rows: impl IntoIterator<Item = &'a str>) -> bool {
    rows.into_iter().any(|row| {
        row.trim()
            .strip_prefix('│')
            .and_then(|line| line.strip_suffix('│'))
            .is_some_and(|inner| inner.trim_start().starts_with('>'))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashClearStage {
    SendEscape,
    Finish,
}

/// 定时清理的下一阶段。Kimi 需要 Ctrl+U 后再单独发 Esc；其他 CLI
/// 的 Ctrl+U 阶段完成后即可结束。
pub fn slash_clear_stage(kind: Option<LlmCliKind>, escape_sent: bool) -> SlashClearStage {
    if kind == Some(LlmCliKind::Kimi) && !escape_sent {
        SlashClearStage::SendEscape
    } else {
        SlashClearStage::Finish
    }
}

/// 仅当整个草稿是“正在输入中的单行斜杠 token”时返回它。
pub fn slash_prefix(text: &str) -> Option<&str> {
    if text.starts_with('/')
        && !text.chars().any(char::is_whitespace)
        && text[1..]
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '?' | '.'))
    {
        Some(text)
    } else {
        None
    }
}

/// 活动探测（扫描或收尾清理）的前缀比本地新前缀更宽时，可继续当前
/// 流程并仅在本地过滤。例如 PTY 保持裸 `/`，本地输入到 `/re` 时
/// 无需重启 CLI 菜单。
pub fn can_filter_active_probe(active: bool, shadow: &str, next: Option<&str>) -> bool {
    active && !shadow.is_empty() && next.is_some_and(|prefix| prefix.starts_with(shadow))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_native_menu_rows_and_deduplicates() {
        let rows = [
            "  ❯ /compact   Compact conversation",
            "    /config — Open settings",
            "    /compact duplicate",
            "not-a-command",
        ];
        assert_eq!(
            parse_slash_commands(rows, "/co"),
            vec![
                SlashCommand {
                    command: "/compact".into(),
                    description: "Compact conversation".into(),
                },
                SlashCommand {
                    command: "/config".into(),
                    description: "Open settings".into(),
                },
            ]
        );
    }

    #[test]
    fn slash_prefix_accepts_bare_slash_and_rejects_sentences() {
        assert_eq!(slash_prefix("/comp"), Some("/comp"));
        assert_eq!(slash_prefix("/"), Some("/"));
        assert_eq!(slash_prefix("/compact now"), None);
        assert_eq!(slash_prefix("please /compact"), None);
    }

    #[test]
    fn active_root_scan_accepts_local_narrowing_without_reprobe() {
        assert!(can_filter_active_probe(true, "/", Some("/y")));
        assert!(can_filter_active_probe(true, "/", Some("/resume")));
        assert!(can_filter_active_probe(true, "/res", Some("/resume")));
        assert!(!can_filter_active_probe(false, "/", Some("/y")));
        assert!(!can_filter_active_probe(true, "/resume", Some("/res")));
        assert!(!can_filter_active_probe(true, "/", None));
    }

    #[test]
    fn detects_direct_and_package_runner_commands() {
        assert_eq!(
            detect_command_line("claude --resume"),
            Some(LlmCliKind::Claude)
        );
        assert_eq!(
            detect_command_line("npx @google/gemini-cli"),
            Some(LlmCliKind::Gemini)
        );
        assert_eq!(detect_command_line("uvx kimi-cli"), Some(LlmCliKind::Kimi));
    }

    #[test]
    fn parses_kimi_boxed_menu_without_slash_and_skips_wrapped_descriptions() {
        let rows = [
            " │  Directory: F:\\repo                                        │",
            " ╰─────────────────────────────────────────────────────────────╯",
            " │   → yolo        Toggle YOLO mode, but the agent may still ask │",
            " │                 questions.                                   │",
            " │     model       Switch LLM model                              │",
            " │     permission  Select permission mode                        │",
            " │     plan        Toggle plan mode                               │",
            " │     settings    Open TUI settings                              │",
            " │     (1/74)                                                   │",
            " K3 thinking: high",
        ];
        assert_eq!(
            parse_kimi_slash_commands(rows, "/"),
            vec![
                SlashCommand {
                    command: "/yolo".into(),
                    description: "Toggle YOLO mode, but the agent may still ask".into(),
                },
                SlashCommand {
                    command: "/model".into(),
                    description: "Switch LLM model".into(),
                },
                SlashCommand {
                    command: "/permission".into(),
                    description: "Select permission mode".into(),
                },
                SlashCommand {
                    command: "/plan".into(),
                    description: "Toggle plan mode".into(),
                },
                SlashCommand {
                    command: "/settings".into(),
                    description: "Open TUI settings".into(),
                },
            ]
        );
    }

    #[test]
    fn parses_kimi_filtered_menu_without_pagination_row() {
        let rows = [
            " ╭────────────────────────────────────────────╮",
            " │ > /hel                                     │",
            " ╰────────────────────────────────────────────╯",
            " │   → help      Show help information         │",
            " K3 thinking: high",
        ];
        assert_eq!(
            parse_kimi_slash_commands(rows, "/hel"),
            vec![SlashCommand {
                command: "/help".into(),
                description: "Show help information".into(),
            }]
        );
    }

    #[test]
    fn kimi_composer_waits_for_the_real_editor_row() {
        assert!(kimi_editor_visible([
            " │ Send /y for help                              │",
            " │ >                                             │",
        ]));
        assert!(!kimi_editor_visible([
            "Kimi Code Update Available",
            "↑↓ choose · Enter confirm · Esc continue",
        ]));
    }

    #[test]
    fn kimi_clear_is_two_timed_stages_and_other_clis_finish_after_ctrl_u() {
        assert_eq!(
            slash_clear_stage(Some(LlmCliKind::Kimi), false),
            SlashClearStage::SendEscape
        );
        assert_eq!(
            slash_clear_stage(Some(LlmCliKind::Kimi), true),
            SlashClearStage::Finish
        );
        assert_eq!(
            slash_clear_stage(Some(LlmCliKind::Claude), false),
            SlashClearStage::Finish
        );
    }

    #[test]
    fn slash_probe_waits_until_deadline_instead_of_counting_terminal_updates() {
        let started = Instant::now();
        let mut probe = SlashProbeState::default();
        probe.begin_probe(
            "/y".to_owned(),
            started + std::time::Duration::from_millis(800),
        );

        assert!(!probe.probe_timed_out(started + std::time::Duration::from_millis(799)));
        assert!(probe.probe_timed_out(started + std::time::Duration::from_millis(800)));

        probe.clearing = true;
        assert!(
            !probe.probe_timed_out(started + std::time::Duration::from_secs(2)),
            "进入清理阶段后不能重复触发探测超时"
        );
    }

    #[test]
    fn parses_selected_command_and_kimi_exact_position() {
        let kimi_rows = [
            " │   → yolo        Toggle YOLO mode              │",
            " │     model       Switch LLM model              │",
            " │     (1/74)                                    │",
        ];
        assert_eq!(
            kimi_selected_command(kimi_rows, "/"),
            Some("/yolo".to_owned())
        );
        assert_eq!(
            kimi_rows.into_iter().find_map(slash_menu_position),
            Some((1, 74))
        );
        assert_eq!(
            selected_slash_command(
                [
                    "  ❯ /compact   Compact conversation",
                    "    /config    Open settings",
                ],
                "/"
            ),
            Some("/compact".to_owned())
        );
    }

    #[test]
    fn kimi_scan_finishes_at_reported_last_command() {
        let started = Instant::now();
        let mut probe = SlashProbeState::default();
        let first = SlashMenuSnapshot {
            commands: Vec::new(),
            selected_command: Some("/yolo".to_owned()),
            position: Some((1, 74)),
        };
        probe.begin_scan(
            &first,
            5,
            started,
            started + std::time::Duration::from_secs(30),
        );
        assert_eq!(
            probe.observe_scan(&first, 5, started, 100, 20),
            SlashScanDecision::Advance
        );

        let last = SlashMenuSnapshot {
            commands: Vec::new(),
            selected_command: Some("/version".to_owned()),
            position: Some((74, 74)),
        };
        assert_eq!(
            probe.observe_scan(&last, 74, started, 100, 20),
            SlashScanDecision::Finish
        );
    }

    #[test]
    fn generic_scan_finishes_when_selection_wraps_to_origin() {
        let started = Instant::now();
        let mut probe = SlashProbeState::default();
        let first = SlashMenuSnapshot {
            commands: Vec::new(),
            selected_command: Some("/compact".to_owned()),
            position: None,
        };
        probe.begin_scan(
            &first,
            4,
            started,
            started + std::time::Duration::from_secs(30),
        );

        let second = SlashMenuSnapshot {
            commands: Vec::new(),
            selected_command: Some("/config".to_owned()),
            position: None,
        };
        assert_eq!(
            probe.observe_scan(&second, 5, started, 100, 20),
            SlashScanDecision::Advance
        );
        assert_eq!(
            probe.observe_scan(&first, 5, started, 100, 20),
            SlashScanDecision::Finish
        );
    }

    #[test]
    fn scan_finishes_when_visible_page_wraps_without_selection_marker() {
        let started = Instant::now();
        let command = |name: &str| SlashCommand {
            command: name.to_owned(),
            description: String::new(),
        };
        let first = SlashMenuSnapshot {
            commands: vec![command("/model"), command("/permissions")],
            selected_command: None,
            position: None,
        };
        let mut probe = SlashProbeState::default();
        probe.begin_scan(
            &first,
            2,
            started,
            started + std::time::Duration::from_secs(30),
        );

        let second = SlashMenuSnapshot {
            commands: vec![command("/permissions"), command("/resume")],
            selected_command: None,
            position: None,
        };
        assert_eq!(
            probe.observe_scan(&second, 3, started, 100, 20),
            SlashScanDecision::Advance
        );
        assert_eq!(
            probe.observe_scan(&first, 3, started, 100, 20),
            SlashScanDecision::Finish
        );
    }

    #[test]
    fn kimi_scan_batches_one_visible_page_without_skipping_last_position() {
        let commands = (1..=5)
            .map(|index| SlashCommand {
                command: format!("/command-{index}"),
                description: String::new(),
            })
            .collect();
        let middle = SlashMenuSnapshot {
            commands,
            selected_command: Some("/command-1".to_owned()),
            position: Some((1, 74)),
        };
        assert_eq!(slash_scan_advance_steps(LlmCliKind::Kimi, &middle), 5);

        let near_end = SlashMenuSnapshot {
            commands: middle.commands.clone(),
            selected_command: Some("/command-71".to_owned()),
            position: Some((71, 74)),
        };
        assert_eq!(slash_scan_advance_steps(LlmCliKind::Kimi, &near_end), 3);
        assert_eq!(slash_scan_advance_steps(LlmCliKind::Claude, &middle), 1);
    }

    #[test]
    fn scan_stagnation_resets_on_new_commands_and_complete_prefix_is_reused() {
        let started = Instant::now();
        let snapshot = SlashMenuSnapshot {
            commands: Vec::new(),
            selected_command: None,
            position: None,
        };
        let mut probe = SlashProbeState::default();
        probe.begin_scan(
            &snapshot,
            3,
            started,
            started + std::time::Duration::from_secs(30),
        );
        assert_eq!(
            probe.observe_scan(&snapshot, 3, started, 100, 2),
            SlashScanDecision::Advance
        );
        assert_eq!(
            probe.observe_scan(&snapshot, 4, started, 100, 2),
            SlashScanDecision::Advance
        );
        assert_eq!(probe.scan_stagnant_steps, 0);
        assert_eq!(
            probe.observe_scan(&snapshot, 4, started, 100, 2),
            SlashScanDecision::Advance
        );
        assert_eq!(
            probe.observe_scan(&snapshot, 4, started, 100, 2),
            SlashScanDecision::Finish
        );

        probe.mark_prefix_complete("/".to_owned());
        assert!(probe.prefix_complete("/"));
        assert!(probe.prefix_complete("/resume"));
        probe.clear_active();
        assert!(probe.prefix_complete("/config"));
        probe.clear();
        assert!(!probe.prefix_complete("/"));
    }
}
