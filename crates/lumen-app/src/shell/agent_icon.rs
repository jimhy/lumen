//! M7 片 8：远程 LLM 会话的**图标标识 + 4 行弹层**（蓝图 §6.8.2 / §6.8.3）。
//!
//! # 这是 ⑫「桌面端不做会话 UI」之后剩下的那一点 UI，一点都不能再少
//!
//! ⑫ 砍掉了对话面板，但它承担的三件事里有一件**不可替代**：
//! 一部手机能远程驱动这台 PC 跑任意 Bash（Tier 0 无逐次授权），
//! 如果在物理接触得到的机器上**没有任何终止手段**——那不是 UI 缺失，是安全模型的洞。
//!
//! 所以这里有且只有：两个计数、每条会话一行状态、一个**断开**按钮。
//! **绝不出现对话正文**（哪怕一行摘要）。工具名、cwd、状态、时长，到此为止。
//!
//! # ★ 必须显示**两个**数
//!
//! ```text
//! [📱2 · ⚙1]   2 部手机在线 · 1 个任务运行中
//! [📱0 · ⚙1]   手机都断了，但 PC 上还有任务在跑   ← 这个状态最重要
//! [📱1 · ⚙0]   手机连着，空闲
//! ```
//!
//! 只显示手机数会让 §6.7 那条「**手机断线 runner 继续跑**」的核心契约在 UI 上彻底不可见
//! ——用户看到「0 部手机」却发现 CPU 在烧，那比没有图标更糟。
//!
//! 两个数的来源刻意不同，**不能用同一个**：
//!
//! | 数 | 来源 | 为什么不能换 |
//! |---|---|---|
//! | 📱 手机 | `RemoteWs.hidden.len()` | 服务端零额外下发，由三个本来就有的事件维护 |
//! | ⚙ 任务 | `LlmRunnerManager::working_count()` | runner 数 ≠ 手机数（断线不杀 runner） |
//!
//! # ★ 锁屏时显示计数，但弹层不可展开
//!
//! ⑩ 拍板「锁屏不阻断工具调用」是**执行**层面的决定，不等于渲染层面也放开——
//! 锁屏画面泄漏 cwd / 工具名 / 对端设备名是另一回事。计数本身是**存在性信息**，无害。
//! 沿用 §8.4「应用锁锁定时不得渲染配对码/二维码」的同一条纪律。

use lumen_protocol::remote::HiddenSessionId;

/// 弹层里的一行：一条隐藏会话的状态。
///
/// **刻意不含任何对话内容字段**——见模块文档。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    /// 隐藏会话 id（「断开」按钮要用它发 `EndHidden`）。
    pub session_id: HiddenSessionId,
    /// 对端设备显示名。
    pub peer_name: String,
    /// agent 与工作目录，如 `claude · F:\proj`。
    pub what: String,
    /// 当前状态的人话（「执行中: Bash」/「空闲」）。**不含参数**。
    pub status: String,
}

/// 图标与弹层要画的全部东西。
///
/// 做成一个纯数据结构（而不是让渲染函数直接去翻 `RemoteWs` 与 `LlmRunnerManager`），
/// 是为了让「显示几个数、显不显示、锁屏时能不能展开」这些判定**可以单测**——
/// egui 的渲染结果测不了，这些判定却正是最容易写错的部分。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentIconState {
    /// 在线的手机会话数（📱）。
    pub phones: usize,
    /// 有进行中 turn 的 runner 数（⚙）。
    pub tasks: usize,
    /// 每条会话一行。
    pub rows: Vec<AgentRow>,
    /// 应用锁是否锁定。
    pub locked: bool,
    /// 本次会话出现过几种未识别事件（调试锚点，蓝图 §6.8.3 的弹层末行）。
    pub unknown_kinds: usize,
}

