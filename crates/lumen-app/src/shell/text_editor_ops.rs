//! 内置文本编辑器的纯文本编辑计算。
//!
//! 这里不依赖 egui：所有函数只接收文本与字符下标，输出
//! 「替换区间 + 插入文本 + 事后选区」的编辑计划（[`EditPlan`]）。
//! `text_editor` 把计划翻译成一条 Paste 事件注入 egui TextEdit，
//! 保证每个操作都是一步可整体撤销的编辑。

use std::ops::Range;

/// 搜索类功能收集的匹配上限，防止病态文本（如全文都是匹配串）拖垮 UI。
pub(super) const MAX_SEARCH_MATCHES: usize = 50_000;

/// 一次可整体注入的编辑计划（全部为字符下标）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EditPlan {
    /// 被替换的字符区间。
    pub replace_chars: Range<usize>,
    /// 替换后的文本。
    pub insertion: String,
    /// 注入完成后的选区（可折叠为光标）。
    pub selection_after: Range<usize>,
}

// ── 基础换算 ─────────────────────────────────────────────

pub(super) fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map_or(text.len(), |(byte, _)| byte)
}

pub(super) fn char_at(text: &str, char_idx: usize) -> Option<char> {
    text.chars().nth(char_idx)
}

pub(super) fn slice_chars(text: &str, range: Range<usize>) -> &str {
    &text[char_to_byte(text, range.start)..char_to_byte(text, range.end)]
}

// ── 行定位 ───────────────────────────────────────────────

/// 文本行数（空文本按 1 行计）。
pub(super) fn line_count(text: &str) -> usize {
    text.lines().count().max(1)
}

/// 第 `line` 行（0 起）第一个字符的字符下标；超出时返回文本总长。
pub(super) fn line_start_char(text: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut current = 0;
    for (idx, ch) in text.chars().enumerate() {
        if ch == '\n' {
            current += 1;
            if current == line {
                return idx + 1;
            }
        }
    }
    char_count(text)
}

/// `char_idx` 所在行号（0 起）。
pub(super) fn line_index_at(text: &str, char_idx: usize) -> usize {
    text.chars().take(char_idx).filter(|ch| *ch == '\n').count()
}

/// 第 `line` 行的字符区间（不含换行符）。
pub(super) fn line_range(text: &str, line: usize) -> Range<usize> {
    let start = line_start_char(text, line);
    let start_byte = char_to_byte(text, start);
    let end_byte = text[start_byte..]
        .find('\n')
        .map_or(text.len(), |offset| start_byte + offset);
    start..start + text[start_byte..end_byte].chars().count()
}

/// 选区覆盖的完整行段 (首行, 末行)（含端点）。
/// 选区末端恰好落在某行行首时，该行不算被覆盖（与 VSCode 一致）。
pub(super) fn selection_line_span(text: &str, sel: &Range<usize>) -> (usize, usize) {
    let first = line_index_at(text, sel.start);
    let mut last = line_index_at(text, sel.end);
    if sel.end > sel.start && sel.end > 0 && char_at(text, sel.end - 1) == Some('\n') {
        last = last.saturating_sub(1);
    }
    (first, last.max(first))
}

/// 1 起的行号换算成 0 起行首字符下标，越界时夹紧到最后一行。
pub(super) fn goto_line_start(text: &str, line_1based: usize) -> usize {
    let line = line_1based.saturating_sub(1).min(line_count(text) - 1);
    line_start_char(text, line)
}

// ── 选区边界调整 ─────────────────────────────────────────

/// 一组 (位置, 删除长度, 插入长度) 编辑对某个原选区边界的平移。
/// 位置按原文字符下标给出，必须升序。
fn adjust_boundary(boundary: usize, edits: &[(usize, usize, usize)]) -> usize {
    let mut adjusted = boundary;
    for &(pos, remove, insert) in edits {
        // 编辑位置是原文件坐标，比较基准必须是最初的边界值。
        if pos > boundary {
            continue;
        }
        if remove == 0 {
            // 纯插入：插在边界处或之前都把边界往后推。
            adjusted += insert;
        } else if pos + remove <= boundary {
            adjusted = adjusted + insert - remove;
        } else if pos < boundary {
            // 边界落在被删区间内：夹到删除点。
            adjusted = pos + insert;
        }
        // pos == adjusted 的删除：被删内容在边界之后，边界不动。
    }
    adjusted
}

