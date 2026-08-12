# `lib/ui/` —— 页面与组件

> 状态：**空壳**，由 M7 **片 5 / 6 / 7** 填。

## 计划目录（§7.2）

```text
ui/
├─ login/  devices/  pair/  pick/      # 片 5：登录 / 设备列表 / 配对 / 项目选择
├─ chat/                               # 片 6、7
│  ├─ chat_page.dart
│  ├─ chat_status_bar.dart             # 模型 / 上下文 / 花费 / 中断
│  ├─ chat_rate_limit_bar.dart         # 额度状态条（rate_limit_event，§7.6）；额度正常时整条不显示
│  ├─ message_list.dart                # reverse ListView + memo + keys
│  ├─ bubbles/  cards/  composer.dart
└─ common/markdown_view.dart           # Markdown 渲染包装层（换实现只改这一处）
```

## 四条必须守的 UI 约束

1. **`markdown_view.dart` 是全 App 唯一 import 具体 Markdown 实现的文件**（R14：官方
   `flutter_markdown` 已归档，生态不稳）。散落 import 会让换包变成改几十处。
   当前实现：`flutter_markdown_plus`，`pubspec.yaml` 里**精确锁版、无 caret**。
2. **上下文占用条与额度条是两回事，不要合并**（§7.9 末）。上下文占用 = `usage` / `contextWindow`；
   额度 = `rate_limit_event`。
   - 轮末刷新出来的百分比是**真值**，可以直接写「上下文 42%」；
   - 轮中沿用上一轮窗口算出来的是**估算**，必须标「约 42%」；
   - 首轮结束前**没有** `contextWindow`，此时不显示百分比。
3. **权限模式必须常驻可见**（R5）：P0 是 Tier 0（`--permission-mode acceptEdits` + `--add-dir` 围栏
   + 黑名单），手机**不能**逐次授权，被拒的调用呈现为「被拒绝的工具调用」卡片。
   顶部状态条要常显当前模式，项目选择页要明确告知。**`bypassPermissions` 手机端永不提供开关。**
4. **降级必须可见**（§14-6 无声降级禁令）：`Dropped` 增量要画成「内容有缺口，点击补齐」
   （点击发 `TurnFetch`），未知工具名要有兜底卡片形态（CLI 会持续新增工具名），
   老 PC 不支持 LLM 子协议要给一句人话而不是转圈（R1，验收第一项）。
