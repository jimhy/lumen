/// 线格式原语——**零 Flutter 依赖**（见 `lib/protocol/README.md` 的铁律）。
///
/// 这一层只做三件事：把 `jsonDecode` 的 `Object?` 安全地读成强类型、把强类型写回
/// 线格式、以及提供两种标签枚举的编解码骨架。协议语义一律不在这里。
///
/// ## 为什么读取器要这么啰嗦
///
/// Rust 侧是 serde：类型对不上就是 `Err`，不会静默降级。Dart 手写模型最容易漂的
/// 恰恰是这一点——`m['seq'] as int` 在收到 `1.0` 时抛的是 `TypeError`，收到缺键时
/// 抛的是 null 断言，两者的报错都不说是哪个字段。这里统一成 [WireFormatException]
/// 并带上字段名，golden 语料一红就能直接看出错在哪一格。
///
/// ## 整数：只认 `int`，不接受 `double`
///
/// 契约（golden README §2 规则 1）明写「`1` 与 `1.0` **算**差异」。Dart 的 `num`
/// 相等是跨 `int`/`double` 的（`1 == 1.0` 为真），所以只要有一处把整数存成
/// `double`，几乎全部语料都会绿灯通过而线格式已经不一致了。读取器在这里把关：
/// 整数字段收到 `double` 一律报错，绝不 `toInt()` 兜。
library;

import 'dart:convert';

/// 线格式解析失败。
///
/// 对应 Rust 侧 serde 的 `Err`：**整帧报废**。前向兼容的降级（未知 op / 未知块 /
/// 未知枚举值）不走这里，它们在各自那一层落到 `Unknown`，见 golden README §2 规则 4。
class WireFormatException implements Exception {
  const WireFormatException(this.message);

  final String message;

  @override
  String toString() => 'WireFormatException: $message';
}

/// 解析后的 JSON 对象。
typedef JsonMap = Map<String, Object?>;

Never _fail(String message) => throw WireFormatException(message);

/// 把任意 JSON 值读成对象。
JsonMap asJsonMap(Object? value, String what) =>
    value is JsonMap ? value : _fail('$what 期望 JSON 对象，实际是 ${_kindOf(value)}');

String _kindOf(Object? value) => switch (value) {
      null => 'null',
      int() => 'int',
      double() => 'double',
      String() => 'String',
      bool() => 'bool',
      List<Object?>() => 'List',
      Map<Object?, Object?>() => 'Map',
      _ => value.runtimeType.toString(),
    };

// ── 必填字段 ──────────────────────────────────────────────────────────────────

/// 必填整数。**收到 `double` 报错**，理由见库文档。
int readInt(JsonMap m, String key) {
  final Object? v = m[key];
  if (v is int) return v;
  _fail('字段 "$key" 期望整数，实际是 ${_kindOf(v)}');
}

/// 必填字符串。
String readString(JsonMap m, String key) {
  final Object? v = m[key];
  if (v is String) return v;
  _fail('字段 "$key" 期望字符串，实际是 ${_kindOf(v)}');
}

/// 必填布尔。
bool readBool(JsonMap m, String key) {
  final Object? v = m[key];
  if (v is bool) return v;
  _fail('字段 "$key" 期望布尔，实际是 ${_kindOf(v)}');
}

/// 把数组元素读成字符串。
///
/// 用它而不是裸 `v as String`：后者抛的是 `TypeError`，不带字段上下文，
/// 也不属于本层统一的 [WireFormatException]。
String asWireString(Object? value) =>
    value is String ? value : _fail('数组元素期望字符串，实际是 ${_kindOf(value)}');

/// 必填对象字段，交给 `parse` 转成模型。
T readObject<T>(JsonMap m, String key, T Function(JsonMap) parse) =>
    parse(asJsonMap(m[key], '字段 "$key"'));