/// 把逐行编辑应用到行段上，返回新段文本与累计编辑列表。
fn rebuild_lines(
    text: &str,
    first: usize,
    last: usize,
    mut edit_line: impl FnMut(usize, &str) -> (String, Vec<(usize, usize, usize)>),
) -> (String, Vec<(usize, usize, usize)>) {
    let mut segment = String::new();
    let mut edits = Vec::new();
    for line in first..=last {
        if line > first {
            segment.push('\n');
        }
        let range = line_range(text, line);
        let line_text = slice_chars(text, range.clone());
        let (new_line, line_edits) = edit_line(range.start, line_text);
        segment.push_str(&new_line);
        edits.extend(line_edits);
    }
    (segment, edits)
}

// ── 注释切换 ─────────────────────────────────────────────

/// 注释风格：行注释前缀，或块注释包裹符号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CommentStyle {
    Line(&'static str),
    Block(&'static str, &'static str),
}

/// Ctrl+/ 注释切换：行段内所有非空行都已注释时去注释，否则加注释。
/// 空选区时把光标移到下一行行首（与 VSCode 一致）。
pub(super) fn toggle_comment(
    text: &str,
    sel: &Range<usize>,
    style: CommentStyle,
) -> Option<EditPlan> {
    let (first, last) = selection_line_span(text, sel);
    let (open, close) = match style {
        CommentStyle::Line(token) => (token, ""),
        CommentStyle::Block(open, close) => (open, close),
    };

    let mut any_content = false;
    let mut all_commented = true;
    for line in first..=last {
        let range = line_range(text, line);
        let line_text = slice_chars(text, range);
        let indent_len = leading_ws_chars(line_text);
        let content = &line_text[char_to_byte(line_text, indent_len)..];
        if content.trim().is_empty() {
            continue;
        }
        any_content = true;
        let commented = if close.is_empty() {
            content.starts_with(open)
        } else {
            content.starts_with(open) && content.trim_end().ends_with(close)
        };
        if !commented {
            all_commented = false;
            break;
        }
    }
    if !any_content {
        return None;
    }

    let (segment, edits) = rebuild_lines(text, first, last, |line_start, line_text| {
        let indent_len = leading_ws_chars(line_text);
        let indent_pos = line_start + indent_len;
        let indent: String = line_text.chars().take(indent_len).collect();
        let content = &line_text[char_to_byte(line_text, indent_len)..];
        if content.trim().is_empty() {
            return (line_text.to_owned(), Vec::new());
        }
        if !all_commented {
            // 加注释：open + 空格插在缩进后；块注释把 close 加到行尾空白前。
            let mut edits = vec![(indent_pos, 0, open.len() + 1)];
            if close.is_empty() {
                return (format!("{indent}{open} {content}"), edits);
            }
            let trimmed = content.trim_end();
            let trailing = &content[trimmed.len()..];
            let insert_pos = indent_pos + trimmed.chars().count();
            edits.push((insert_pos, 0, close.len() + 1));
            return (format!("{indent}{open} {trimmed} {close}{trailing}"), edits);
        }
        // 去注释：删 open（及其后一个空格）；块注释同时删 close（及其前一个空格）。
        let after_open = &content[open.len()..];
        let strip = usize::from(after_open.starts_with(' '));
        let mut new_content = after_open[strip..].to_owned();
        let mut edits = vec![(indent_pos, open.len() + strip, 0)];
        if !close.is_empty() {
            let trailing_ws = new_content.len() - new_content.trim_end().len();
            let body_end = new_content.len() - trailing_ws;
            if new_content[..body_end].ends_with(close) {
                let close_start = body_end - close.len();
                let space =
                    usize::from(close_start > 0 && new_content[..close_start].ends_with(' '));
                let remove_chars = close.chars().count() + space;
                // close 在原行中的位置：行尾空白之前。
                let line_chars = line_text.chars().count();
                let trailing_chars = new_content[body_end..].chars().count();
                let orig_close_start =
                    line_start + line_chars - trailing_chars - close.chars().count();
                new_content.replace_range(close_start - space..body_end, "");
                edits.push((orig_close_start - space, remove_chars, 0));
            }
        }
        (format!("{indent}{new_content}"), edits)
    });

    let replace_chars = line_start_char(text, first)..line_range(text, last).end;
    let selection_after = if sel.is_empty() {
        let next = line_start_char(text, last + 1);
        let cursor = adjust_boundary(next, &edits);
        cursor..cursor
    } else {
        adjust_boundary(sel.start, &edits)..adjust_boundary(sel.end, &edits)
    };
    Some(EditPlan {
        replace_chars,
        insertion: segment,
        selection_after,
    })
}

