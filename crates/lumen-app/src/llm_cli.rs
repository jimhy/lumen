//! 通用 LLM CLI 识别与原生命令菜单解析。
//!
//! Lumen 只识别“当前前台程序是否为受支持的 LLM CLI”，不推断 CLI
//! 正在思考、输出或等待输入。识别成功后，footer 编辑器始终可用。

use std::path::Path;
use std::time::{Duration, Instant};

use lumen_term::{CellFlags, Terminal};

/// Lumen 当前内置适配的主流 LLM CLI。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// HUD 上下文百分比的语义。不同 CLI 有的报告“已用”，有的报告
/// “剩余”，必须保留原语义再由 UI 换算占用进度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HudContextKind {
    Used,
    Remaining,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HudContext {
    pub percent: u8,
    pub kind: HudContextKind,
}

/// 从状态行刮到的一个额度窗口。
///
/// Codex/Kimi 的额度有 JSON 通路（见 `llm_hud`），Claude 没有——它的
/// 额度只在状态行上以文字形式存在，只能从画面读。
#[derive(Debug, Clone, PartialEq)]
pub struct HudUsageWindow {
    /// 窗口名，与 `llm_hud` 的 JSON 通路同口径（`"5h"` / `"7d"`）。
    pub label: &'static str,
    pub used_percent: f32,
    /// 距重置还剩多久。状态行改用绝对时刻（`重置于 14:30`）时取不到。
    pub resets_in: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct HudMetrics {
    pub model: Option<String>,
    pub context: Option<HudContext>,
    pub usage: Vec<HudUsageWindow>,
}

/// 从当前 CLI 已经绘制到终端里的原生状态信息生成 HUD 数据。
///
/// 这里只做只读解析：不注入命令、不读取账号配置，也不估算 token。
/// CLI 没有把模型/上下文显示出来时对应字段保持 `None`。
pub fn hud_metrics(term: &Terminal, kind: LlmCliKind) -> HudMetrics {
    let rows = visible_rows(term);
    HudMetrics {
        model: hud_model(rows.iter().rev().map(String::as_str), term.title(), kind),
        context: rows.iter().rev().find_map(|row| hud_context_from_line(row)),
        usage: hud_usage(rows.iter().rev().map(String::as_str), kind),
    }
}

/// 从状态行读额度窗口。
///
/// **只对 Claude 做**：其余 CLI 的额度有结构化 JSON 原值（见 `llm_hud`），
/// 拿画面去猜只会多一条误匹配的路——而画面上一次误匹配就足以顶掉一份准数。
/// Claude 没有任何本地额度文件（`~/.claude.json` 的缓存只有 `/usage` 面板
/// 会刷新，实测能停在几天前），状态行是唯一的活数据。
fn hud_usage<'a>(rows: impl IntoIterator<Item = &'a str>, kind: LlmCliKind) -> Vec<HudUsageWindow> {
    if !matches!(kind, LlmCliKind::Claude) {
        return Vec::new();
    }
    rows.into_iter()
        .find_map(|row| {
            let windows = hud_usage_from_line(row);
            (!windows.is_empty()).then_some(windows)
        })
        .unwrap_or_default()
}

/// 状态行把多组指标画在同一行里，用这些字符分隔。
///
/// 分段是必须的而不是锦上添花：实测 claude-hud 会把
/// `上下文 ░░ 1% │ 用量 ██ 45% | 本周 ██ 30% (重置剩余 6d 19h)`
/// 画成**一整行**，按整行做关键词匹配会让「重置剩余」里的「剩余」
/// 命中上下文的语义判定，把「已用 1%」显示成「剩余 1%」。
const SEGMENT_SEPARATORS: [char; 5] = ['│', '┃', '|', '·', '•'];

