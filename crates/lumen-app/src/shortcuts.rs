//! 用户可配置的应用快捷键。
//!
//! 持久化格式刻意使用可读字符串（例如 `"Ctrl+Shift+D"`），便于用户
//! 手工检查 `settings.json`。运行时使用结构化值精确匹配修饰键，避免
//! `Ctrl+R` 误命中 `Ctrl+Shift+R` 一类组合。

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use winit::keyboard::{Key, ModifiersState, NamedKey as WinitNamedKey};

/// 可配置的快捷键动作。枚举顺序同时决定设置页展示顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutAction {
    NewTab,
    CloseTab,
    NextTab,
    PreviousTab,
    NewPane,
    ClosePane,
    ToggleMaximizePane,
    ToggleFiletree,
    ToggleSettings,
    ToggleClassicMode,
    PreviousBlock,
    NextBlock,
    SearchHistory,
    CopyOrInterrupt,
    Paste,
    AlternatePaste,
    ScrollUp,
    ScrollDown,
    CloseSettings,
}

impl ShortcutAction {
    pub const ALL: [Self; 19] = [
        Self::NewTab,
        Self::CloseTab,
        Self::NextTab,
        Self::PreviousTab,
        Self::NewPane,
        Self::ClosePane,
        Self::ToggleMaximizePane,
        Self::ToggleFiletree,
        Self::ToggleSettings,
        Self::ToggleClassicMode,
        Self::PreviousBlock,
        Self::NextBlock,
        Self::SearchHistory,
        Self::CopyOrInterrupt,
        Self::Paste,
        Self::AlternatePaste,
        Self::ScrollUp,
        Self::ScrollDown,
        Self::CloseSettings,
    ];

    pub const fn default_binding(self) -> ShortcutBinding {
        use ShortcutKey::{Character as Char, Named};
        use ShortcutNamedKey as NamedKey;

        match self {
            Self::NewTab => ShortcutBinding::ctrl(Char('t')),
            Self::CloseTab => ShortcutBinding::ctrl(Char('w')),
            Self::NextTab => ShortcutBinding::ctrl(Named(NamedKey::Tab)),
            Self::PreviousTab => ShortcutBinding::ctrl_shift(Named(NamedKey::Tab)),
            Self::NewPane => ShortcutBinding::ctrl_shift(Char('d')),
            Self::ClosePane => ShortcutBinding::ctrl_shift(Char('w')),
            Self::ToggleMaximizePane => ShortcutBinding::ctrl_shift(Named(NamedKey::Enter)),
            Self::ToggleFiletree => ShortcutBinding::ctrl(Char('b')),
            Self::ToggleSettings => ShortcutBinding::ctrl(Char(',')),
            Self::ToggleClassicMode => ShortcutBinding::ctrl_shift(Char('e')),
            Self::PreviousBlock => ShortcutBinding::ctrl(Named(NamedKey::ArrowUp)),
            Self::NextBlock => ShortcutBinding::ctrl(Named(NamedKey::ArrowDown)),
            Self::SearchHistory => ShortcutBinding::ctrl(Char('r')),
            Self::CopyOrInterrupt => ShortcutBinding::ctrl(Char('c')),
            Self::Paste => ShortcutBinding::ctrl(Char('v')),
            Self::AlternatePaste => ShortcutBinding::shift(Named(NamedKey::Insert)),
            Self::ScrollUp => ShortcutBinding::shift(Named(NamedKey::PageUp)),
            Self::ScrollDown => ShortcutBinding::shift(Named(NamedKey::PageDown)),
            Self::CloseSettings => ShortcutBinding::plain(Named(NamedKey::Escape)),
        }
    }
}

/// 快捷键中的具名键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShortcutNamedKey {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Space,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

