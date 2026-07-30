//! 输入模式机（M4.1 批B）——设计稿 §2。
//!
//! # 核心纪律（铁律）
//! **禁止任何地方缓存模式副本。** 模式是推导值，由 [`input_mode`] 从终端
//! 状态单点求值；每次按键处理与每批 PTY 数据消化后实时调用，不在
//! `AppState` 或任何结构体字段里保存 `InputMode` 的副本——防止状态漂移
//! 与「模式锁死」类 bug（设计稿 §2 铁律，M4 远程控制同一路径）。
//!
//! # 使用方式
//! ```no_run
//! # use lumen_term::Terminal;
//! # use crate::mode::{input_mode, effective_mode};
//! // 直接按需求值，禁止保存结果到字段
//! let mode = effective_mode(&term, force_fallback);
//! ```
//!
//! # 设计稿对应章节
//! 设计稿 §2「输入模式机（纯推导）」。

use lumen_term::Terminal;

/// 输入模式。
///
/// 模式是**推导值**，由 [`input_mode`] 从终端状态单点求值。
/// **禁止在任何结构体字段里缓存此枚举副本**（设计稿 §2 铁律）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// 等待用户输入命令（OSC 133;B 到、133;C 未到）。
    ///
    /// 按键路由：M4.1 批B 暂与 Running 相同（直通 PTY），批D 开闸时
    /// 切换为本地编辑（keymap 表内 Compose 态条目到时更新）。
    Compose,
    /// 命令运行中（133;C 到、133;D 未到；或 y/n 确认 / REPL / 密码）。
    ///
    /// 按键路由：逐键直通 PTY，不做本地缓冲。
    Running,
    /// 已识别的大模型 CLI 正在运行，且保留 Lumen 智能输入框。
    ///
    /// 默认在本地 composer 编辑，Enter 后把整段提示发送给 CLI；用户切换
    /// 经典直通模式后才进入 [`InputMode::LlmCli`]，以使用 CLI 原生的逐键交互。
    LlmCompose,
    /// 已识别的大模型 CLI（Claude Code / Codex / Kimi）正在运行。
    ///
    /// 用户已开启经典直通：逐键交给 CLI，让 `/` 命令菜单和原生图片粘贴
    /// 正常工作；单独成态让 keymap 能对图片剪贴板做专用放行。
    LlmCli,
    /// 备用屏幕（vim / htop / codex TUI；`is_alt_screen()` == true）。
    ///
    /// 按键路由：完全直通（含 IME、Ctrl+C/V）。
    AltScreen,
    /// 降级直通（shell integration 未生效 / 注入失败，blocks 为空）。
    ///
    /// 按键路由：永久直通 = M2 现状。
    Fallback,
}

impl InputMode {
    /// 此模式是否由 Lumen 本地智能输入框接管编辑。
    pub const fn uses_composer(self) -> bool {
        matches!(self, Self::Compose | Self::LlmCompose)
    }
}

/// 从终端状态纯函数推导输入模式（设计稿 §2 原文实现）。
///
/// **禁止在任何地方缓存此函数的返回值到字段**。
///
/// # 推导规则
/// 1. 当前未闭合命令是受支持的大模型 CLI → [`InputMode::LlmCompose`]
/// 2. `is_alt_screen()` → [`InputMode::AltScreen`]（全屏 TUI 让位）
/// 3. `blocks` 为空 → [`InputMode::Fallback`]（从未见 OSC 133 标记，降级直通）
/// 4. 最后一块 `cmd_line.is_some()` && `output_line.is_none()` → [`InputMode::Compose`]
///    （133;B 到、133;C 未到 = PSReadLine 正在等输入）
/// 5. 其余 → [`InputMode::Running`]（命令执行中 / REPL / 密码输入等）
pub fn input_mode(term: &Terminal) -> InputMode {
    // 规则 1：受支持的 LLM CLI。默认保留 Lumen 智能输入框；用户显式切换
    // 经典模式后由 effective_mode 改为 LlmCli 原生直通。
    if active_llm_cli(term) {
        return InputMode::LlmCompose;
    }
    // 规则 2：备用屏幕（vim / htop 等）。先于 blocks 为空判断，使尚无
    // shell integration 的 Linux/macOS 会话也能把全屏 TUI 键盘完整让位。
    if term.is_alt_screen() {
        return InputMode::AltScreen;
    }
    // 规则 3：从未见 133 标记 → 降级直通
    if term.blocks().is_empty() {
        return InputMode::Fallback;
    }
    // 规则 4：133;B 到、133;C 未到 = 等待输入
    match term.blocks().last() {
        Some(b) if b.cmd_line.is_some() && b.output_line.is_none() => InputMode::Compose,
        // 规则 5：其余均为运行中（含命令已发、退出码未到、REPL 等）
        _ => InputMode::Running,
    }
}

