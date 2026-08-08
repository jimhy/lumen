//! 增量**行切分器**——headless CLI stdout / stderr 的第一层。
//!
//! # 为什么不用 [`std::io::BufRead::lines`]（设计蓝图 §6.3，三条理由各有依据）
//!
//! 1. **`lines()` 遇非 UTF-8 直接返回 `Err`，会断掉整条流。** 实测 hook 透传的 stdout 里存在被
//!    错误解码的字节（样本 B 有两行因此损坏、入库时只能留占位）。一个坏字节让整个会话再也读不到
//!    后续事件，这是不可接受的失败模式——本模块改成**有损解码**（坏字节变 `U+FFFD`），行本身照常
//!    产出，最坏结果只是那一行 JSON 解析失败被当成协议错误跳过。
//! 2. **`lines()` 的 `String` 增长无上限。** 实测单行已达 8677 字节（一个 `SessionStart` hook 的
//!    `additionalContext`），一次大文件 `Read` 的 `tool_result` 行可到 MB 级。没有上限就等于把
//!    「上游想让我们分配多少内存」的决定权交出去。
//! 3. **物理 `\n` 是唯一的重同步锚点。** JSON 行内的换行是转义的（`\n` 两个字符），物理换行只出现
//!    在行尾，所以按 `b'\n'` 切是**安全**的。丢掉它就丢掉了唯一的自愈点——这也是不用
//!    [`serde_json::StreamDeserializer`] 的理由：它按**值边界**切，中间出现半个坏值就再也无法
//!    重新对齐，而按行切只会损失出问题的那一行。
//!
//! # 被否决的替代方案
//!
//! - **`BufReader::split(b'\n')`**：能绕开 UTF-8 的问题，但仍然无长度上限（`Vec<u8>` 无界增长），
//!   且每行一次 `Vec` 分配。本模块复用同一个 `buf`，稳态零分配（只在产出行时分配 `String`）。
//! - **超长行截断后硬喂 serde**：会产出一个**看似合法实则残缺**的 JSON（截断点恰好在某个字符串
//!   中间时 serde 报错，恰好在对象边界时反而"解析成功"但内容缺失）。故本模块的语义是
//!   **整行丢弃 + 显式报量**（[`Chunk::Overlong`]），绝不截断。
//! - **在切分器里顺手做 JSON 解析**：切分与解析耦合会让「行超长」与「JSON 非法」两种失败混在一起，
//!   而它们的处置完全不同（前者是我们主动丢、后者要计入协议错误）。故本模块只切分，不认识 JSON。

/// 切分器每次吐出的东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    /// 一条完整行（已剥掉尾部 `\r\n` / `\n`），**有损 UTF-8 解码**：坏字节变 `U+FFFD`，
    /// **绝不因为编码问题断流**。
    ///
    /// 空行也会以 `Line(String::new())` 的形式产出——切分器不替调用方做「有没有内容」的判断，
    /// 那是 [`super::event::classify`] 的事。
    Line(String),
    /// 该行超过 `max_line`，**已整行丢弃**。`bytes` 是这一行的总字节数（不含行尾 `\n`），
    /// 用于报错时给出规模。
    ///
    /// 产出本项之后切分器已经回到干净状态：**下一行仍能正常解析**（重同步语义，有单测覆盖）。
    Overlong { bytes: usize },
}

/// 从字节流增量切出「完整行」。
///
/// # UTF-8 跨读块边界为什么天然安全
/// 这是本模块相对 `BufReader::lines()` 必须自己做对的核心：一次 `read()` 可能恰好把一个多字节
/// 字符切成两半（读 64 KiB 时这不是理论风险，中文正文里每 21845 个汉字就有一次机会撞上）。
/// 本切分器**只有拿到完整行才解码**，半个多字节字符只会静静停在 `buf` 里等下一块——
/// 于是"跨块的半个汉字"这件事在类型上就不可能发生。单测里有一条就是把一个汉字按字节劈开两次喂入。
///
/// 反面写法是「每读一块就 `String::from_utf8_lossy` 再找换行」——那会在块边界处稳定产出
/// `U+FFFD`，而且**每次读都产出**，肉眼看上去只是"偶尔有个乱码字符"，极难归因。
pub struct LineSplitter {
    /// 当前未完成行的字节缓冲。稳态下容量稳定，不重复分配。
    buf: Vec<u8>,
    /// 单行字节上限（不含行尾 `\n`）。超过即进入丢弃态。
    max_line: usize,
    /// 超长行丢弃态：吞掉一切直到下一个 `\n`。
    discarding: bool,
    /// 本次丢弃已吞掉的字节数（报错时给出规模）。用 `saturating_add`——
    /// 上游理论上可以送来一条永不结束的行。
    discarded: usize,
}

