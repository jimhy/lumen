/// 工具卡片的呈现（片 9）。
///
/// 领域判断已在 `test/domain/tool_card_test.dart` 测完，这里只测**画出来才看得见**的三件事：
///
/// 1. 不可展开时**不画箭头**（一个点开什么都没有的箭头比没有箭头更糟）；
/// 2. diff 的行首恒带 `+`/`-`——**颜色不是唯一判据**，色盲用户与灰度截图下也要读得出来；
/// 3. 展开状态由父级持有，**滚出屏幕再回来不丢**。
library;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/domain/tool_card.dart';
import 'package:lumen_mobile/domain/tool_shape.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';
import 'package:lumen_mobile/state/conversation_controller.dart';
import 'package:lumen_mobile/state/streaming_tail.dart';
import 'package:lumen_mobile/ui/chat/message_list.dart';
import 'package:lumen_mobile/ui/chat/tool_card_tile.dart';

Future<void> pumpCard(
  WidgetTester tester,
  ToolCard card, {
  bool expanded = false,
  VoidCallback? onToggle,
  VoidCallback? onFetchFull,
}) =>
    tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: ToolCardTile(
          card: card,
          expanded: expanded,
          onToggle: onToggle,
          onFetchFull: onFetchFull,
        ),
      ),
    ));

ChatToolCall editCall(String oldText, String newText) => ChatToolCall(
      turn: 1,
      blockId: 0,
      role: ChatRole.assistant,
      callId: 'c1',
      name: 'Edit',
      input: LlmToolInput(<String, Object?>{
        'file_path': 'src/main.rs',
        'old_string': oldText,
        'new_string': newText,
      }),
    );