/// 当前未闭合命令是否为已验证具备「图片粘贴 + `/` 命令补全」的 LLM CLI。
///
/// `cmd_text` 来自 shell integration 在 OSC 133;C 中上报的权威命令文本，
/// 因而不会依赖进程树形态（npm/PowerShell 包装器经常把真正程序藏在
/// `node.exe` 或更深的子进程后面）。
pub fn active_llm_cli(term: &Terminal) -> bool {
    term.blocks().last().is_some_and(|block| {
        block.output_line.is_some()
            && !block.is_closed()
            && block
                .cmd_text
                .as_deref()
                .is_some_and(is_supported_llm_command)
    })
}

/// 识别已调研并验证过交互能力的 CLI 启动命令。
///
/// 只识别命令位置，不扫描普通参数，避免 `echo codex` 一类误判。除直接命令
/// 外兼容 PowerShell 调用运算符与常见的一次性包执行器。
fn is_supported_llm_command(command: &str) -> bool {
    let tokens = command_tokens(command);
    let mut index = 0;

    while tokens.get(index).is_some_and(|token| token == "&") {
        index += 1;
    }

    let Some(program) = tokens.get(index).map(|token| executable_name(token)) else {
        return false;
    };
    if is_llm_program(&program) {
        return true;
    }

    match program.as_str() {
        "npx" | "bunx" | "pnpx" | "uvx" => tokens
            .iter()
            .skip(index + 1)
            .find(|token| !token.starts_with('-'))
            .is_some_and(|token| is_llm_package(token)),
        "npm"
            if tokens
                .get(index + 1)
                .is_some_and(|token| token.eq_ignore_ascii_case("exec")) =>
        {
            tokens
                .iter()
                .skip(index + 2)
                .find(|token| token.as_str() != "--" && !token.starts_with('-'))
                .is_some_and(|token| is_llm_package(token))
        }
        "pnpm" | "yarn"
            if tokens
                .get(index + 1)
                .is_some_and(|token| token.eq_ignore_ascii_case("dlx")) =>
        {
            tokens
                .iter()
                .skip(index + 2)
                .find(|token| !token.starts_with('-'))
                .is_some_and(|token| is_llm_package(token))
        }
        "python" | "python3" | "py"
            if tokens
                .get(index + 1)
                .is_some_and(|token| token.eq_ignore_ascii_case("-m")) =>
        {
            tokens
                .get(index + 2)
                .is_some_and(|token| is_llm_package(token))
        }
        _ => false,
    }
}

fn is_llm_program(program: &str) -> bool {
    matches!(
        program,
        "claude" | "codex" | "kimi" | "kimi-cli" | "kimi-code"
    )
}

fn is_llm_package(package: &str) -> bool {
    let normalized = package.trim_matches(['\'', '"']).to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "@anthropic-ai/claude-code"
            | "@openai/codex"
            | "@moonshot-ai/kimi-code"
            | "kimi-cli"
            | "kimi_cli"
            | "kimi-code"
    )
}

fn executable_name(token: &str) -> String {
    let token = token.trim_matches(['\'', '"']);
    let file = token.rsplit(['/', '\\']).next().unwrap_or(token);
    let normalized = file.to_ascii_lowercase();
    [".exe", ".cmd", ".bat", ".ps1"]
        .iter()
        .find_map(|suffix| normalized.strip_suffix(suffix))
        .unwrap_or(&normalized)
        .to_owned()
}