fn hud_model<'a>(
    rows: impl IntoIterator<Item = &'a str>,
    title: &'a str,
    kind: LlmCliKind,
) -> Option<String> {
    rows.into_iter()
        .chain(std::iter::once(title))
        .flat_map(|line| line.split(SEGMENT_SEPARATORS))
        .find_map(|segment| hud_model_segment(segment, kind))
}

fn hud_model_segment(segment: &str, kind: LlmCliKind) -> Option<String> {
    let mut value = segment
        .trim()
        .trim_matches(|ch: char| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '[' | ']' | '(' | ')' | '{' | '}' | '<' | '>' | '─' | '━' | '═'
                )
        })
        .trim();
    let lower = value.to_ascii_lowercase();
    if let Some(rest) = lower
        .strip_prefix("model:")
        .or_else(|| lower.strip_prefix("model "))
    {
        let offset = value.len().saturating_sub(rest.len());
        value = value[offset..].trim();
    }
    let lower = value.to_ascii_lowercase();
    let matches_kind = match kind {
        LlmCliKind::Claude => {
            lower.contains("opus")
                || lower.contains("sonnet")
                || lower.contains("haiku")
                || lower.contains("claude-")
        }
        LlmCliKind::Codex => lower.split_whitespace().any(|token| {
            let token = token.trim_matches(['[', ']', '(', ')', ',']);
            token.starts_with("gpt-")
                || token.starts_with("codex-")
                || matches!(token, "o1" | "o3" | "o3-mini" | "o4" | "o4-mini")
        }),
        LlmCliKind::Gemini => lower.contains("gemini-"),
        LlmCliKind::Kimi => {
            lower.contains("kimi-")
                || lower.split_whitespace().any(|token| {
                    token.starts_with("k2-")
                        || token.starts_with("k2.")
                        || token.starts_with("moonshot-")
                })
        }
    };
    if !matches_kind || value.chars().count() > 64 {
        return None;
    }
    Some(value.to_owned())
}

/// 表示“还剩多少”的措辞。命中即把百分比按剩余量解读。
///
/// `until auto-compact` 是 Claude Code 自带状态行的说法，实测源码里
/// 它取的就是剩余量（`${pct_left}% until auto-compact`）。
const REMAINING_NEEDLES: [&str; 9] = [
    "left",
    "remaining",
    "available",
    "free",
    "remain",
    "until auto-compact",
    "剩余",
    "剩餘",
    "可用",
];

/// 表示“已经用掉多少”的措辞。只用于识别自相矛盾的段落，
/// 不命中也照样按已用解读——带进度条的裸百分比（`Context ██░░ 45%`）
/// 是各家 CLI 的通用画法，一律指已用。
const USED_NEEDLES: [&str; 4] = ["used", "已用", "占用", "使用"];

fn hud_context_from_line(line: &str) -> Option<HudContext> {
    line.split(SEGMENT_SEPARATORS).find_map(hud_context_segment)
}

fn hud_context_segment(segment: &str) -> Option<HudContext> {
    if !segment.contains('%') {
        return None;
    }
    let lower = segment.to_ascii_lowercase();
    // Claude Code 自带状态行的低水位形态只说 auto-compact、不带 context 字样，
    // 光认 context 会把它整条漏掉。
    if !lower.contains("context") && !lower.contains("auto-compact") && !segment.contains("上下文")
    {
        return None;
    }
    let percent = percent_in_line(segment)?;
    let remaining = contains_any(&lower, segment, &REMAINING_NEEDLES);
    // 一段话里两种语义都出现说明它没法判准。宁可空着，也不显示一个
    // 反过来的数——反过来的数会连进度条一起反（见 shell/hud.rs 的换算）。
    if remaining && contains_any(&lower, segment, &USED_NEEDLES) {
        return None;
    }
    Some(HudContext {
        percent,
        kind: if remaining {
            HudContextKind::Remaining
        } else {
            HudContextKind::Used
        },
    })
}