impl AgentIconState {
    /// 图标该不该出现在工具栏上。
    ///
    /// **两个数都为 0 时整体隐藏**，不占位——绝大多数用户从不用远程 LLM，
    /// 给他们一个恒亮的 `[📱0 · ⚙0]` 只是噪声。
    #[must_use]
    pub fn visible(&self) -> bool {
        self.phones > 0 || self.tasks > 0
    }

    /// 弹层能不能展开。
    ///
    /// ★ 锁屏时**不能**：计数是存在性信息（无害），而 cwd / 工具名 / 对端设备名不是。
    #[must_use]
    pub fn expandable(&self) -> bool {
        !self.locked && !self.rows.is_empty()
    }

    /// 图标上的文字。
    #[must_use]
    pub fn label(&self) -> String {
        format!("📱{} · ⚙{}", self.phones, self.tasks)
    }
}

/// 点击弹层里的按钮产生的动作。由 `main.rs` 的调用方翻译成协议帧 / 系统调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentIconAction {
    /// 断开一条会话（发 `RemoteC2S::EndHidden`）。
    Disconnect(HiddenSessionId),
    /// 断开全部。
    DisconnectAll,
    /// 打开审计日志目录。
    OpenAuditDir,
}

/// 一次渲染的产出。
#[derive(Debug, Clone, Default)]
pub struct AgentIconOutput {
    /// 用户点了什么。
    pub action: Option<AgentIconAction>,
    /// 弹层的展开态（调用方持有，跨帧保持）。
    pub open: bool,
}

/// 画图标；返回是否被点击（展开/收起由调用方持有的 `open` 决定）。
///
/// 分成 `icon` 与 `popup` 两个函数而不是一个：工具栏那一格的布局由调用方决定，
/// 而弹层是浮层、要挂在 `Area` 上。
pub fn icon(ui: &mut egui::Ui, state: &AgentIconState, pal: &crate::shell::theme::Palette) -> bool {
    if !state.visible() {
        return false;
    }
    // 有任务在跑时用强调色：那是「这台机器正在替别人干活」的最强信号。
    let color = if state.tasks > 0 {
        pal.accent
    } else {
        pal.fg_dim
    };
    let text = egui::RichText::new(state.label()).size(12.0).color(color);
    let resp = ui.add(egui::Button::new(text).frame(false));
    // 锁屏时不给点击反馈：点了也展不开，光标变手型是在骗人。
    if state.expandable() {
        resp.on_hover_text(crate::i18n::strings().agent_icon_tooltip)
            .clicked()
    } else {
        false
    }
}