fn leading_ws_chars(line: &str) -> usize {
    line.chars()
        .take_while(|ch| matches!(ch, ' ' | '\t'))
        .count()
}

// ── 块缩进 / 反缩进 ──────────────────────────────────────

/// Tab / Shift+Tab：对选区覆盖的每一行加/去一级缩进。
/// 加缩进跳过空行；反缩进去掉至多一级（tab 一个，空格至多 `unit` 宽）。
pub(super) fn indent_lines(
    text: &str,
    sel: &Range<usize>,
    unit: &str,
    dedent: bool,
) -> Option<EditPlan> {
    let (first, last) = selection_line_span(text, sel);
    // tab 缩进时反缩进按 4 列空格处理。
    let unit_cols = if unit == "\t" {
        4
    } else {
        unit.chars().count().max(1)
    };
    let mut touched = false;
    let (segment, edits) = rebuild_lines(text, first, last, |line_start, line_text| {
        if !dedent {
            if line_text.is_empty() {
                return (line_text.to_owned(), Vec::new());
            }
            touched = true;
            let mut new_line = String::with_capacity(line_text.len() + unit.len());
            new_line.push_str(unit);
            new_line.push_str(line_text);
            (new_line, vec![(line_start, 0, unit.chars().count())])
        } else {
            let remove = if line_text.starts_with('\t') {
                1
            } else {
                let spaces = line_text.chars().take_while(|ch| *ch == ' ').count();
                spaces.min(unit_cols)
            };
            if remove == 0 {
                return (line_text.to_owned(), Vec::new());
            }
            touched = true;
            let new_line: String = line_text.chars().skip(remove).collect();
            (new_line, vec![(line_start, remove, 0)])
        }
    });
    if !touched {
        return None;
    }
    let replace_chars = line_start_char(text, first)..line_range(text, last).end;
    let selection_after = if sel.is_empty() {
        let cursor = adjust_boundary(sel.start, &edits);
        cursor..cursor
    } else {
        adjust_boundary(sel.start, &edits)..adjust_boundary(sel.end, &edits)
    };
    Some(EditPlan {
        replace_chars,
        insertion: segment,
        selection_after,
    })
}

// ── 行移动 / 复制 / 删除 ─────────────────────────────────

/// Alt+↑/↓：把选区覆盖的行段整体上移或下移一行。
pub(super) fn move_lines(text: &str, sel: &Range<usize>, up: bool) -> Option<EditPlan> {
    let (first, last) = selection_line_span(text, sel);
    if up {
        if first == 0 {
            return None;
        }
        let prev = line_range(text, first - 1);
        let block_end = line_range(text, last).end;
        let prev_text = slice_chars(text, prev.clone());
        let block_text = slice_chars(text, line_start_char(text, first)..block_end);
        let insertion = format!("{block_text}\n{prev_text}");
        let shift = prev_text.chars().count() + 1;
        Some(EditPlan {
            replace_chars: prev.start..block_end,
            insertion,
            selection_after: sel.start - shift..sel.end - shift,
        })
    } else {
        if last + 1 >= line_count(text) {
            return None;
        }
        let next = line_range(text, last + 1);
        let block_start = line_start_char(text, first);
        let block_text = slice_chars(text, block_start..line_range(text, last).end);
        let next_text = slice_chars(text, next.clone());
        let insertion = format!("{next_text}\n{block_text}");
        let shift = next_text.chars().count() + 1;
        Some(EditPlan {
            replace_chars: block_start..next.end,
            insertion,
            selection_after: sel.start + shift..sel.end + shift,
        })
    }
}

/// Shift+Alt+↓：复制当前行（空选区）或选中文本（非空选区）。
pub(super) fn duplicate(text: &str, sel: &Range<usize>) -> EditPlan {
    if sel.is_empty() {
        let line = line_index_at(text, sel.start);
        let range = line_range(text, line);
        let line_text = slice_chars(text, range.clone());
        let column = sel.start - range.start;
        let insertion = format!("{line_text}\n{line_text}");
        let cursor = range.start + line_text.chars().count() + 1 + column;
        EditPlan {
            replace_chars: range,
            insertion,
            selection_after: cursor..cursor,
        }
    } else {
        let selected = slice_chars(text, sel.clone());
        let len = sel.end - sel.start;
        EditPlan {
            replace_chars: sel.clone(),
            insertion: format!("{selected}{selected}"),
            selection_after: sel.end..sel.end + len,
        }
    }
}