/// 关键词既可能是 ASCII（须忽略大小写）也可能是中文（原样匹配）。
fn contains_any(lower: &str, raw: &str, needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|needle| lower.contains(needle) || raw.contains(needle))
}

fn percent_in_line(line: &str) -> Option<u8> {
    let chars = line.char_indices().collect::<Vec<_>>();
    for (index, &(byte, ch)) in chars.iter().enumerate() {
        if ch != '%' {
            continue;
        }
        let mut start_index = index;
        // 小数点也要吞进来。只认整数位的话，`77.5%`（`/context` 面板那张表
        // 就是一位小数）会被读成 5——一个看着完全正常的错数。
        while start_index > 0 && matches!(chars[start_index - 1].1, '0'..='9' | '.') {
            start_index -= 1;
        }
        if start_index == index {
            continue;
        }
        let start = chars[start_index].0;
        // 解析不出或越界只跳过这一个百分号，不能中断整段扫描——
        // 否则一个 `999%` 会让它后面真正的百分比全部读不到。
        if let Ok(value) = line[start..byte].parse::<f64>() {
            if (0.0..=100.0).contains(&value) {
                return Some(value.round() as u8);
            }
        }
    }
    None
}

/// 周额度窗口的措辞。先于 5 小时窗口判定：状态行只画周窗口时，
/// 段首仍带着「用量 / Usage」这个总标题，两者会同时出现。
const WEEKLY_NEEDLES: [&str; 5] = ["7d", "本周", "weekly", "week", "每周"];

/// 5 小时额度窗口的措辞。
const FIVE_HOUR_NEEDLES: [&str; 3] = ["5h", "用量", "usage"];

fn hud_usage_from_line(line: &str) -> Vec<HudUsageWindow> {
    let mut windows: Vec<HudUsageWindow> = Vec::new();
    for segment in line.split(SEGMENT_SEPARATORS) {
        collect_usage_windows(segment, &mut windows);
    }
    windows
}

/// 一个分段里可能并排画着多个窗口——Claude Code 文档自带的状态行例子
/// 就是 `5h:45% 7d:30%`，中间没有任何分隔符。所以分段之后还得按窗口名
/// 的出现位置再切一刀：每个窗口名到下一个窗口名之间，才是它自己的数。
/// 整段只取第一个百分比会把 5h 的数字挂到 7d 名下，**数错了还不报错**。
fn collect_usage_windows(segment: &str, windows: &mut Vec<HudUsageWindow>) {
    if !segment.contains('%') {
        return;
    }
    let lower = segment.to_ascii_lowercase();
    // 上下文与额度是两个指标，一段只归一个；否则上下文段里的百分比
    // 会被某个额度关键词捎带成一个凭空的额度窗口。
    if lower.contains("context") || segment.contains("上下文") {
        return;
    }
    let hits = usage_label_hits(&lower);
    for (index, &(start, label)) in hits.iter().enumerate() {
        let end = hits.get(index + 1).map_or(segment.len(), |&(next, _)| next);
        let slice = &segment[start..end];
        let Some(percent) = percent_in_line(slice) else {
            continue;
        };
        if windows.iter().any(|seen| seen.label == label) {
            continue;
        }
        windows.push(HudUsageWindow {
            label,
            used_percent: f32::from(percent),
            resets_in: reset_countdown(slice),
        });
    }
}