/// 必填数组字段。
List<T> readList<T>(JsonMap m, String key, T Function(Object?) parse) {
  final Object? v = m[key];
  if (v is! List<Object?>) {
    _fail('字段 "$key" 期望数组，实际是 ${_kindOf(v)}');
  }
  return v.map(parse).toList(growable: false);
}

// ── `#[serde(default)]`：键缺席或为 null 时取默认值 ────────────────────────────

/// 带默认值的整数。
int readIntOr(JsonMap m, String key, int fallback) =>
    m[key] == null ? fallback : readInt(m, key);

/// 带默认值的布尔。
bool readBoolOr(JsonMap m, String key, {required bool fallback}) =>
    m[key] == null ? fallback : readBool(m, key);

/// 带默认值的数组（缺省为空表）。
List<T> readListOr<T>(JsonMap m, String key, T Function(Object?) parse) =>
    m[key] == null ? const [] : readList(m, key, parse);

/// 带默认值的对象（缺省时用 `orElse` 造一个默认实例）。
T readObjectOr<T>(
  JsonMap m,
  String key,
  T Function(JsonMap) parse,
  T Function() orElse,
) =>
    m[key] == null ? orElse() : readObject(m, key, parse);

// ── `Option<T>`：**输入侧** null 与键缺席等价 ─────────────────────────────────
//
// 输出侧不等价——`skip_serializing_if` 让该键整个消失，见 [writeOpt]。

/// 可选整数。
int? readIntOpt(JsonMap m, String key) => m[key] == null ? null : readInt(m, key);

/// 可选字符串。
String? readStringOpt(JsonMap m, String key) =>
    m[key] == null ? null : readString(m, key);

/// 可选布尔。
///
/// **不要**把它退化成 `readBoolOr(..., fallback: false)`：协议里有字段
/// （`LlmRateLimit.using_overage`）刻意区分「上游没给」与「上游明确给了 false」。
bool? readBoolOpt(JsonMap m, String key) =>
    m[key] == null ? null : readBool(m, key);

/// 可选对象。
T? readObjectOpt<T>(JsonMap m, String key, T Function(JsonMap) parse) =>
    m[key] == null ? null : readObject(m, key, parse);

/// 可选数组。
List<T>? readListOpt<T>(JsonMap m, String key, T Function(Object?) parse) =>
    m[key] == null ? null : readList(m, key, parse);

// ── 写出 ─────────────────────────────────────────────────────────────────────

/// `Option + skip_serializing_if`：非 null 才写键。
///
/// 写成 `'model': null` 就与 Rust 不同——golden 的 `edge_null_option.json` 专门守这条，
/// 它是手写模型最常见的一处漂移。
void writeOpt(JsonMap out, String key, Object? value) {
  if (value != null) out[key] = value;
}

// ── UTF-8 夹紧（调试渲染用）────────────────────────────────────────────────────

/// 该字符串的 UTF-8 字节数。
///
/// Dart 的 `String.length` 是 **UTF-16 码元**数，中文一字算 1、emoji 算 2；
/// 而协议里所有按长度夹紧的地方（`LLM_TOOL_OUTPUT_MAX_BYTES` = 32 KiB、
/// `LLM_ENUM_WIRE_MAX_BYTES` = 64 B）都是 UTF-8 字节。直接用 `.length` 会少算 2–3 倍。
int utf8Length(String s) => utf8.encode(s).length;

/// 按 UTF-8 字节数截到不超过 `maxBytes`，并退回字符边界。
///
/// 裸切第 N 字节会切出半个多字节字符（`utf8.decode` 报错或产出替换符）。
String clampUtf8(String s, int maxBytes) {
  final List<int> bytes = utf8.encode(s);
  if (bytes.length <= maxBytes) return s;
  int end = maxBytes;
  // UTF-8 续接字节形如 0b10xxxxxx；落在续接字节上说明切在字符中间，往回退。
  while (end > 0 && (bytes[end] & 0xC0) == 0x80) {
    end--;
  }
  return utf8.decode(bytes.sublist(0, end));
}

