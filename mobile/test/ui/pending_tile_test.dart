/// 乐观气泡的三态文案（片 10）。
///
/// ## 为什么这一条值得单独一个文件
///
/// `OutboxDelivery.uncertain` 的文案是本片**唯一一处「说错了会造成真实损失」的地方**：
/// 写成「发送失败」会让用户放心地再发一遍，而对方很可能已经执行过一次——
/// 而这一片的消息内容是「让 PC 跑一遍 Bash」。重复执行的代价是真的。
///
/// 这跟片 6 那三条 `DenyReason` 文案是同一类断言（`link_messages_test.dart`）：
/// 测的不是像素，是**说法本身**。
library;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/ui/chat/chat_bubbles.dart';

Future<void> pumpTile(
  WidgetTester tester,
  OutboxDelivery state, {
  void Function(ChatPending)? onResend,
  void Function(ChatPending)? onDiscard,
}) async {
  await tester.pumpWidget(MaterialApp(
    home: Scaffold(
      body: PendingTile(
        item: ChatPending(
          blockId: 0,
          clientMsgId: 'msg-1',
          text: '跑一下测试',
          state: state,
        ),
        onResend: onResend,
        onDiscard: onDiscard,
      ),
    ),
  ));
}

void main() {
  testWidgets('三态都把正文画出来', (WidgetTester tester) async {
    for (final OutboxDelivery s in OutboxDelivery.values) {
      await pumpTile(tester, s);
      expect(find.text('跑一下测试'), findsOneWidget, reason: '${s.name} 态');
    }
  });

  testWidgets('待发送：说清楚是「还没发出去」且会自动补发', (WidgetTester tester) async {
    await pumpTile(tester, OutboxDelivery.queued);
    expect(find.textContaining('待发送'), findsOneWidget);
    expect(find.textContaining('自动'), findsOneWidget,
        reason: '不说这句，用户会以为要自己重按一次');
  });

  testWidgets('已送出：不说「成功」', (WidgetTester tester) async {
    // `send` 返回 true 只表示写进了 socket，中继还没转给 PC 就可能断线。
    await pumpTile(tester, OutboxDelivery.sent);
    expect(find.textContaining('已送出'), findsOneWidget);
    expect(find.textContaining('成功'), findsNothing);
  });

  testWidgets('★ 未知：必须说「对方可能已经收到」，且不许说「失败」',
      (WidgetTester tester) async {
    await pumpTile(tester, OutboxDelivery.uncertain);
    expect(find.textContaining('可能已经收到'), findsOneWidget);
    expect(find.textContaining('失败'), findsNothing,
        reason: '「发送失败」会让用户放心地再发一遍，而对方很可能已经执行过一次');
  });

  testWidgets('★ 未知态才给重发/删除入口（前两态自己会好，给按钮是误导）',
      (WidgetTester tester) async {
    await pumpTile(tester, OutboxDelivery.queued);
    expect(find.text('再发一次'), findsNothing);

    await pumpTile(tester, OutboxDelivery.uncertain);
    expect(find.text('再发一次'), findsOneWidget);
    expect(find.text('删除'), findsOneWidget);
  });

  testWidgets('重发与删除各自回调，且带的是本条的 id', (WidgetTester tester) async {
    final List<String> resent = <String>[];
    final List<String> dropped = <String>[];
    await pumpTile(
      tester,
      OutboxDelivery.uncertain,
      onResend: (ChatPending i) => resent.add(i.clientMsgId),
      onDiscard: (ChatPending i) => dropped.add(i.clientMsgId),
    );

    await tester.tap(find.text('再发一次'));
    await tester.tap(find.text('删除'));
    expect(resent, <String>['msg-1']);
    expect(dropped, <String>['msg-1']);
  });

  testWidgets('没给回调时按钮是禁用的，不是点了没反应', (WidgetTester tester) async {
    await pumpTile(tester, OutboxDelivery.uncertain);
    final TextButton button =
        tester.widget<TextButton>(find.widgetWithText(TextButton, '再发一次'));
    expect(button.onPressed, isNull);
  });
}