impl ShortcutNamedKey {
    const fn label(self) -> &'static str {
        match self {
            Self::Enter => "Enter",
            Self::Tab => "Tab",
            Self::Escape => "Esc",
            Self::Backspace => "Backspace",
            Self::Delete => "Delete",
            Self::Insert => "Insert",
            Self::Home => "Home",
            Self::End => "End",
            Self::PageUp => "PgUp",
            Self::PageDown => "PgDn",
            Self::ArrowUp => "↑",
            Self::ArrowDown => "↓",
            Self::ArrowLeft => "←",
            Self::ArrowRight => "→",
            Self::Space => "Space",
            Self::F1 => "F1",
            Self::F2 => "F2",
            Self::F3 => "F3",
            Self::F4 => "F4",
            Self::F5 => "F5",
            Self::F6 => "F6",
            Self::F7 => "F7",
            Self::F8 => "F8",
            Self::F9 => "F9",
            Self::F10 => "F10",
            Self::F11 => "F11",
            Self::F12 => "F12",
        }
    }

    fn from_label(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "enter" | "return" => Some(Self::Enter),
            "tab" => Some(Self::Tab),
            "esc" | "escape" => Some(Self::Escape),
            "backspace" => Some(Self::Backspace),
            "delete" | "del" => Some(Self::Delete),
            "insert" | "ins" => Some(Self::Insert),
            "home" => Some(Self::Home),
            "end" => Some(Self::End),
            "pageup" | "pgup" => Some(Self::PageUp),
            "pagedown" | "pgdn" => Some(Self::PageDown),
            "up" | "arrowup" | "↑" => Some(Self::ArrowUp),
            "down" | "arrowdown" | "↓" => Some(Self::ArrowDown),
            "left" | "arrowleft" | "←" => Some(Self::ArrowLeft),
            "right" | "arrowright" | "→" => Some(Self::ArrowRight),
            "space" => Some(Self::Space),
            "f1" => Some(Self::F1),
            "f2" => Some(Self::F2),
            "f3" => Some(Self::F3),
            "f4" => Some(Self::F4),
            "f5" => Some(Self::F5),
            "f6" => Some(Self::F6),
            "f7" => Some(Self::F7),
            "f8" => Some(Self::F8),
            "f9" => Some(Self::F9),
            "f10" => Some(Self::F10),
            "f11" => Some(Self::F11),
            "f12" => Some(Self::F12),
            _ => None,
        }
    }

    fn matches_winit(self, key: &WinitNamedKey) -> bool {
        matches!(
            (self, key),
            (Self::Enter, WinitNamedKey::Enter)
                | (Self::Tab, WinitNamedKey::Tab)
                | (Self::Escape, WinitNamedKey::Escape)
                | (Self::Backspace, WinitNamedKey::Backspace)
                | (Self::Delete, WinitNamedKey::Delete)
                | (Self::Insert, WinitNamedKey::Insert)
                | (Self::Home, WinitNamedKey::Home)
                | (Self::End, WinitNamedKey::End)
                | (Self::PageUp, WinitNamedKey::PageUp)
                | (Self::PageDown, WinitNamedKey::PageDown)
                | (Self::ArrowUp, WinitNamedKey::ArrowUp)
                | (Self::ArrowDown, WinitNamedKey::ArrowDown)
                | (Self::ArrowLeft, WinitNamedKey::ArrowLeft)
                | (Self::ArrowRight, WinitNamedKey::ArrowRight)
                | (Self::Space, WinitNamedKey::Space)
                | (Self::F1, WinitNamedKey::F1)
                | (Self::F2, WinitNamedKey::F2)
                | (Self::F3, WinitNamedKey::F3)
                | (Self::F4, WinitNamedKey::F4)
                | (Self::F5, WinitNamedKey::F5)
                | (Self::F6, WinitNamedKey::F6)
                | (Self::F7, WinitNamedKey::F7)
                | (Self::F8, WinitNamedKey::F8)
                | (Self::F9, WinitNamedKey::F9)
                | (Self::F10, WinitNamedKey::F10)
                | (Self::F11, WinitNamedKey::F11)
                | (Self::F12, WinitNamedKey::F12)
        )
    }
}

/// 快捷键主键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShortcutKey {
    Character(char),
    Named(ShortcutNamedKey),
}

impl ShortcutKey {
    fn parse(raw: &str) -> Option<Self> {
        if let Some(named) = ShortcutNamedKey::from_label(raw) {
            return Some(Self::Named(named));
        }
        let punctuation = match raw.to_ascii_lowercase().as_str() {
            "comma" => Some(','),
            "period" => Some('.'),
            "slash" => Some('/'),
            "backslash" => Some('\\'),
            "semicolon" => Some(';'),
            "quote" => Some('\''),
            "minus" => Some('-'),
            "equals" => Some('='),
            "backtick" => Some('`'),
            "openbracket" => Some('['),
            "closebracket" => Some(']'),
            _ => None,
        };
        if let Some(ch) = punctuation {
            return Some(Self::Character(ch));
        }
        let mut chars = raw.chars();
        let ch = chars.next()?;
        if chars.next().is_none() && !ch.is_control() {
            Some(Self::Character(ch.to_ascii_lowercase()))
        } else {
            None
        }
    }

    fn from_egui(key: egui::Key) -> Option<Self> {
        Self::parse(key.name())
    }

