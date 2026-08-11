# 回放语料（M7 片 7）

这个目录里的 `*.jsonl` 与上一层的 `*.json` **不是一类东西**，所以才独立成目录：

|                | 上一层 `golden/mobile/*.json` | 本目录 `replay/*.jsonl`        |
| -------------- | ----------------------------- | ------------------------------ |
| 形状           | 一文件一用例，帧收在信封的 `frame` 键下 | 一行一个 `LlmFrame` 原文，无信封 |
| 用途           | 钉死**线格式的形状**（两端往返断言）  | 喂给手机端做**离线回放**，验渲染   |
| 来源           | 手写                          | 由 PC 侧真实归一化链路**生成**    |
| 谁读它         | `mobile_golden.rs` / `golden_test.dart` | 片 7 的回放入口（人 / 测试）  |

两端枚举语料时都只取上一层的 `*.json`（Rust 侧按扩展名过滤，Dart 侧 `whereType<File>()`
天然跳过目录），故本目录不会被误当成用例。

---

## 文件

| 文件                       | 来源样本 | 帧数 | op 分布                              |
| -------------------------- | -------- | ---- | ------------------------------------ |
| `sample_b_llmframes.jsonl` | 样本 B（成功路径 + 工具调用） | 5 | `TurnStarted` ×1、`Delta` ×3、`RateLimit` ×1 |
| `sample_a_llmframes.jsonl` | 样本 A（失败路径，403 认证失败） | 3 | `TurnStarted` ×1、`Delta` ×1、`TurnEnded` ×1 |

## 怎么生成 / 怎么重新生成

```
cargo test -p lumen-app -- --ignored 生成片7回放语料 --nocapture
```

生成器是 `crates/lumen-app/src/remote_ws/llm.rs` 里的 `#[ignore]` 测试
`生成片7回放语料`。它把两份 fixture
（`crates/lumen-app/src/llm_runner/fixtures/claude-stream-json-sample-*.jsonl`，
Claude CLI 的**原始 stream-json**）逐行喂过 PC 侧的完整链路：

```
event::classify（白名单严入）
  → LlmAgentDecoder::decode_line（片 3 归一化）
  → LlmPlane::pump（片 4：轮号 / seq / 33 ms 合并窗口 / 白名单严出）
  → 出站 LlmFrame
```

> ⚠ **原始样本不能直接当回放语料**。样本是归一化的**输入**（`{"type":"assistant",…}`，
> CLI 私有形状），手机端解的是内部标签为 `"op"` 的 `LlmFrame`。直接喂给 Dart，每一行都会
> **静默**落到 `LlmUnknown` 兜底变体上——测试全绿、画面全空。设计蓝图 :3928 与
> `docs/M7-交接-2026-08-10.md` 里「回放语料直接用样本 B」那句话按字面做就是这个下场。

## 私人内容

hook 行（`system/hook_started` / `system/hook_response`，样本里第 1-6 行）在**白名单严入**
那一关就被挡掉，只留一个 `LineDropped{tag}`；`system/init` 的 `cwd` / `plugins` / `skills` /
`slash_commands` 同样不进任何出站帧。生成器末尾的 `回放泄漏自查` 对**序列化后的文本**逐条
断言（禁用键 + 私人内容来源标记），任一条命中就 panic 且**不落盘**。

产物里出现的 `<<REDACTED …>>` 都是 fixture 自带的脱敏占位符，且只出现在**协议本来就要
转发**的位置（工具入参 `file_path`、工具结果 `output` 与 `detail.path`）。

## 已知缺口（片 7 要知道的）

- **没有 `LlmBlock::Text`**：两份样本里一个纯文本块都没有（样本 B 全是工具调用与思考，
  样本 A 的助手行是 `<synthetic>` 的 403 报错，归一化成 `Error` 块）。
  流式文本的渲染请用上一层的 `frame_delta.json` / `edge_text_append_split.json`。
- **`Thinking` 只有 `Omitted` 形态**：样本 B 那条 thinking 的正文是空串（只有 signature），
  归一化后是 `{"kind":"Omitted"}`。带正文的思考请另找语料。
- **样本 B 没有 `TurnEnded`**：它的 `result` 行在采样时被 PowerShell 重定向的编码损坏了
  （fixture 里是 `_note` 占位）。收尾帧请用 `sample_a_llmframes.jsonl` 的那条。
- **`TurnStarted.user` 是空的**：用户提示词是走 stdin 进去的，不在 stdout 样本里。
- **没有 `ConvStarted` / `Attached` 基线帧**：这两条由片 4 在建会话时发，与样本无关；
  回放时请先用上一层的 `frame_conv_started.json` 建出对话再灌本目录的流。
- 时间戳（`started_ms` / `ended_ms` / `observed_ms`）是**生成那一刻的墙钟**，
  重新生成会变。它们不参与任何断言。