/// Ctrl+Shift+K：删除选区覆盖的整行（含行尾换行）。
pub(super) fn delete_lines(text: &str, sel: &Range<usize>) -> EditPlan {
    let (first, last) = selection_line_span(text, sel);
    let total = char_count(text);
    let start = line_start_char(text, first);
    let next_start = line_start_char(text, last + 1);
    let (range, cursor) = if next_start < total {
        (start..next_start, start)
    } else if start > 0 {
        // 删到文件末尾：连同前一行的换行符一起删除。
        (start - 1..total, line_start_char(text, first - 1))
    } else {
        (0..total, 0)
    };
    EditPlan {
        replace_chars: range,
        insertion: String::new(),
        selection_after: cursor..cursor,
    }
}

// ── 自动闭合 ─────────────────────────────────────────────

/// 输入一个字符后的处理决策。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TypedCharDecision {
    /// 不干预，按原样插入。
    Plain,
    /// 插入成对符号，光标落在中间。
    Pair { open: char, close: char },
    /// 用成对符号包裹当前选区，原选区内容保持选中。
    Wrap { open: char, close: char },
    /// 不插入字符，光标右移一位越过已有的闭合符。
    SkipCloser,
}

fn matching_close(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '"' => Some('"'),
        '\'' => Some('\''),
        _ => None,
    }
}

fn is_word_char(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

/// 判定输入 `typed` 时是否自动闭合 / 包裹 / 跳过。
///
/// 引号规则避免散文误触发：光标前是单词字符（don't）或光标后是
/// 单词字符时不自动成对；闭合符右侧已有相同字符时只越过不插入。
pub(super) fn typed_char_decision(
    text: &str,
    sel: &Range<usize>,
    typed: char,
) -> TypedCharDecision {
    let close = matching_close(typed);
    let is_opener = matches!(typed, '(' | '[' | '{');
    let is_quote = matches!(typed, '"' | '\'');
    if !sel.is_empty() {
        return if is_opener || is_quote {
            TypedCharDecision::Wrap {
                open: typed,
                close: close.unwrap_or(typed),
            }
        } else {
            TypedCharDecision::Plain
        };
    }
    let cursor = sel.start;
    let next = char_at(text, cursor);
    if (matches!(typed, ')' | ']' | '}') || is_quote) && next == Some(typed) {
        return TypedCharDecision::SkipCloser;
    }
    if is_opener {
        let allowed = next.is_none_or(|ch| {
            ch.is_whitespace() || matches!(ch, ')' | ']' | '}' | ',' | '.' | ';' | ':' | '"' | '\'')
        });
        return if allowed {
            TypedCharDecision::Pair {
                open: typed,
                close: close.unwrap_or(typed),
            }
        } else {
            TypedCharDecision::Plain
        };
    }
    if is_quote {
        let prev_is_word = cursor > 0 && char_at(text, cursor - 1).is_some_and(is_word_char);
        let next_is_word = next.is_some_and(is_word_char);
        return if prev_is_word || next_is_word {
            TypedCharDecision::Plain
        } else {
            TypedCharDecision::Pair {
                open: typed,
                close: typed,
            }
        };
    }
    TypedCharDecision::Plain
}

/// 空选区 Backspace：光标夹在成对符号中间时返回整对的删除区间。
pub(super) fn backspace_pair_range(text: &str, sel: &Range<usize>) -> Option<Range<usize>> {
    if !sel.is_empty() || sel.start == 0 {
        return None;
    }
    let prev = char_at(text, sel.start - 1)?;
    let close = matching_close(prev)?;
    if char_at(text, sel.start) == Some(close) {
        Some(sel.start - 1..sel.start + 1)
    } else {
        None
    }
}

// ── 括号配对 ─────────────────────────────────────────────

/// 光标旁的括号配对：返回 (括号字符下标, 配对字符下标)；失配为 (括号, None)。
///
/// 只做同类型深度计数，不理解字符串/注释内的括号——嵌套字符串含
/// 括号时可能不精确（与着色器同源的近似），但永不 panic。
pub(super) fn matching_bracket(text: &str, cursor: usize) -> Option<(usize, Option<usize>)> {
    fn partner(ch: char) -> Option<(bool, char)> {
        match ch {
            '(' => Some((true, ')')),
            ')' => Some((false, '(')),
            '[' => Some((true, ']')),
            ']' => Some((false, '[')),
            '{' => Some((true, '}')),
            '}' => Some((false, '{')),
            _ => None,
        }
    }
    let before = cursor.checked_sub(1);
    let positions = [before, Some(cursor)];
    for pos in positions.into_iter().flatten() {
        let Some(ch) = char_at(text, pos) else {
            continue;
        };
        let Some((is_open, close)) = partner(ch) else {
            continue;
        };
        let mut depth = 1usize;
        if is_open {
            let from = char_to_byte(text, pos + 1);
            for (char_idx, (_, scanned)) in text[from..].char_indices().enumerate() {
                if scanned == ch {
                    depth += 1;
                } else if scanned == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some((pos, Some(pos + 1 + char_idx)));
                    }
                }
            }
        } else {
            let upto = char_to_byte(text, pos);
            for (byte, scanned) in text[..upto].char_indices().rev() {
                if scanned == ch {
                    depth += 1;
                } else if scanned == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some((pos, Some(text[..byte].chars().count())));
                    }
                }
            }
        }
        return Some((pos, None));
    }
    None
}