void main() {
  group('折叠与展开', () {
    testWidgets('★ 不可展开就不画箭头', (WidgetTester tester) async {
      const ToolCard card = ToolCard(
        shape: ToolShape.read,
        summary: '读取 a.rs 第 1–10 行（共 10 行）',
        body: ToolBodyNone(),
      );
      await pumpCard(tester, card);

      expect(find.text(card.summary), findsOneWidget);
      expect(find.byIcon(Icons.expand_more), findsNothing);
      expect(find.byIcon(Icons.expand_less), findsNothing);
    });

    testWidgets('可展开时画箭头，点一下回调', (WidgetTester tester) async {
      int taps = 0;
      await pumpCard(
        tester,
        const ToolCard(
          shape: ToolShape.bash,
          summary: 'cargo test',
          body: ToolBodyText('输出'),
        ),
        onToggle: () => taps++,
      );

      expect(find.byIcon(Icons.expand_more), findsOneWidget);
      await tester.tap(find.byIcon(Icons.expand_more));
      expect(taps, 1);
    });

    testWidgets('展开后画出正文，箭头翻转', (WidgetTester tester) async {
      await pumpCard(
        tester,
        const ToolCard(
          shape: ToolShape.bash,
          summary: 'cargo test',
          body: ToolBodyText('test result: ok'),
        ),
        expanded: true,
      );

      expect(find.text('test result: ok'), findsOneWidget);
      expect(find.byIcon(Icons.expand_less), findsOneWidget);
      expect(find.text('复制'), findsOneWidget);
    });

    testWidgets('折叠时不画正文（省下渲染，也不误导）', (WidgetTester tester) async {
      await pumpCard(
        tester,
        const ToolCard(
          shape: ToolShape.bash,
          summary: 'cargo test',
          body: ToolBodyText('test result: ok'),
        ),
      );
      expect(find.text('test result: ok'), findsNothing);
    });
  });

  group('diff', () {
    testWidgets('★ 行首恒带 +/-，颜色不是唯一判据', (WidgetTester tester) async {
      await pumpCard(
        tester,
        toolCallCard(editCall('let x = 1;', 'let x = 2;')),
        expanded: true,
      );

      // 色盲用户与灰度截图下也要读得出来哪行是加、哪行是删。
      expect(find.text('-let x = 1;'), findsOneWidget);
      expect(find.text('+let x = 2;'), findsOneWidget);
    });

    testWidgets('增删计数与文件名画在一起', (WidgetTester tester) async {
      await pumpCard(
        tester,
        toolCallCard(editCall('a\nb', 'a\nB\nC')),
        expanded: true,
      );
      // 整行精确断言：`src/main.rs` 在摘要行里也出现一次，textContaining 会撞车。
      expect(find.text('+2 −1 · src/main.rs'), findsOneWidget);
      expect(find.text('修改 src/main.rs'), findsOneWidget, reason: '摘要行');
    });

    testWidgets('★ 改动过大时说出来，不画空的也不转圈', (WidgetTester tester) async {
      final String big =
          List<String>.generate(600, (int i) => 'old$i').join('\n');
      final String big2 =
          List<String>.generate(600, (int i) => 'new$i').join('\n');
      await pumpCard(tester, toolCallCard(editCall(big, big2)), expanded: true);

      expect(find.textContaining('改动过大'), findsOneWidget);
      expect(find.textContaining('600 行'), findsOneWidget);
    });
  });

  group('按需拉全文', () {
    testWidgets('★ 说出还有多少字节，不只说「已截断」', (WidgetTester tester) async {
      // 只写「内容已截断」会让用户以为少了一两行，而实测一次 Read 被夹掉的是 12 KB。
      await pumpCard(
        tester,
        const ToolCard(
          shape: ToolShape.read,
          summary: '读取 README.md 第 1–165 行（共 165 行）',
          body: ToolBodyNone(),
          truncatedBytes: 12272,
        ),
        expanded: true,
        onFetchFull: () {},
      );
      expect(find.textContaining('12272 字节'), findsOneWidget);
      expect(find.textContaining('点击拉取'), findsOneWidget);
    });

    testWidgets('没接回调时不说「点击拉取」（别给一个点不动的入口）',
        (WidgetTester tester) async {
      await pumpCard(
        tester,
        const ToolCard(
          shape: ToolShape.read,
          summary: 'x',
          body: ToolBodyNone(),
          truncatedBytes: 99,
        ),
        expanded: true,
      );
      expect(find.textContaining('99 字节'), findsOneWidget);
      expect(find.textContaining('点击拉取'), findsNothing);
    });
  });

  group('★ 展开状态活在列表里，不在卡片里', () {
    testWidgets('滚出屏幕再滚回来，展开的卡片仍然是展开的',
        (WidgetTester tester) async {
      // 存在卡片自己的 State 里的话，ListView.builder 一销毁重建它就折叠了 ——
      // 一个不报错、只让人觉得「这 App 有点怪」的 bug。
      final List<ChatItem> items = <ChatItem>[
        const ChatToolCall(
          turn: 1,
          blockId: 0,
          role: ChatRole.assistant,
          callId: 'c1',
          name: 'Bash',
          input: LlmToolInput(<String, Object?>{'command': '第一条命令'}),
        ),
        for (int i = 1; i < 40; i++)
          ChatText(
            turn: 1,
            blockId: i,
            role: ChatRole.assistant,
            markdown: '填充第 $i 行',
          ),
      ];

      final StreamingTail tail = StreamingTail();
      addTearDown(tail.dispose);
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: MessageList(
            snapshot: ConversationSnapshot(items: items),
            tail: tail,
          ),
        ),
      ));
      await tester.pumpAndSettle();

      // 展开后的卡片里有横向滚动视图，`scrollUntilVisible` 默认要求「唯一的
      // Scrollable」会当场 Too many elements —— 显式指定外层这一个。
      final Finder list = find.byType(Scrollable).first;

      // reverse: true ⇒ items[0]（工具卡片）在视觉最上方，得滚过 39 条填充才看得到。
      await tester.scrollUntilVisible(find.byIcon(Icons.expand_more), 300,
          scrollable: list);
      await tester.tap(find.byIcon(Icons.expand_more));
      await tester.pumpAndSettle();
      expect(find.byIcon(Icons.expand_less), findsOneWidget, reason: '已展开');

      // 滚到列表另一端 —— items[39] 离工具卡片最远，中间隔着 38 条，卡片一定被销毁了。
      // （别用「填充第 1 行」：它是 items[1]，**紧挨着**卡片，滚到它时卡片还在屏上。）
      await tester.scrollUntilVisible(find.text('填充第 39 行'), -300,
          scrollable: list);
      expect(find.byIcon(Icons.expand_less), findsNothing, reason: '确实滚出屏幕了');

      // 再滚回来。
      await tester.scrollUntilVisible(find.byIcon(Icons.expand_less), 300,
          scrollable: list);
      expect(find.byIcon(Icons.expand_less), findsOneWidget,
          reason: '★ 滚回来还该是展开的（状态存在列表里，不在卡片里）');
    });
  });
}
