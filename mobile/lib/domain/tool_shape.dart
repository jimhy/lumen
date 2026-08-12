/// 工具名 → 卡片形态（片 9）——**零 Flutter 依赖**。
///
/// ## ★ 兜底不是可选项
///
/// CLI 的工具清单**会持续新增**（Claude Code 每个小版本都可能加）。没有 [ToolShape.generic]
/// 这一档，新工具在手机上就是一个空白卡片或一次崩溃。这里的每一个 `switch` 都以 `_ =>`
/// 收尾，且 `shapeOf` 有一条测试专门喂它一个编出来的工具名。
///
/// ## ★ 形态只决定「怎么画」，不决定「画什么字段」
///
/// 这条边界很重要：认出 `Edit` 只意味着「值得尝试画成 diff」，**不意味着可以假定
/// `old_string` / `new_string` 一定在**。取字段一律走 `tool_card.dart` 里那套
/// 「尝试 + 兜底」，取不到就退回折叠 JSON。
///
/// 理由见蓝图 §7.6 与 §3.3「仍未取到」表：**只有 `Read` 的 `tool_use_result` 形态被实测过**，
/// 其余工具的结果形状全是推定。而入参（`tool_use.input`）虽然是模型必须按 schema 生成的
/// 公开契约、比结果可靠，也照样可能随版本变。
library;

/// 卡片形态。与蓝图 §7.6 那张表一一对应。
enum ToolShape {
  /// 读文件：摘要走 `LlmToolResultDetailFile`（唯一被实测过的结构化形态）。
  read,

  /// 改文件：**尝试**从入参里取 `old_string` / `new_string` 画行级 diff。
  edit,

  /// 写文件：入参里的 `content`。
  write,

  /// 执行命令：等宽字体渲染。
  bash,

  /// 搜索：查询词 + 命中。
  search,

  /// 网络：URL / 查询词。
  web,

  /// 待办清单。
  todo,

  /// 子代理任务。
  task,

  /// **兜底**：折叠 JSON。新工具、改了名的工具、拼错的工具都落这里。
  generic,
}

/// 工具名 → 形态。**未知一律 [ToolShape.generic]。**
///
/// 名字取自 Claude Code 的工具清单（蓝图 §7.6）。大小写敏感——上游就是这么发的，
/// 做成大小写不敏感反而会让「Read 和 read 是两个不同工具」这种真实情况被悄悄合并。
ToolShape shapeOf(String name) => switch (name) {
      'Read' => ToolShape.read,
      'Edit' || 'MultiEdit' || 'NotebookEdit' => ToolShape.edit,
      'Write' => ToolShape.write,
      'Bash' || 'BashOutput' || 'KillShell' => ToolShape.bash,
      'Grep' || 'Glob' => ToolShape.search,
      'WebFetch' || 'WebSearch' => ToolShape.web,
      'TodoWrite' => ToolShape.todo,
      'Task' => ToolShape.task,
      _ => ToolShape.generic,
    };

/// 形态的图标语义名。UI 层据此选 `IconData`——**`domain/` 不 import material**。
String iconNameOf(ToolShape shape) => switch (shape) {
      ToolShape.read => 'description',
      ToolShape.edit => 'edit_note',
      ToolShape.write => 'note_add',
      ToolShape.bash => 'terminal',
      ToolShape.search => 'search',
      ToolShape.web => 'public',
      ToolShape.todo => 'checklist',
      ToolShape.task => 'account_tree',
      ToolShape.generic => 'build_outlined',
    };
