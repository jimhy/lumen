/// Markdown 渲染的**唯一**包装层。
///
/// 全 App 只有这个文件 import `flutter_markdown_plus`。理由写在 `pubspec.yaml` 里：
/// 官方 `flutter_markdown` 已归档，社区 fork 各有取舍且**版本锁死不用 caret**——
/// 换实现时只该改这一处，而不是满仓库找 `MarkdownBody`。
///
/// ## memo 缓存：条目定稿后不再解析第二次
///
/// Markdown 解析是长对话卡顿的大头。缓存 key 是 `(itemKey, revision)`：
/// 条目一旦定稿就命中缓存，只有**块被终态覆盖**（revision 自增）时才重新解析一次。
///
/// 配 LRU 裁剪防长会话内存膨胀——缓存的是 Widget（不可变的配置对象），跨 BuildContext
/// 复用是安全的。
library;

import 'dart:collection';

import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';

/// LRU 上限。200 条 ≈ 一屏的十几倍，够覆盖来回滚动的范围；再大对内存不划算。
const int kMarkdownMemoCapacity = 200;

/// 已解析 Markdown 的 LRU 缓存。
///
/// 挂在 State 上（每个列表一份），不做全局单例——全局的那份会在页面销毁后继续
/// 攥着一堆 Widget，而它们引用的 Theme 早已失效。
final class MarkdownMemo {
  MarkdownMemo({this.capacity = kMarkdownMemoCapacity});

  final int capacity;
  final LinkedHashMap<String, Widget> _lru = LinkedHashMap<String, Widget>();

  /// 取（必要时构建）一个条目的渲染结果。
  Widget of(String itemKey, int revision, Widget Function() build) {
    final String key = '$itemKey#$revision';
    final Widget? hit = _lru.remove(key);
    if (hit != null) {
      _lru[key] = hit; // 命中即提到最近使用
      return hit;
    }
    final Widget built = build();
    _lru[key] = built;
    if (_lru.length > capacity) {
      _lru.remove(_lru.keys.first);
    }
    return built;
  }

  /// 当前缓存条数（测试与诊断用）。
  int get length => _lru.length;

  void clear() => _lru.clear();
}

/// 渲染一段 Markdown。
///
/// **流式期间不要用它**——末块要用纯 [Text]，理由见 `chat/message_list.dart`：
/// 既省解析，又避免半截 code fence 造成的闪烁。
class MarkdownView extends StatelessWidget {
  const MarkdownView({required this.data, super.key});

  final String data;

  @override
  Widget build(BuildContext context) {
    return MarkdownBody(
      data: data,
      selectable: true,
      // shrinkWrap：条目宽度由列表决定，让 Markdown 自己按内容定高。
      shrinkWrap: true,
    );
  }
}