    fn matches_winit(self, key: &Key) -> bool {
        match (self, key) {
            (Self::Character(expected), Key::Character(actual)) => actual
                .chars()
                .next()
                .is_some_and(|ch| ch.eq_ignore_ascii_case(&expected)),
            (Self::Named(expected), Key::Named(actual)) => expected.matches_winit(actual),
            _ => false,
        }
    }
}

/// 一个精确快捷键组合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShortcutBinding {
    pub key: ShortcutKey,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub logo: bool,
}

impl ShortcutBinding {
    pub const fn plain(key: ShortcutKey) -> Self {
        Self {
            key,
            ctrl: false,
            shift: false,
            alt: false,
            logo: false,
        }
    }

    pub const fn ctrl(key: ShortcutKey) -> Self {
        Self {
            ctrl: true,
            ..Self::plain(key)
        }
    }

    pub const fn shift(key: ShortcutKey) -> Self {
        Self {
            shift: true,
            ..Self::plain(key)
        }
    }

    pub const fn ctrl_shift(key: ShortcutKey) -> Self {
        Self {
            ctrl: true,
            shift: true,
            ..Self::plain(key)
        }
    }

    /// 从 egui 键盘事件生成绑定。裸可打印字符会吞掉终端日常输入，
    /// 因此要求至少一个修饰键；导航键和功能键可单独使用。
    pub fn from_egui(key: egui::Key, modifiers: egui::Modifiers) -> Option<Self> {
        let key = ShortcutKey::from_egui(key)?;
        let binding = Self {
            key,
            ctrl: modifiers.ctrl,
            shift: modifiers.shift,
            alt: modifiers.alt,
            logo: modifiers.mac_cmd,
        };
        if matches!(key, ShortcutKey::Character(_))
            && !binding.ctrl
            && !binding.alt
            && !binding.logo
        {
            return None;
        }
        Some(binding)
    }

    pub fn matches_winit(self, key: &Key, mods: ModifiersState) -> bool {
        self.ctrl == mods.control_key()
            && self.shift == mods.shift_key()
            && self.alt == mods.alt_key()
            && self.logo == mods.super_key()
            && self.key.matches_winit(key)
    }

    pub fn to_egui(self) -> Option<egui::KeyboardShortcut> {
        let logical_key = match self.key {
            ShortcutKey::Character(ch) => egui::Key::from_name(&ch.to_string())?,
            ShortcutKey::Named(named) => egui::Key::from_name(match named {
                ShortcutNamedKey::Enter => "Enter",
                ShortcutNamedKey::Tab => "Tab",
                ShortcutNamedKey::Escape => "Escape",
                ShortcutNamedKey::Backspace => "Backspace",
                ShortcutNamedKey::Delete => "Delete",
                ShortcutNamedKey::Insert => "Insert",
                ShortcutNamedKey::Home => "Home",
                ShortcutNamedKey::End => "End",
                ShortcutNamedKey::PageUp => "PageUp",
                ShortcutNamedKey::PageDown => "PageDown",
                ShortcutNamedKey::ArrowUp => "Up",
                ShortcutNamedKey::ArrowDown => "Down",
                ShortcutNamedKey::ArrowLeft => "Left",
                ShortcutNamedKey::ArrowRight => "Right",
                ShortcutNamedKey::Space => "Space",
                ShortcutNamedKey::F1 => "F1",
                ShortcutNamedKey::F2 => "F2",
                ShortcutNamedKey::F3 => "F3",
                ShortcutNamedKey::F4 => "F4",
                ShortcutNamedKey::F5 => "F5",
                ShortcutNamedKey::F6 => "F6",
                ShortcutNamedKey::F7 => "F7",
                ShortcutNamedKey::F8 => "F8",
                ShortcutNamedKey::F9 => "F9",
                ShortcutNamedKey::F10 => "F10",
                ShortcutNamedKey::F11 => "F11",
                ShortcutNamedKey::F12 => "F12",
            })?,
        };
        let modifiers = egui::Modifiers {
            alt: self.alt,
            ctrl: self.ctrl,
            shift: self.shift,
            mac_cmd: self.logo,
            command: self.ctrl || self.logo,
        };
        Some(egui::KeyboardShortcut::new(modifiers, logical_key))
    }
}

impl fmt::Display for ShortcutBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("Ctrl+")?;
        }
        if self.shift {
            f.write_str("Shift+")?;
        }
        if self.alt {
            f.write_str("Alt+")?;
        }
        if self.logo {
            f.write_str("Super+")?;
        }
        match self.key {
            ShortcutKey::Character(ch) => write!(f, "{}", ch.to_ascii_uppercase()),
            ShortcutKey::Named(named) => f.write_str(named.label()),
        }
    }
}