/// 段内所有窗口名的出现位置，按位置升序、同一位置只留一条。
///
/// `to_ascii_lowercase` 只改 ASCII 字母、不改字节数，所以 `lower` 里的
/// 下标可以直接拿去切原文。
fn usage_label_hits(lower: &str) -> Vec<(usize, &'static str)> {
    let mut hits: Vec<(usize, &'static str)> = Vec::new();
    for (needles, label) in [
        (WEEKLY_NEEDLES.as_slice(), "7d"),
        (FIVE_HOUR_NEEDLES.as_slice(), "5h"),
    ] {
        for needle in needles {
            hits.extend(
                lower
                    .match_indices(needle)
                    .filter(|&(start, _)| starts_at_word_boundary(lower, start))
                    .map(|(start, _)| (start, label)),
            );
        }
    }
    hits.sort_by_key(|&(start, _)| start);
    // 「weekly」与「week」起点相同；周窗口先入表，同位置保留先入的那条。
    hits.dedup_by_key(|&mut (start, _)| start);
    hits
}

/// 窗口名前面不能紧挨着字母数字。
///
/// 少了这一条，倒计时里的 `4d 15h` 会因为 `15h` 含子串 `5h` 被当成一个
/// 5 小时窗口的名字，把周窗口的切片从括号中间截断——周窗口的倒计时随之丢掉。
fn starts_at_word_boundary(text: &str, start: usize) -> bool {
    text[..start]
        .chars()
        .next_back()
        .is_none_or(|ch| !ch.is_ascii_alphanumeric())
}

/// 从 `(重置剩余 6d 19h)` / `(resets in 2h 30m)` 这类括号里取倒计时。
///
/// 只认括号内的内容，免得把别处的数字当成时长。**要逐个括号从左往右试，
/// 不能只看最后一个**：状态行末尾常被别的东西拼上一个无关括号
/// （实测 engram 状态行空闲时是 `● Engram (idle)`，且用空格拼接、
/// 不带分隔符，就落在周窗口那一段里），只看最后一个会把真倒计时整个吞掉。
/// 一个都认不出（例如配成了绝对时刻 `(重置于 14:30)`）返回 `None`，
/// 少显示一个倒计时好过显示一个错的。
fn reset_countdown(text: &str) -> Option<Duration> {
    let mut rest = text;
    while let Some(open) = rest.find('(') {
        let after = &rest[open + 1..];
        let close = after.find(')')?;
        if let Some(found) = countdown_of(&after[..close]) {
            return Some(found);
        }
        rest = &after[close + 1..];
    }
    None
}

/// 把 `6d 19h` / `2h 30m` 这类时长累加成秒；一个单位都认不出返回 `None`。
fn countdown_of(inner: &str) -> Option<Duration> {
    let mut seconds = 0_u64;
    let mut digits = String::new();
    for ch in inner.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        let unit = match ch {
            'd' | 'D' => 24 * 60 * 60,
            'h' | 'H' => 60 * 60,
            'm' | 'M' => 60,
            _ => {
                digits.clear();
                continue;
            }
        };
        if let Ok(value) = digits.parse::<u64>() {
            seconds = seconds.saturating_add(value.saturating_mul(unit));
        }
        digits.clear();
    }
    (seconds > 0).then(|| Duration::from_secs(seconds))
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
    fn parses_hud_models_for_supported_clis() {
        assert_eq!(
            hud_model(["[Opus 4.6] │ lumen"].into_iter(), "", LlmCliKind::Claude),
            Some("Opus 4.6".into())
        );
        assert_eq!(
            hud_model(
                ["gpt-5.4 high · 82% context left"].into_iter(),
                "",
                LlmCliKind::Codex
            ),
            Some("gpt-5.4 high".into())
        );
        assert_eq!(
            hud_model(
                ["Model: gemini-2.5-pro"].into_iter(),
                "",
                LlmCliKind::Gemini
            ),
            Some("gemini-2.5-pro".into())
        );
        assert_eq!(
            hud_model(["│ kimi-k2.5 │"].into_iter(), "", LlmCliKind::Kimi),
            Some("kimi-k2.5".into())
        );
    }

    #[test]
    fn hud_ignores_cli_brand_without_a_model() {
        assert_eq!(
            hud_model(["Claude Code"].into_iter(), "", LlmCliKind::Claude),
            None
        );
        assert_eq!(
            hud_model(["Kimi Code"].into_iter(), "", LlmCliKind::Kimi),
            None
        );
    }

    #[test]
    fn parses_used_and_remaining_context_percentages() {
        assert_eq!(
            hud_context_from_line("Context ███░░ 45%"),
            Some(HudContext {
                percent: 45,
                kind: HudContextKind::Used,
            })
        );
        assert_eq!(
            hud_context_from_line("gpt-5.4 high · 82% context left"),
            Some(HudContext {
                percent: 82,
                kind: HudContextKind::Remaining,
            })
        );
        assert_eq!(
            hud_context_from_line("上下文剩余 63%"),
            Some(HudContext {
                percent: 63,
                kind: HudContextKind::Remaining,
            })
        );
        assert_eq!(
            hud_context_from_line("0% context used"),
            Some(HudContext {
                percent: 0,
                kind: HudContextKind::Used,
            })
        );
        assert_eq!(hud_context_from_line("CPU 45%"), None);
    }

    /// Claude Code 2.1.226 自带状态行的三种形态，逐字取自其二进制。
    #[test]
    fn parses_claude_native_status_line_wordings() {
        assert_eq!(
            hud_context_from_line("8% context used · /model opus[1m]"),
            Some(HudContext {
                percent: 8,
                kind: HudContextKind::Used,
            })
        );
        assert_eq!(
            hud_context_from_line("23% until auto-compact"),
            Some(HudContext {
                percent: 23,
                kind: HudContextKind::Remaining,
            })
        );
        assert_eq!(
            hud_context_from_line(
                "Context low (5% remaining) · Run /compact to compact & continue"
            ),
            Some(HudContext {
                percent: 5,
                kind: HudContextKind::Remaining,
            })
        );
    }

    /// claude-hud 把上下文与额度画在同一行，而额度那半边带着「重置剩余」。
    /// 按整行做关键词匹配会让「剩余」跨段命中，把「已用 1%」显示成
    /// 「剩余 1%」——进度条也跟着反成 99%。逐字取自本机实测输出。
    #[test]
    fn context_ignores_remaining_wording_from_other_segments() {
        let line = "上下文 ░░░░░░░░░░ 1% │ 用量 █████░░░░░ 45% | \
                    本周 ███░░░░░░░ 30% (重置剩余 6d 19h)   ● Engram | L1:1 L2:1 L3:13";
        assert_eq!(
            hud_context_from_line(line),
            Some(HudContext {
                percent: 1,
                kind: HudContextKind::Used,
            })
        );
        assert_eq!(
            hud_context_from_line("Context ░░ 0% │ Usage ██ 45% | Weekly ██ 30% (resets in 2h)"),
            Some(HudContext {
                percent: 0,
                kind: HudContextKind::Used,
            })
        );
    }

    /// 同一段里两种语义都出现就判不准了，此时空着比显示反的数安全。
    #[test]
    fn context_with_conflicting_wording_is_dropped() {
        assert_eq!(hud_context_from_line("Context 10% used, 90% free"), None);
    }

    /// 越界的百分比只能跳过它自己，不能中断整段扫描。
    #[test]
    fn percent_scan_skips_out_of_range_values() {
        assert_eq!(percent_in_line("999% 42%"), Some(42));
        assert_eq!(percent_in_line("101%"), None);
        assert_eq!(percent_in_line("no digits %"), None);
        // 小数：只吞整数位会把 77.5 读成 5。
        assert_eq!(percent_in_line("77.5%"), Some(78));
        assert_eq!(percent_in_line("155.0k | 12%"), Some(12));
        assert_eq!(percent_in_line("1.2.3%"), None);
    }

    #[test]
    fn parses_usage_windows_from_status_line() {
        let line = "上下文 ░░░░░░░░░░ 1% │ 用量 █████░░░░░ 45% | \
                    本周 ███░░░░░░░ 30% (重置剩余 6d 19h)   ● Engram | L1:1 L2:1 L3:13";
        assert_eq!(
            hud_usage_from_line(line),
            vec![
                HudUsageWindow {
                    label: "5h",
                    used_percent: 45.0,
                    resets_in: None,
                },
                HudUsageWindow {
                    label: "7d",
                    used_percent: 30.0,
                    resets_in: Some(Duration::from_secs((6 * 24 + 19) * 3600)),
                },
            ]
        );
    }

    #[test]
    fn parses_usage_windows_in_english_and_compact_forms() {
        assert_eq!(
            hud_usage_from_line("Usage ██ 45% | Weekly ██ 30% (resets in 6d 19h)"),
            vec![
                HudUsageWindow {
                    label: "5h",
                    used_percent: 45.0,
                    resets_in: None,
                },
                HudUsageWindow {
                    label: "7d",
                    used_percent: 30.0,
                    resets_in: Some(Duration::from_secs((6 * 24 + 19) * 3600)),
                },
            ]
        );
        assert_eq!(
            hud_usage_from_line("5h: 45% (2h 30m) | 7d: 30% (6d)"),
            vec![
                HudUsageWindow {
                    label: "5h",
                    used_percent: 45.0,
                    resets_in: Some(Duration::from_secs(2 * 3600 + 30 * 60)),
                },
                HudUsageWindow {
                    label: "7d",
                    used_percent: 30.0,
                    resets_in: Some(Duration::from_secs(6 * 24 * 3600)),
                },
            ]
        );
    }

    /// 同一段里并排两个窗口时，各取各的数——Claude Code 文档自带的状态行
    /// 例子就是这个形状，中间没有分隔符。整段取第一个百分比会把 45 挂到
    /// 7d 名下。
    #[test]
    fn adjacent_windows_in_one_segment_keep_their_own_numbers() {
        assert_eq!(
            hud_usage_from_line("5h:45% 7d:30%"),
            vec![
                HudUsageWindow {
                    label: "5h",
                    used_percent: 45.0,
                    resets_in: None,
                },
                HudUsageWindow {
                    label: "7d",
                    used_percent: 30.0,
                    resets_in: None,
                },
            ]
        );
        assert_eq!(
            hud_usage_from_line("5h: 45%"),
            vec![HudUsageWindow {
                label: "5h",
                used_percent: 45.0,
                resets_in: None,
            }]
        );
    }

    /// 倒计时里的 `4d 15h` 含子串 `5h`，不能被当成 5 小时窗口的名字——
    /// 否则周窗口的切片会从括号中间被截断，倒计时随之丢掉。
    #[test]
    fn digits_before_a_window_name_do_not_start_a_window() {
        assert_eq!(
            hud_usage_from_line("本周 ███ 30% (重置剩余 4d 15h)"),
            vec![HudUsageWindow {
                label: "7d",
                used_percent: 30.0,
                resets_in: Some(Duration::from_secs((4 * 24 + 15) * 3600)),
            }]
        );
    }

    /// engram 状态行空闲时在行尾拼一个 `(idle)`，且用空格拼接、不带分隔符，
    /// 于是它落在周窗口那一段里。只看最后一个括号会把真倒计时整个吞掉。
    /// 这是会话刚开时的常态形态。
    #[test]
    fn trailing_unrelated_parenthesis_does_not_swallow_the_countdown() {
        let line = "上下文 ░░░░░░░░░░ 1% │ 用量 █░░░░░░░░░ 12% (重置剩余 3h 20m) | \
                    本周 ███░░░░░░░ 30% (重置剩余 4d 15h)   ● Engram (idle)";
        assert_eq!(
            hud_context_from_line(line),
            Some(HudContext {
                percent: 1,
                kind: HudContextKind::Used,
            })
        );
        assert_eq!(
            hud_usage_from_line(line),
            vec![
                HudUsageWindow {
                    label: "5h",
                    used_percent: 12.0,
                    resets_in: Some(Duration::from_secs(3 * 3600 + 20 * 60)),
                },
                HudUsageWindow {
                    label: "7d",
                    used_percent: 30.0,
                    resets_in: Some(Duration::from_secs((4 * 24 + 15) * 3600)),
                },
            ]
        );
    }

    /// Claude Code 文档给的自定义状态行示例会打 `Context: N% remaining`，
    /// 以及 `/context` 面板里那张含「Free space」的表——后者不该被认成上下文。
    #[test]
    fn context_handles_documented_status_line_and_context_panel() {
        assert_eq!(
            hud_context_from_line("Context: 63% remaining"),
            Some(HudContext {
                percent: 63,
                kind: HudContextKind::Remaining,
            })
        );
        assert_eq!(
            hud_context_from_line("| Free space | 155.0k | 77.5% |"),
            None
        );
        assert_eq!(hud_context_from_line("Context Usage"), None);
    }

    /// 只画周窗口时段首仍带着「用量」总标题，不能因此判成 5 小时窗口。
    #[test]
    fn weekly_only_segment_is_not_mistaken_for_five_hour() {
        assert_eq!(
            hud_usage_from_line("用量 本周 ███ 30% (重置于 14:30)"),
            vec![HudUsageWindow {
                label: "7d",
                used_percent: 30.0,
                resets_in: None,
            }]
        );
    }

    #[test]
    fn usage_ignores_lines_without_a_window_label() {
        assert!(hud_usage_from_line("上下文 ░░ 1%").is_empty());
        assert!(hud_usage_from_line("CPU 45% | MEM 30%").is_empty());
    }

    /// 端到端：把 claude-hud（zh-Hans）实际画出来的那一行连同 SGR 一起喂进真终端，
    /// 走完整条 `hud_metrics`——顺带覆盖宽字符占位过滤与自下而上的取行顺序。
    #[test]
    fn hud_metrics_reads_context_and_usage_from_a_real_status_line() {
        let mut term = Terminal::new(6, 160, 100);
        term.advance(b"claude-opus-4-5\r\n");
        term.advance(
            "\x1b[2m上下文\x1b[0m \x1b[32m░░░░░░░░░░\x1b[0m \x1b[32m1%\x1b[0m │ \
             \x1b[2m用量\x1b[0m \x1b[94m█████\x1b[2m░░░░░\x1b[0m \x1b[94m45%\x1b[0m | \
             \x1b[2m本周\x1b[0m \x1b[94m███\x1b[2m░░░░░░░\x1b[0m \x1b[94m30%\x1b[0m \
             (重置剩余 6d 19h)"
                .as_bytes(),
        );

        let metrics = hud_metrics(&term, LlmCliKind::Claude);
        assert_eq!(
            metrics.context,
            Some(HudContext {
                percent: 1,
                kind: HudContextKind::Used,
            })
        );
        assert_eq!(metrics.model.as_deref(), Some("claude-opus-4-5"));
        assert_eq!(
            metrics.usage,
            vec![
                HudUsageWindow {
                    label: "5h",
                    used_percent: 45.0,
                    resets_in: None,
                },
                HudUsageWindow {
                    label: "7d",
                    used_percent: 30.0,
                    resets_in: Some(Duration::from_secs((6 * 24 + 19) * 3600)),
                },
            ]
        );
    }

    /// 有 JSON 通路的 CLI 不吃画面这条，免得误匹配顶掉准确数据。
    #[test]
    fn usage_is_scraped_only_for_claude() {
        let rows = ["用量 ██ 45% | 本周 ██ 30%"];
        assert_eq!(hud_usage(rows, LlmCliKind::Claude).len(), 2);
        assert!(hud_usage(rows, LlmCliKind::Codex).is_empty());
        assert!(hud_usage(rows, LlmCliKind::Kimi).is_empty());
        assert!(hud_usage(rows, LlmCliKind::Gemini).is_empty());
    }

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