// ── 搜索 / 替换 ──────────────────────────────────────────

fn chars_eq(left: char, right: char, case_sensitive: bool) -> bool {
    left == right || (!case_sensitive && left.to_lowercase().eq(right.to_lowercase()))
}

fn search(
    text: &str,
    query: &str,
    case_sensitive: bool,
    word_char: Option<&dyn Fn(char) -> bool>,
) -> Vec<Range<usize>> {
    let query_chars: Vec<char> = query.chars().collect();
    let Some(&first_ch) = query_chars.first() else {
        return Vec::new();
    };
    let qlen = query_chars.len();
    let mut out = Vec::new();
    for (start, (byte, ch)) in text.char_indices().enumerate() {
        if !chars_eq(ch, first_ch, case_sensitive) {
            continue;
        }
        let mut matched = true;
        let mut iter = text[byte + ch.len_utf8()..].chars();
        for expected in &query_chars[1..] {
            match iter.next() {
                Some(actual) if chars_eq(actual, *expected, case_sensitive) => {}
                _ => {
                    matched = false;
                    break;
                }
            }
        }
        if !matched {
            continue;
        }
        if let Some(is_word_char) = word_char {
            let before_word =
                byte > 0 && text[..byte].chars().next_back().is_some_and(is_word_char);
            let after = text[byte..].chars().nth(qlen);
            if before_word || after.is_some_and(is_word_char) {
                continue;
            }
        }
        out.push(start..start + qlen);
        if out.len() >= MAX_SEARCH_MATCHES {
            break;
        }
    }
    out
}

/// 全文查找（子串，可重叠），返回字符区间（升序）。
pub(super) fn find_matches(text: &str, query: &str, case_sensitive: bool) -> Vec<Range<usize>> {
    search(text, query, case_sensitive, None)
}

/// 选中词出现位置：大小写敏感 + 两侧不得是词字符。
pub(super) fn word_matches(
    text: &str,
    word: &str,
    is_word_char: impl Fn(char) -> bool,
) -> Vec<Range<usize>> {
    search(text, word, true, Some(&is_word_char))
}

/// 把一组有序匹配整体替换为 `replacement`，一次完成（单条可撤销）。
/// 重叠匹配只替换最前者。
pub(super) fn replace_matches(text: &str, matches: &[Range<usize>], replacement: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut skip_until = 0usize;
    let mut iter = matches.iter();
    let mut next = iter.next();
    for (idx, (_, ch)) in text.char_indices().enumerate() {
        while let Some(m) = next {
            if m.start < skip_until {
                next = iter.next();
                continue;
            }
            if idx == m.start {
                out.push_str(replacement);
                skip_until = m.end;
                next = iter.next();
            }
            break;
        }
        if idx >= skip_until {
            out.push(ch);
        }
    }
    out
}

// ── 缩进参考线 ───────────────────────────────────────────