impl FromStr for ShortcutBinding {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("快捷键为空".to_owned());
        }
        let parts: Vec<&str> = raw.split('+').map(str::trim).collect();
        let (key_raw, modifiers) = parts
            .split_last()
            .ok_or_else(|| "快捷键缺少主键".to_owned())?;
        let key =
            ShortcutKey::parse(key_raw).ok_or_else(|| format!("不支持的快捷键主键：{key_raw}"))?;
        let mut binding = Self::plain(key);
        for modifier in modifiers {
            match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => binding.ctrl = true,
                "shift" => binding.shift = true,
                "alt" | "option" => binding.alt = true,
                "super" | "win" | "cmd" | "command" => binding.logo = true,
                _ => return Err(format!("不支持的修饰键：{modifier}")),
            }
        }
        if matches!(key, ShortcutKey::Character(_))
            && !binding.ctrl
            && !binding.alt
            && !binding.logo
        {
            return Err("可打印字符必须搭配 Ctrl、Alt 或 Super".to_owned());
        }
        Ok(binding)
    }
}

impl Serialize for ShortcutBinding {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ShortcutBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// 全部用户快捷键。缺少的新动作自动补默认值，旧配置可平滑升级。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyboardShortcuts {
    bindings: BTreeMap<ShortcutAction, ShortcutBinding>,
}

impl Default for KeyboardShortcuts {
    fn default() -> Self {
        let bindings = ShortcutAction::ALL
            .into_iter()
            .map(|action| (action, action.default_binding()))
            .collect();
        Self { bindings }
    }
}

impl KeyboardShortcuts {
    pub fn get(&self, action: ShortcutAction) -> ShortcutBinding {
        self.bindings
            .get(&action)
            .copied()
            .unwrap_or_else(|| action.default_binding())
    }

    pub fn set(&mut self, action: ShortcutAction, binding: ShortcutBinding) {
        self.bindings.insert(action, binding);
    }

    pub fn reset(&mut self, action: ShortcutAction) {
        self.set(action, action.default_binding());
    }

    pub fn reset_all(&mut self) {
        *self = Self::default();
    }

    pub fn matches(&self, action: ShortcutAction, key: &Key, modifiers: ModifiersState) -> bool {
        self.get(action).matches_winit(key, modifiers)
    }

    pub fn conflict(
        &self,
        action: ShortcutAction,
        binding: ShortcutBinding,
    ) -> Option<ShortcutAction> {
        ShortcutAction::ALL
            .into_iter()
            .find(|other| *other != action && self.get(*other) == binding)
    }

    /// 旧配置缺少后来新增的动作时补默认值。
    pub fn fill_missing_defaults(&mut self) {
        for action in ShortcutAction::ALL {
            self.bindings
                .entry(action)
                .or_insert_with(|| action.default_binding());
        }
    }

    pub fn has_conflicts(&self) -> bool {
        ShortcutAction::ALL
            .iter()
            .enumerate()
            .any(|(index, action)| {
                ShortcutAction::ALL[index + 1..]
                    .iter()
                    .any(|other| self.get(*action) == self.get(*other))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_round_trip_uses_readable_string() {
        let binding = ShortcutAction::SearchHistory.default_binding();
        let json = serde_json::to_string(&binding).unwrap();
        assert_eq!(json, "\"Ctrl+R\"");
        assert_eq!(
            serde_json::from_str::<ShortcutBinding>(&json).unwrap(),
            binding
        );
    }

    #[test]
    fn defaults_are_unique() {
        let shortcuts = KeyboardShortcuts::default();
        for action in ShortcutAction::ALL {
            assert_eq!(shortcuts.conflict(action, shortcuts.get(action)), None);
        }
    }

    #[test]
    fn printable_key_requires_modifier() {
        assert!("R".parse::<ShortcutBinding>().is_err());
        assert!("Ctrl+R".parse::<ShortcutBinding>().is_ok());
        assert!("Esc".parse::<ShortcutBinding>().is_ok());
    }

    #[test]
    fn conflict_reports_the_existing_action() {
        let shortcuts = KeyboardShortcuts::default();
        let binding = shortcuts.get(ShortcutAction::NewTab);
        assert_eq!(
            shortcuts.conflict(ShortcutAction::SearchHistory, binding),
            Some(ShortcutAction::NewTab)
        );
    }
}