// ── 开放式 C 型枚举（线上是裸字符串）──────────────────────────────────────────

/// 已知枚举值自带的**线格式原文**。
///
/// 线格式是本协议自己的 `PascalCase`，与 Dart 的 `lowerCamelCase` 命名不同，
/// 也与上游 CLI 的 `snake_case` 不同（三者都不一样，见 `llm.rs` 的 `open_enum!` 文档）。
/// 所以已知值必须显式带上自己的线格式串，不能靠 `name` 推。
abstract interface class WireNamed {
  String get wire;
}

/// 调试渲染里 `Other` 分支只打前多少字节（对齐 Rust 的 `LLM_ENUM_DEBUG_HEAD_BYTES`）。
const int llmEnumDebugHeadBytes = 16;

/// 被控端把上游原文落进 `Other` 前的夹紧上限（对齐 Rust 的 `LLM_ENUM_WIRE_MAX_BYTES`）。
///
/// **只在发送侧生效**：接收侧刻意不校验长度（校验会把「超长」变成整帧报废，比留原文更糟），
/// 所以调试渲染必须自己再守一道，见 [OpenEnum.toString]。
const int llmEnumWireMaxBytes = 64;

/// 开放式 C 型枚举：**已知值 + 未知原文**二选一。
///
/// 直接映射成 Dart `enum` 会在遇到新版本对端的取值时抛异常并丢掉原文——而这类字段
/// （错误码、退出原因）恰恰是「出事时唯一告诉用户出了什么事」的那一格。
final class OpenEnum<K extends WireNamed> {
  const OpenEnum.known(K this.known) : other = null;

  /// 未知原文。**本端构造应走 [OpenEnumCodec.fromWireClamped]**，它会先试已知集合、
  /// 再按 [llmEnumWireMaxBytes] 夹紧，避免一个本端认识的值静默落进 `Other`。
  const OpenEnum.other(String this.other) : known = null;

  final K? known;
  final String? other;

  bool get isKnown => known != null;

  /// 线上原文（日志 / UI 兜底展示用）。
  String get wire => known?.wire ?? other!;

  Object? toJson() => wire;

  @override
  bool operator ==(Object other) =>
      other is OpenEnum<K> &&
      other.known == known &&
      // 这里的 this 不能省：参数名被基类签名钉死成 other，与字段同名。
      other.other == this.other;

  @override
  int get hashCode => Object.hash(known, other);

  /// **手写而非 derive**：`Other` 只打前 [llmEnumDebugHeadBytes] 字节加总长。
  ///
  /// 收进来的 `Other` 长度不受本端控制，朴素地整串打出来正好绕开「审计日志只记白名单」
  /// 那道兜底——golden 的 `redact_open_enum_other.json` 把假密钥藏在第 16 字节之后，
  /// 就是来钉这一条的。
  @override
  String toString() {
    if (known != null) return 'OpenEnum.${known!.wire}';
    final String s = other!;
    final int bytes = utf8Length(s);
    final String head = clampUtf8(s, llmEnumDebugHeadBytes);
    final String ellipsis = bytes > llmEnumDebugHeadBytes ? '…' : '';
    return 'OpenEnum.Other("$head$ellipsis", ${bytes}B)';
  }
}

/// 一个开放式枚举的编解码器（已知值表 + 字段名，用于报错定位）。
final class OpenEnumCodec<K extends WireNamed> {
  const OpenEnumCodec(this.values, this.label);

  final List<K> values;
  final String label;

  /// 线格式 → 模型。**非字符串一律报错**，不宽容成 `Other`。
  ///
  /// 这不是苛刻：Rust 侧 `untagged` 对非字符串同样是整帧 `Err`，宽容成 `Other("42")`
  /// 反而让两端不一致。golden 的 `compat_open_enum_shape_change_is_fatal.json` 期望
  /// 的正是这个异常。
  OpenEnum<K> decode(Object? json) {
    if (json is! String) {
      _fail('$label 期望字符串，实际是 ${_kindOf(json)}');
    }
    return fromWireClamped(json, clamp: false);
  }