/// 足够解析 CLI 启动命令的轻量 tokenizer：空白分词、保留引号内空格，
/// 并把 PowerShell 的独立 `&` 调用运算符作为普通 token。
fn command_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;

    for ch in command.chars() {
        match quote {
            Some(end) if ch == end => quote = None,
            Some(_) => current.push(ch),
            None if matches!(ch, '\'' | '"') => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// 有效输入模式（含 `force_fallback` 手动逃生舱覆盖层）。
///
/// `Ctrl+Shift+E` 置位 `force_fallback` 后，普通程序返回
/// [`InputMode::Fallback`]；活动 LLM CLI 返回 [`InputMode::LlmCli`]，
/// 进入支持 `/` 提示与图片粘贴的原生直通。CLI 退出后仍保持经典模式。
///
/// **模式机纯函数 [`input_mode`] 本身不变；此函数是唯一的「逃生舱包装层」。**
///
/// # Arguments
/// * `term` - 终端状态机引用（按需求值，不缓存）。
/// * `force_fallback` - `AppState::force_fallback` 字段（Ctrl+Shift+E 开关）。
pub fn effective_mode(term: &Terminal, force_fallback: bool) -> InputMode {
    // 活动 LLM 仅在用户显式开启经典模式后进入专用直通态；默认由
    // input_mode 返回 LlmCompose，保留智能输入框。
    if force_fallback && active_llm_cli(term) {
        return InputMode::LlmCli;
    }
    if force_fallback {
        return InputMode::Fallback;
    }
    input_mode(term)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_term::Terminal;

    /// 构造一个指定行列的终端，并用 VT 序列注入 OSC 133 标记。
    fn make_term(rows: usize, cols: usize) -> Terminal {
        Terminal::new(rows, cols, 100)
    }

    /// 向终端注入原始字节序列。
    fn feed(term: &mut Terminal, data: &[u8]) {
        term.advance(data);
    }

    // ── 规则 1：blocks 为空 → Fallback ──────────────────────────────────

    #[test]
    fn fallback_when_no_blocks() {
        let term = make_term(24, 80);
        assert_eq!(
            input_mode(&term),
            InputMode::Fallback,
            "无 block 应为 Fallback"
        );
    }

    // ── 规则 2：is_alt_screen → AltScreen ───────────────────────────────

    #[test]
    fn alt_screen_when_smcup() {
        let mut term = make_term(24, 80);
        // 先注入 133;A（Prompt Start）+ 133;B（Command Start）让 blocks 非空
        feed(&mut term, b"\x1b]133;A\x07\x1b]133;B\x07");
        // 进入备用屏幕
        feed(&mut term, b"\x1b[?1049h");
        assert_eq!(
            input_mode(&term),
            InputMode::AltScreen,
            "进入备用屏后应为 AltScreen"
        );
    }

    #[test]
    fn alt_screen_precedes_fallback_without_shell_integration() {
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b[?1049h");
        assert_eq!(
            input_mode(&term),
            InputMode::AltScreen,
            "无 OSC blocks 的跨平台 TUI 仍应完整让位"
        );
    }

    // ── 规则 3：133;B 到、133;C 未到 → Compose ──────────────────────────

    #[test]
    fn compose_when_b_without_c() {
        let mut term = make_term(24, 80);
        // A 标记（Prompt Start）
        feed(&mut term, b"\x1b]133;A\x07");
        // B 标记（Command Start = 提示符渲染完毕，等待输入）
        feed(&mut term, b"\x1b]133;B\x07");
        assert_eq!(
            input_mode(&term),
            InputMode::Compose,
            "133;B 后未 133;C 应为 Compose"
        );
    }

    // ── 规则 4：133;C 到 → Running ──────────────────────────────────────

    #[test]
    fn running_when_c_received() {
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b]133;A\x07");
        feed(&mut term, b"\x1b]133;B\x07");
        // C 标记（Output Start = 用户回车执行命令）
        feed(&mut term, b"\x1b]133;C\x07");
        assert_eq!(
            input_mode(&term),
            InputMode::Running,
            "133;C 后应为 Running"
        );
    }

    // ── 规则 2：受支持的 LLM CLI → LlmCompose ──────────────────────────

    #[test]
    fn llm_compose_when_supported_command_is_running() {
        let mut term = make_term(24, 80);
        // base64("codex") = Y29kZXg=
        feed(
            &mut term,
            b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C;Y29kZXg=\x07",
        );
        assert_eq!(input_mode(&term), InputMode::LlmCompose);
        assert_eq!(effective_mode(&term, false), InputMode::LlmCompose);

        // 默认智能输入优先于备用屏；显式经典模式才会切到原生直通。
        feed(&mut term, b"\x1b[?1049h");
        assert_eq!(input_mode(&term), InputMode::LlmCompose);
        assert_eq!(effective_mode(&term, true), InputMode::LlmCli);

        // 命令闭合后不再误判为活动 LLM。
        feed(&mut term, b"\x1b]133;D;0\x07");
        assert_eq!(input_mode(&term), InputMode::AltScreen);
    }

    #[test]
    fn supported_llm_command_detector_accepts_verified_entry_points() {
        for command in [
            "claude",
            "Codex.EXE --resume",
            "kimi --continue",
            "kimi-cli",
            "& 'C:\\Program Files\\Claude\\claude.exe' --resume",
            "npx @openai/codex",
            "bunx @anthropic-ai/claude-code",
            "pnpm dlx @moonshot-ai/kimi-code",
            "npm exec -- @openai/codex",
            "python -m kimi_cli",
        ] {
            assert!(
                is_supported_llm_command(command),
                "应识别受支持的 LLM CLI：{command}"
            );
        }
    }

    #[test]
    fn supported_llm_command_detector_rejects_mentions_and_unknown_tools() {
        for command in [
            "",
            "echo codex",
            "rg claude",
            "python script.py kimi",
            "gemini",
            "opencode",
        ] {
            assert!(
                !is_supported_llm_command(command),
                "不应把未验证或普通参数误判为 LLM CLI：{command}"
            );
        }
    }

    // ── clear 后回到 Fallback 检验 ───────────────────────────────────────

    #[test]
    fn fallback_after_ris_reset() {
        let mut term = make_term(24, 80);
        // 先建立一个 Compose 态
        feed(&mut term, b"\x1b]133;A\x07\x1b]133;B\x07");
        assert_eq!(input_mode(&term), InputMode::Compose);
        // RIS 全重置（clear 命令走 Shell 侧发 \x1bc 或 ESC c）
        feed(&mut term, b"\x1bc");
        // 重置后 blocks 清空，回到 Fallback
        assert_eq!(
            input_mode(&term),
            InputMode::Fallback,
            "RIS 重置后应回到 Fallback"
        );
    }

    // ── 嵌套 shell（外层块停 Running）────────────────────────────────────

    #[test]
    fn running_when_nested_shell_no_integration() {
        // 嵌套裸 shell：外层已有 133;C（运行中），内层无 integration
        // 不发新 A/B，外层块持续 Running。
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b]133;A\x07");
        feed(&mut term, b"\x1b]133;B\x07");
        feed(&mut term, b"\x1b]133;C\x07");
        // 模拟嵌套 shell 的一些输出（无 OSC 133 标记）
        feed(&mut term, b"$ echo hi\r\nhi\r\n");
        assert_eq!(
            input_mode(&term),
            InputMode::Running,
            "嵌套 shell 无 integration 时外层块应维持 Running"
        );
    }

    // ── force_fallback 覆盖层 ────────────────────────────────────────────

    #[test]
    fn effective_mode_force_fallback_overrides_compose() {
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b]133;A\x07\x1b]133;B\x07");
        // 底层是 Compose，但 force_fallback=true 强制 Fallback
        assert_eq!(
            effective_mode(&term, true),
            InputMode::Fallback,
            "force_fallback=true 应覆盖 Compose 为 Fallback"
        );
    }

    #[test]
    fn effective_mode_no_force_returns_compose() {
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b]133;A\x07\x1b]133;B\x07");
        assert_eq!(
            effective_mode(&term, false),
            InputMode::Compose,
            "force_fallback=false 应正常返回 Compose"
        );
    }

    #[test]
    fn effective_mode_force_fallback_overrides_running() {
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        assert_eq!(
            effective_mode(&term, true),
            InputMode::Fallback,
            "force_fallback=true 应覆盖 Running 为 Fallback"
        );
    }

    #[test]
    fn effective_mode_llm_cli_keeps_image_paste_routing_in_classic_mode() {
        let mut term = make_term(24, 80);
        feed(
            &mut term,
            b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C;Y2xhdWRl\x07",
        );
        assert_eq!(
            effective_mode(&term, true),
            InputMode::LlmCli,
            "经典模式下仍需保留 LLM 图片粘贴专用路由"
        );
    }

    #[test]
    fn effective_mode_force_fallback_overrides_alt_screen() {
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b]133;A\x07\x1b]133;B\x07\x1b[?1049h");
        assert_eq!(
            effective_mode(&term, true),
            InputMode::Fallback,
            "force_fallback=true 应覆盖 AltScreen 为 Fallback"
        );
    }
}