/// 行前导空白换算的列数（tab 对齐到 `unit_cols` 的倍数）。
pub(super) fn indent_columns(line: &str, unit_cols: usize) -> usize {
    let unit = unit_cols.max(1);
    let mut cols = 0usize;
    for ch in line.chars() {
        match ch {
            ' ' => cols += 1,
            '\t' => cols += unit - (cols % unit),
            _ => break,
        }
    }
    cols
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str) -> Range<usize> {
        0..char_count(text)
    }

    fn apply(text: &str, plan: &EditPlan) -> String {
        let mut out = String::new();
        out.push_str(slice_chars(text, 0..plan.replace_chars.start));
        out.push_str(&plan.insertion);
        out.push_str(slice_chars(text, plan.replace_chars.end..char_count(text)));
        out
    }

    // ── 行定位 ──

    #[test]
    fn line_helpers_cover_edges() {
        let text = "ab\n\ncd\n";
        assert_eq!(line_count(text), 3);
        assert_eq!(line_start_char(text, 0), 0);
        assert_eq!(line_start_char(text, 1), 3);
        assert_eq!(line_start_char(text, 2), 4);
        assert_eq!(line_start_char(text, 3), 7);
        assert_eq!(line_range(text, 0), 0..2);
        assert_eq!(line_range(text, 1), 3..3);
        assert_eq!(line_range(text, 2), 4..6);
        assert_eq!(line_index_at(text, 3), 1);
        assert_eq!(line_index_at(text, 6), 2);
        assert_eq!(line_index_at("", 0), 0);
        assert_eq!(line_range("", 0), 0..0);
    }

    #[test]
    fn selection_span_excludes_line_start_tail() {
        let text = "aa\nbb\ncc";
        // 全选第一行（含换行）：只覆盖第 0 行。
        assert_eq!(selection_line_span(text, &(0..3)), (0, 0));
        // 选到第二行行中：覆盖 0..=1。
        assert_eq!(selection_line_span(text, &(0..4)), (0, 1));
        // 空选区：只有光标所在行。
        assert_eq!(selection_line_span(text, &(5..5)), (1, 1));
    }

    #[test]
    fn goto_line_clamps() {
        let text = "a\nbb\nccc";
        assert_eq!(goto_line_start(text, 0), 0);
        assert_eq!(goto_line_start(text, 2), 2);
        assert_eq!(goto_line_start(text, 3), 5);
        assert_eq!(goto_line_start(text, 99), 5);
    }

    // ── 注释切换 ──

    #[test]
    fn comment_adds_and_removes_line_tokens() {
        let text = "fn main() {\n    let a = 1;\n}";
        let plan = toggle_comment(text, &(12..21), CommentStyle::Line("//")).expect("plan");
        let new = apply(text, &plan);
        assert_eq!(new, "fn main() {\n    // let a = 1;\n}");
        let plan = toggle_comment(&new, &(12..25), CommentStyle::Line("//")).expect("plan");
        assert_eq!(apply(&new, &plan), text);
    }

    #[test]
    fn comment_mixed_selection_only_adds() {
        let text = "# a\nb\n# c";
        let plan = toggle_comment(text, &span(text), CommentStyle::Line("#")).expect("plan");
        assert_eq!(apply(text, &plan), "# # a\n# b\n# # c");
        // 全部已注释 → 去注释。
        let text = "# a\n# b";
        let plan = toggle_comment(text, &span(text), CommentStyle::Line("#")).expect("plan");
        assert_eq!(apply(text, &plan), "a\nb");
    }

    #[test]
    fn comment_skips_empty_lines_and_keeps_selected_content() {
        let text = "a\n\nb";
        let plan = toggle_comment(text, &span(text), CommentStyle::Line("#")).expect("plan");
        assert_eq!(apply(text, &plan), "# a\n\n# b");
        // 选区跟随原内容（注释符本身不被选中，与 VSCode 一致）。
        assert_eq!(plan.selection_after, 2..8);
    }

    #[test]
    fn comment_empty_selection_moves_to_next_line() {
        let text = "aa\nbb";
        let plan = toggle_comment(text, &(1..1), CommentStyle::Line("#")).expect("plan");
        assert_eq!(apply(text, &plan), "# aa\nbb");
        assert_eq!(plan.selection_after, 5..5, "光标移到下一行行首");
    }

    #[test]
    fn comment_block_wraps_per_line() {
        let text = "color: red;\n\nmargin: 0;";
        let style = CommentStyle::Block("/*", "*/");
        let plan = toggle_comment(text, &span(text), style).expect("plan");
        let new = apply(text, &plan);
        assert_eq!(new, "/* color: red; */\n\n/* margin: 0; */");
        let plan = toggle_comment(&new, &span(&new), style).expect("plan");
        assert_eq!(apply(&new, &plan), text);
    }

    #[test]
    fn comment_block_html_with_indent() {
        let text = "  <p>hi</p>  ";
        let style = CommentStyle::Block("<!--", "-->");
        let plan = toggle_comment(text, &span(text), style).expect("plan");
        let new = apply(text, &plan);
        assert_eq!(new, "  <!-- <p>hi</p> -->  ");
        let plan = toggle_comment(&new, &span(&new), style).expect("plan");
        assert_eq!(apply(&new, &plan), text);
    }

    #[test]
    fn comment_all_empty_returns_none() {
        assert!(toggle_comment("\n  \n", &span("\n  \n"), CommentStyle::Line("#")).is_none());
    }

    // ── 缩进 ──

    #[test]
    fn indent_and_dedent_selection() {
        let text = "a\nb";
        let plan = indent_lines(text, &span(text), "    ", false).expect("plan");
        let new = apply(text, &plan);
        assert_eq!(new, "    a\n    b");
        assert_eq!(plan.selection_after, 4..11);
        let plan = indent_lines(&new, &span(&new), "    ", true).expect("plan");
        assert_eq!(apply(&new, &plan), text);
    }

    #[test]
    fn indent_skips_empty_lines_but_dedent_strips_whitespace() {
        let text = "a\n\nb";
        let plan = indent_lines(text, &span(text), "  ", false).expect("plan");
        assert_eq!(apply(text, &plan), "  a\n\n  b");
        let text = "  a\n  \n  b";
        let plan = indent_lines(text, &span(text), "  ", true).expect("plan");
        assert_eq!(apply(text, &plan), "a\n\nb");
    }

    #[test]
    fn dedent_partial_and_tab() {
        let text = "   a";
        let plan = indent_lines(text, &span(text), "    ", true).expect("plan");
        assert_eq!(apply(text, &plan), "a", "不足一级时全部去掉");
        let text = "\t\ta";
        let plan = indent_lines(text, &span(text), "    ", true).expect("plan");
        assert_eq!(apply(text, &plan), "\ta");
    }

    // ── 行移动 / 复制 / 删除 ──

    #[test]
    fn move_lines_up_and_down() {
        let text = "a\nbb\nccc";
        let plan = move_lines(text, &(3..4), true).expect("up");
        let new = apply(text, &plan);
        assert_eq!(new, "bb\na\nccc");
        assert_eq!(plan.selection_after, 1..2, "选区跟随行移动");
        assert!(
            move_lines(&new, &(0..1), true).is_none(),
            "第一行不能再上移"
        );
        let plan = move_lines(&new, &(0..1), false).expect("down");
        assert_eq!(apply(&new, &plan), text);
        assert!(
            move_lines(text, &(5..8), false).is_none(),
            "最后一行不能再下移"
        );
    }

    #[test]
    fn move_block_and_last_line_up() {
        let text = "a\nbb\nccc";
        // 选 1..=2 行整体上移
        let plan = move_lines(text, &(2..8), true).expect("up");
        assert_eq!(apply(text, &plan), "bb\nccc\na");
        // 末行（无尾换行）上移
        let plan = move_lines(text, &(5..8), true).expect("up");
        assert_eq!(apply(text, &plan), "a\nccc\nbb");
    }

    #[test]
    fn duplicate_line_and_selection() {
        let text = "ab\ncd";
        let plan = duplicate(text, &(1..1));
        let new = apply(text, &plan);
        assert_eq!(new, "ab\nab\ncd");
        assert_eq!(plan.selection_after, 4..4);
        let plan = duplicate(text, &(0..2));
        let new = apply(text, &plan);
        assert_eq!(new, "abab\ncd");
        assert_eq!(plan.selection_after, 2..4);
    }

    #[test]
    fn delete_lines_covers_all_positions() {
        let text = "a\nbb\nccc";
        let plan = delete_lines(text, &(0..0));
        assert_eq!(apply(text, &plan), "bb\nccc");
        assert_eq!(plan.selection_after, 0..0);
        let plan = delete_lines(text, &(6..6));
        assert_eq!(apply(text, &plan), "a\nbb");
        let plan = delete_lines(text, &(2..4));
        assert_eq!(apply(text, &plan), "a\nccc");
        let plan = delete_lines("only", &(0..0));
        assert_eq!(apply("only", &plan), "");
    }

    // ── 自动闭合 ──

    #[test]
    fn auto_close_pairs_and_skips() {
        let text = "foo ";
        assert_eq!(
            typed_char_decision(text, &(4..4), '('),
            TypedCharDecision::Pair {
                open: '(',
                close: ')'
            }
        );
        // 右侧是单词字符：不成对。
        let text = "foo bar";
        assert_eq!(
            typed_char_decision(text, &(4..4), '('),
            TypedCharDecision::Plain
        );
        // 右侧已有闭合符：越过。
        let text = "foo )";
        assert_eq!(
            typed_char_decision(text, &(4..4), ')'),
            TypedCharDecision::SkipCloser
        );
        // 引号：前是单词 → 不成对；行尾空白处 → 成对。
        let text = "don";
        assert_eq!(
            typed_char_decision(text, &(3..3), '\''),
            TypedCharDecision::Plain
        );
        let text = "say ";
        assert_eq!(
            typed_char_decision(text, &(4..4), '"'),
            TypedCharDecision::Pair {
                open: '"',
                close: '"'
            }
        );
        let text = "\"\"";
        assert_eq!(
            typed_char_decision(text, &(1..1), '"'),
            TypedCharDecision::SkipCloser
        );
    }

    #[test]
    fn auto_close_wraps_selection() {
        let text = "foo";
        assert_eq!(
            typed_char_decision(text, &(0..3), '('),
            TypedCharDecision::Wrap {
                open: '(',
                close: ')'
            }
        );
        assert_eq!(
            typed_char_decision(text, &(0..3), ')'),
            TypedCharDecision::Plain
        );
    }

    #[test]
    fn backspace_deletes_empty_pair() {
        let text = "fn()";
        assert_eq!(backspace_pair_range(text, &(3..3)), Some(2..4));
        let text = "fn(a)";
        assert_eq!(backspace_pair_range(text, &(3..3)), None);
        let text = "\"\"";
        assert_eq!(backspace_pair_range(text, &(1..1)), Some(0..2));
        assert_eq!(backspace_pair_range("(", &(1..1)), None);
    }

    // ── 括号配对 ──

    #[test]
    fn bracket_matching_nested_and_unmatched() {
        let text = "a(b[c]d)e";
        assert_eq!(matching_bracket(text, 2), Some((1, Some(7))));
        // 光标在闭合符右侧：匹配闭合符本身。
        assert_eq!(matching_bracket(text, 8), Some((7, Some(1))));
        assert_eq!(matching_bracket(text, 5), Some((5, Some(3))));
        let text = "a(b";
        assert_eq!(matching_bracket(text, 2), Some((1, None)));
        let text = "abc";
        assert_eq!(matching_bracket(text, 1), None);
    }

    // ── 搜索 / 替换 ──

    #[test]
    fn find_matches_case_and_positions() {
        let text = "Foo foo FOO bar";
        assert_eq!(find_matches(text, "foo", true), vec![4..7]);
        assert_eq!(find_matches(text, "foo", false), vec![0..3, 4..7, 8..11]);
        assert_eq!(find_matches(text, "", true).len(), 0);
        assert_eq!(find_matches("aaaa", "aa", true), vec![0..2, 1..3, 2..4]);
    }

    #[test]
    fn word_matches_respect_boundaries() {
        let is_word = |ch: char| ch == '_' || ch.is_alphanumeric();
        let text = "foo foodie foo_foo foo";
        assert_eq!(word_matches(text, "foo", is_word), vec![0..3, 19..22]);
    }

    #[test]
    fn replace_all_matches() {
        let text = "a1 b1 a1";
        let matches = find_matches(text, "a1", true);
        assert_eq!(replace_matches(text, &matches, "zz"), "zz b1 zz");
        // 重叠匹配只替换最前者。
        let matches = find_matches("aaaa", "aa", true);
        assert_eq!(replace_matches("aaaa", &matches, "b"), "bb");
    }

    #[test]
    fn indent_column_counting() {
        assert_eq!(indent_columns("    foo", 4), 4);
        assert_eq!(indent_columns("\tfoo", 4), 4);
        assert_eq!(indent_columns("  \tfoo", 4), 4);
        assert_eq!(indent_columns("foo", 4), 0);
        assert_eq!(indent_columns("      foo", 4), 6);
    }
}