  /// 已知就收敛成 `Known`（不留两个等价表示），不认识才落 `Other`。
  OpenEnum<K> fromWireClamped(String wire, {bool clamp = true}) {
    for (final K k in values) {
      if (k.wire == wire) return OpenEnum<K>.known(k);
    }
    return OpenEnum<K>.other(clamp ? clampUtf8(wire, llmEnumWireMaxBytes) : wire);
  }

  /// 必填字段。
  OpenEnum<K> read(JsonMap m, String key) {
    if (!m.containsKey(key) || m[key] == null) {
      _fail('字段 "$key" 缺失（$label 是必填）');
    }
    return decode(m[key]);
  }

  /// 可选字段（`Option` + `skip_serializing_if`）。
  OpenEnum<K>? readOpt(JsonMap m, String key) =>
      m[key] == null ? null : decode(m[key]);
}

// ── 封闭式 C 型枚举（控制面专用，**没有 Other 兜底**）────────────────────────

/// 读一个**封闭**的 C 型枚举：未知取值直接报错。
///
/// 与上面那套 [OpenEnum] 正好相反，而两者的差别是有依据的、不是随手定的：
///
/// | | 开放式（`LlmStopReason` 一类） | 封闭式（`DenyReason` 一类） |
/// |---|---|---|
/// | 在哪层 | LLM 子协议（数据面） | 远程控制协议（控制面） |
/// | 上游 | CLI 随时会长新取值 | 只有本仓库自己会改 |
/// | 未知取值 | 落 `Other(原文)`，正文照常显示 | **整条消息报废** |
///
/// 控制面选封闭，不是因为「不会加取值」，而是因为**加取值本身就被禁止了**：
/// `DenyReason` 随 `ControlDenied` 下行，加一个变体会让老客户端整条解析失败，
/// 丢掉的正是它**唯一**的拒绝回执 —— UI 从此永久转圈。所以这里宽容成 `Other` 没有意义：
/// 真出现未知取值时，本端已经没有正确行为可做了，报错至少会被日志记下来。
T readClosedEnum<T extends WireNamed>(
  JsonMap m,
  String key,
  List<T> values,
  String label,
) {
  final String wire = readString(m, key);
  for (final T v in values) {
    if (v.wire == wire) return v;
  }
  _fail(
    '$label 收到未知取值 "$wire"（本枚举没有 Other 兜底：'
    '要么对端版本比本端新，要么协议被改坏了）',
  );
}

// ── 内部标签枚举（`{"kind": "...", ...}` / `{"op": "...", ...}`）──────────────

/// 内部标签枚举的解码骨架。
///
/// 标签值不在 `cases` 里时返回 `fallback`（对应 Rust 的 `#[serde(other)]`）——
/// **不是**报错，也**不是** null。未知 body 连同标签一起丢弃，不保留、不回显：
/// 任何「先收下来、以后再说」的容器都是白名单转发的后门。
T decodeTagged<T>(
  Object? json, {
  required String tagKey,
  required String label,
  required Map<String, T Function(JsonMap)> cases,
  required T fallback,
}) {
  final JsonMap m = asJsonMap(json, label);
  final Object? tag = m[tagKey];
  if (tag is! String) {
    _fail('$label 的标签字段 "$tagKey" 期望字符串，实际是 ${_kindOf(tag)}');
  }
  final T Function(JsonMap)? parse = cases[tag];
  if (parse == null) return fallback;
  return parse(m);
}

/// 内部标签枚举的编码骨架：把标签塞进 body 的同一层。
JsonMap taggedJson(String tagKey, String tag, [JsonMap? body]) =>
    <String, Object?>{tagKey: tag, ...?body};