impl LineSplitter {
    /// `max_line`：单行字节上限（不含行尾 `\n`）。传 [`super::MAX_LINE_BYTES`]。
    #[must_use]
    pub fn new(max_line: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_line,
            discarding: false,
            discarded: 0,
        }
    }

    /// 喂入一块原始字节，把本块产生的所有完整行**追加**到 `out`。
    ///
    /// `out` 只追加不清空：调用方通常复用同一个 `Vec` 并在处理完后 `drain(..)`。
    pub fn feed(&mut self, data: &[u8], out: &mut Vec<Chunk>) {
        let mut rest = data;
        loop {
            let Some(nl) = rest.iter().position(|&b| b == b'\n') else {
                // 本块内没有行尾：整块并入当前状态，等下一块。
                self.absorb_tail(rest);
                return;
            };
            let (head, tail) = rest.split_at(nl);
            self.finish_line(head, out);
            // `tail[0]` 恒为 `b'\n'`，跳过它继续切下一行。
            rest = &tail[1..];
        }
    }

    /// 流结束（读到 EOF）时调用：把最后一段**没有行尾换行符**的内容作为一行吐出来。
    ///
    /// 不调用它就会静默吞掉最后一行。实测 CLI 正常退出时最后一条 `result` 行是带 `\n` 的，
    /// 但**进程被 kill 时管道里剩下的半行没有**——那半行本来就不完整、解析必然失败，
    /// 但仍然要走一遍协议错误路径而不是凭空消失（否则排障时会看到"事件流莫名少一条"）。
    pub fn finish(&mut self, out: &mut Vec<Chunk>) {
        if self.discarding {
            out.push(Chunk::Overlong {
                bytes: self.discarded,
            });
            self.reset_discard();
            return;
        }
        if !self.buf.is_empty() {
            out.push(Chunk::Line(self.take_line()));
        }
    }

    /// 当前缓冲里挂着的未完成字节数（诊断用；正常应远小于 `max_line`）。
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        if self.discarding {
            self.discarded
        } else {
            self.buf.len()
        }
    }

    // ── 内部 ─────────────────────────────────────────────────────────────────

    /// 本块尾部（不含换行）并入当前行；越限即转入丢弃态。
    fn absorb_tail(&mut self, tail: &[u8]) {
        if tail.is_empty() {
            return;
        }
        if self.discarding {
            self.discarded = self.discarded.saturating_add(tail.len());
            return;
        }
        let total = self.buf.len().saturating_add(tail.len());
        if total > self.max_line {
            // 进入丢弃态。**立刻释放 buf**（`clear()` 不还内存，这里要的是还内存）：
            // 一条 32 MiB 的畸形行不应该让这个 runner 的常驻内存永久涨 32 MiB。
            self.discarded = total;
            self.buf = Vec::new();
            self.discarding = true;
            return;
        }
        self.buf.extend_from_slice(tail);
    }

    /// 收尾一行（`head` 是行尾 `\n` 之前的全部字节，可能为空）。
    fn finish_line(&mut self, head: &[u8], out: &mut Vec<Chunk>) {
        if self.discarding {
            let bytes = self.discarded.saturating_add(head.len());
            out.push(Chunk::Overlong { bytes });
            self.reset_discard();
            return;
        }
        let total = self.buf.len().saturating_add(head.len());
        if total > self.max_line {
            // 越限但行尾就在本块内：直接报量，**不必**进丢弃态。
            self.buf = Vec::new();
            out.push(Chunk::Overlong { bytes: total });
            return;
        }
        self.buf.extend_from_slice(head);
        out.push(Chunk::Line(self.take_line()));
    }

    fn reset_discard(&mut self) {
        self.discarding = false;
        self.discarded = 0;
    }

    /// 把 `buf` 取成一个 `String`：剥掉 CRLF 的 `\r`，有损解码，清空缓冲。
    ///
    /// **只剥一个 `\r`**：`"a\r\r\n"` 的第一个 `\r` 属于内容（JSON 字符串里的 `\r` 是转义的
    /// `\\r`，物理 `\r` 只可能来自 CRLF 行尾本身），多剥就是改内容。
    fn take_line(&mut self) -> String {
        let end = if self.buf.last() == Some(&b'\r') {
            self.buf.len() - 1
        } else {
            self.buf.len()
        };
        let line = String::from_utf8_lossy(&self.buf[..end]).into_owned();
        self.buf.clear();
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一次性喂入并取回全部产出的小工具。
    fn feed_all(sp: &mut LineSplitter, data: &[u8]) -> Vec<Chunk> {
        let mut out = Vec::new();
        sp.feed(data, &mut out);
        out
    }

    fn lines(chunks: &[Chunk]) -> Vec<&str> {
        chunks
            .iter()
            .filter_map(|c| match c {
                Chunk::Line(s) => Some(s.as_str()),
                Chunk::Overlong { .. } => None,
            })
            .collect()
    }

    #[test]
    fn 单行分多块喂入_只在收到换行后产出() {
        let mut sp = LineSplitter::new(1024);
        assert!(feed_all(&mut sp, b"{\"type\":").is_empty());
        assert!(feed_all(&mut sp, b"\"assistant\"}").is_empty());
        let out = feed_all(&mut sp, b"\n");
        assert_eq!(lines(&out), vec!["{\"type\":\"assistant\"}"]);
        assert_eq!(sp.pending_bytes(), 0);
    }

    #[test]
    fn 多字节汉字被切在块边界_不产生替换字符() {
        // "中" = E4 B8 AD，按字节劈成两半分两次喂入。
        let mut sp = LineSplitter::new(1024);
        assert!(feed_all(&mut sp, &[0xE4, 0xB8]).is_empty());
        let out = feed_all(&mut sp, &[0xAD, b'\n']);
        assert_eq!(lines(&out), vec!["中"]);
        // 逐字节喂入同样成立（最坏情况）。
        let mut sp = LineSplitter::new(1024);
        for b in [0xE4u8, 0xB8, 0xAD] {
            assert!(feed_all(&mut sp, &[b]).is_empty());
        }
        let out = feed_all(&mut sp, b"\n");
        assert_eq!(lines(&out), vec!["中"]);
        assert!(!lines(&out)[0].contains('\u{FFFD}'));
    }

    #[test]
    fn 一块里含多行_全部产出() {
        let mut sp = LineSplitter::new(1024);
        let out = feed_all(&mut sp, b"a\nb\nc\n");
        assert_eq!(lines(&out), vec!["a", "b", "c"]);
    }

    #[test]
    fn crlf_行尾的回车被剥掉() {
        let mut sp = LineSplitter::new(1024);
        let out = feed_all(&mut sp, b"{\"a\":1}\r\n{\"b\":2}\r\n");
        assert_eq!(lines(&out), vec!["{\"a\":1}", "{\"b\":2}"]);
    }

    #[test]
    fn crlf_跨块_回车与换行分两次到达() {
        let mut sp = LineSplitter::new(1024);
        assert!(feed_all(&mut sp, b"x\r").is_empty());
        let out = feed_all(&mut sp, b"\n");
        assert_eq!(lines(&out), vec!["x"]);
    }

    #[test]
    fn 只剥一个回车_内容里的回车保留() {
        let mut sp = LineSplitter::new(1024);
        let out = feed_all(&mut sp, b"a\r\r\n");
        assert_eq!(lines(&out), vec!["a\r"]);
    }

    #[test]
    fn 空行照常产出_不被吞掉() {
        let mut sp = LineSplitter::new(1024);
        let out = feed_all(&mut sp, b"\n\r\na\n");
        assert_eq!(lines(&out), vec!["", "", "a"]);
    }

    #[test]
    fn 最后一行无换行符_由_finish_补吐() {
        let mut sp = LineSplitter::new(1024);
        let out = feed_all(&mut sp, b"first\nsecond-no-newline");
        assert_eq!(lines(&out), vec!["first"]);
        let mut tail = Vec::new();
        sp.finish(&mut tail);
        assert_eq!(lines(&tail), vec!["second-no-newline"]);
        // finish 后缓冲已空，重复调用不再产出。
        let mut again = Vec::new();
        sp.finish(&mut again);
        assert!(again.is_empty());
    }

    #[test]
    fn 超长行整行丢弃_并报出字节数() {
        let mut sp = LineSplitter::new(8);
        let out = feed_all(&mut sp, b"0123456789\n");
        assert_eq!(out, vec![Chunk::Overlong { bytes: 10 }]);
    }

    #[test]
    fn 超长行跨多块_丢弃态吞到行尾后重同步() {
        let mut sp = LineSplitter::new(8);
        // 第一块就越限 → 进丢弃态。
        assert!(feed_all(&mut sp, b"0123456789ab").is_empty());
        // 第二块继续吞，仍无产出。
        assert!(feed_all(&mut sp, b"cdefghij").is_empty());
        // 行尾到达 → 报出整行规模，然后**下一行必须正常解析**（重同步）。
        let out = feed_all(&mut sp, b"kl\n{\"ok\":1}\n");
        assert_eq!(
            out,
            vec![
                Chunk::Overlong { bytes: 22 },
                Chunk::Line("{\"ok\":1}".into()),
            ]
        );
        assert_eq!(sp.pending_bytes(), 0);
    }

    #[test]
    fn 丢弃态遇到_eof_也要报出来() {
        let mut sp = LineSplitter::new(4);
        assert!(feed_all(&mut sp, b"aaaaaaaa").is_empty());
        let mut out = Vec::new();
        sp.finish(&mut out);
        assert_eq!(out, vec![Chunk::Overlong { bytes: 8 }]);
    }

    #[test]
    fn 非_utf8_字节有损解码_而不是丢整行() {
        let mut sp = LineSplitter::new(1024);
        // 0xFF 不是任何合法 UTF-8 序列的起始字节。
        let out = feed_all(&mut sp, &[b'{', 0xFF, b'}', b'\n']);
        let got = lines(&out);
        assert_eq!(got.len(), 1, "坏字节绝不能让整行消失");
        assert!(got[0].contains('\u{FFFD}'));
        assert!(got[0].starts_with('{') && got[0].ends_with('}'));
    }

    #[test]
    fn 恰好等于上限的行不算超长() {
        let mut sp = LineSplitter::new(4);
        let out = feed_all(&mut sp, b"abcd\nabcde\n");
        assert_eq!(
            out,
            vec![Chunk::Line("abcd".into()), Chunk::Overlong { bytes: 5 },]
        );
    }

    #[test]
    fn 四字节emoji被切在块边界_不产生替换字符() {
        // 现有用例只到 3 字节汉字。4 字节序列走的是 `from_utf8_lossy` 的另一条分支
        // （首字节 0xF0 起头的 4 字节形式），必须单独钉住。
        let mut sp = LineSplitter::new(1024);
        assert!(feed_all(&mut sp, &[0xF0, 0x9F]).is_empty());
        let out = feed_all(&mut sp, &[0x98, 0x80, b'\n']);
        assert_eq!(lines(&out), vec!["😀"]);
        assert!(!lines(&out)[0].contains('\u{FFFD}'));
    }

    #[test]
    fn 超长边界落在多字节字符中间_只报量不panic() {
        // 上限 4 字节、内容是 3 个汉字（9 字节）：越限点落在第 2 个「中」的中间。
        // 当前实现是**整行丢弃**故不可能 panic，但这条不变量此前没有测试钉住——
        // 将来若有人改成「截断后上报」，`&s[..max]` 会在这里直接 panic。
        let mut sp = LineSplitter::new(4);
        let out = feed_all(&mut sp, "中中中\n".as_bytes());
        assert_eq!(out, vec![Chunk::Overlong { bytes: 9 }]);
        // 重同步：下一行仍要正常产出（这里上限只有 4 字节，故用一条 2 字节的行）。
        let out = feed_all(&mut sp, b"ok\n");
        assert_eq!(lines(&out), vec!["ok"]);
    }

    #[test]
    fn 只有bom的一块照常产出_剥bom是上层的事() {
        // 切分器**不认识** BOM（它只认 `\n` 与 `\r\n`）：`U+FEFF` 原样留在行里，
        // 由 `event::strip_bom` 在白名单那一层剥。分工写在这里，免得两层都剥或都不剥。
        let mut sp = LineSplitter::new(1024);
        let out = feed_all(&mut sp, b"\xEF\xBB\xBF\n");
        assert_eq!(lines(&out), vec!["\u{feff}"]);
        let out = feed_all(&mut sp, "\u{feff}{\"a\":1}\n".as_bytes());
        assert_eq!(lines(&out), vec!["\u{feff}{\"a\":1}"]);
    }

    #[test]
    fn 丢弃态下的_pending_bytes_报的是已吞字节数() {
        let mut sp = LineSplitter::new(4);
        assert!(feed_all(&mut sp, b"0123456789").is_empty());
        assert_eq!(
            sp.pending_bytes(),
            10,
            "丢弃态下报的是已吞掉的字节数（诊断要看得见规模），不是 buf.len()"
        );
        let out = feed_all(&mut sp, b"\n");
        assert_eq!(out, vec![Chunk::Overlong { bytes: 10 }]);
        assert_eq!(sp.pending_bytes(), 0, "报完必须回到干净状态");
    }

    #[test]
    fn 空输入块是_no_op() {
        let mut sp = LineSplitter::new(16);
        assert!(feed_all(&mut sp, b"").is_empty());
        assert!(feed_all(&mut sp, b"ab").is_empty());
        assert!(feed_all(&mut sp, b"").is_empty());
        let out = feed_all(&mut sp, b"\n");
        assert_eq!(lines(&out), vec!["ab"]);
    }
}