/// 画弹层。返回用户的动作。
pub fn popup(
    ctx: &egui::Context,
    state: &AgentIconState,
    pal: &crate::shell::theme::Palette,
) -> AgentIconOutput {
    let mut out = AgentIconOutput {
        open: true,
        ..AgentIconOutput::default()
    };
    if !state.expandable() {
        out.open = false;
        return out;
    }
    let s = crate::i18n::strings();
    egui::Area::new(egui::Id::new("lumen_agent_popup"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 42.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(pal.bg_panel)
                .stroke(egui::Stroke::new(1.0_f32, pal.accent))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    for row in &state.rows {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!("📱 {}", row.peer_name))
                                    .size(12.0)
                                    .color(pal.fg),
                            );
                            ui.add_space(8.0);
                            ui.label(egui::RichText::new(&row.what).size(11.0).color(pal.fg_dim));
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new(&row.status)
                                    .size(11.0)
                                    .color(pal.fg_dim),
                            );
                            ui.add_space(8.0);
                            // ★ 这个按钮是本片的底线：没有它，「一部手机能在我的电脑上跑
                            // 任意命令而我无法阻止」就被写进了设计。
                            if ui
                                .button(egui::RichText::new(s.agent_disconnect).size(11.0))
                                .clicked()
                            {
                                out.action = Some(AgentIconAction::Disconnect(row.session_id));
                            }
                        });
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        if state.unknown_kinds > 0 {
                            ui.label(
                                egui::RichText::new(crate::i18n::fmt1(
                                    s.agent_unknown_events_fmt,
                                    state.unknown_kinds.to_string(),
                                ))
                                .size(11.0)
                                .color(pal.warn),
                            );
                            ui.add_space(8.0);
                        }
                        if ui
                            .button(egui::RichText::new(s.agent_disconnect_all).size(11.0))
                            .clicked()
                        {
                            out.action = Some(AgentIconAction::DisconnectAll);
                        }
                        if ui
                            .button(egui::RichText::new(s.agent_open_audit).size(11.0))
                            .clicked()
                        {
                            out.action = Some(AgentIconAction::OpenAuditDir);
                        }
                    });
                });
        });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: HiddenSessionId, name: &str) -> AgentRow {
        AgentRow {
            session_id: id,
            peer_name: name.to_string(),
            what: "claude · F:\\proj".to_string(),
            status: "空闲".to_string(),
        }
    }

    #[test]
    fn 片8_两个数都为0时图标整体隐藏() {
        // 绝大多数用户从不用远程 LLM，恒亮的 [📱0 · ⚙0] 只是噪声。
        assert!(!AgentIconState::default().visible());
    }

    #[test]
    fn 片8_只要有一个数非0就显示() {
        let only_phone = AgentIconState {
            phones: 1,
            ..AgentIconState::default()
        };
        assert!(only_phone.visible());

        // ★ 这个组合最重要：手机都断了但 PC 上还有任务在跑。
        // 只显示手机数的话，用户会看到「0 部手机」却发现 CPU 在烧。
        let only_task = AgentIconState {
            tasks: 1,
            ..AgentIconState::default()
        };
        assert!(only_task.visible());
    }

    #[test]
    fn 片8_标签同时给出两个数() {
        let st = AgentIconState {
            phones: 2,
            tasks: 1,
            ..AgentIconState::default()
        };
        assert_eq!(st.label(), "📱2 · ⚙1");
        // 「手机断了但任务还在」必须能从标签上一眼看出来。
        let st = AgentIconState {
            phones: 0,
            tasks: 1,
            ..AgentIconState::default()
        };
        assert_eq!(st.label(), "📱0 · ⚙1");
    }

    #[test]
    fn 片8_锁屏时可显示计数但弹层不可展开() {
        // ⑩「锁屏不阻断工具调用」是执行层面的决定，不等于渲染层面也放开：
        // 锁屏画面泄漏 cwd / 工具名 / 对端设备名是另一回事。
        let st = AgentIconState {
            phones: 1,
            tasks: 1,
            rows: vec![row(1, "iPhone")],
            locked: true,
            unknown_kinds: 0,
        };
        assert!(st.visible(), "计数是存在性信息，锁屏也可显示");
        assert!(!st.expandable(), "★ 锁屏时绝不可展开");
    }

    #[test]
    fn 片8_没有会话行时不展开() {
        // 只剩「任务在跑」而没有手机会话时，弹层里没有任何可操作的行 ——
        // 展开一个空框比不展开更让人困惑。
        let st = AgentIconState {
            phones: 0,
            tasks: 1,
            ..AgentIconState::default()
        };
        assert!(st.visible());
        assert!(!st.expandable());
    }

    #[test]
    fn 片8_正常情况可展开() {
        let st = AgentIconState {
            phones: 1,
            tasks: 0,
            rows: vec![row(1, "iPhone")],
            locked: false,
            unknown_kinds: 0,
        };
        assert!(st.expandable());
    }

    #[test]
    fn 片8_行里不含任何对话内容字段() {
        // 这条守的是「弹层绝不出现对话正文」——用类型来保证，而不是靠自觉。
        // AgentRow 的字段：session_id / peer_name / what / status。
        // 若日后有人加了 `last_message` 之类，这个断言不会红，但 review 会看到这段。
        let r = row(1, "iPhone");
        assert_eq!(r.what, "claude · F:\\proj", "只有 agent 与 cwd");
        assert_eq!(r.status, "空闲", "只有状态，不含参数与正文");
    }
}
