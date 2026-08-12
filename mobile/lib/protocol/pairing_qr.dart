/// 扫码配对的二维码载荷与四重校验——**零 Flutter 依赖**。
///
/// 对应 Rust 侧 `crates/lumen-protocol/src/pairing_qr.rs`（**权威定义在那里**）。
///
/// ## 这个载荷不上线缆
///
/// 它只经「PC 屏幕 → 手机摄像头」这条**带外信道**传递，永远不进 WebSocket、不进 REST。
/// 但两端各有一份实现，字段名或校验顺序漂一处，扫码要么失效、要么把安全校验放空
/// ——所以它和线格式一样有 golden 语料（`qr_*.json`）钉着。
///
/// ## 扫码没有降低配对强度
///
/// 9 位配对码的强度**不在「码有多长」**，而在服务端那一行「码只能由发起配对的那台设备
/// 提交」（身份取自 JWT 而非消息自报）。把码渲染成二维码是**纯展示层替换**：owner 绑定、
/// 单次使用、5 次尝试、120 秒 TTL、同账户限制、被控端否决权全部原样保留。
///
/// ## ★ 第五重是状态闸，不在这个文件里
///
/// 相机页**只在「已收到 `PairingNeeded`」时才存在**（`LinkPairing` 状态），
/// 所以一张钓鱼码根本无处可用——用户没在配对流程里时，App 里没有任何入口去扫它。
/// 这四重校验是那道闸之后的第二层，不是唯一一层。
library;

import 'dart:convert';

import 'package:crypto/crypto.dart';
import 'package:lumen_mobile/protocol/codec.dart';

/// 载荷魔数。解析后**第一件事**就是校验它。
const String kPairingQrMagic = 'lumen.pair.v1';

/// 配对码位数。
const int kPairingCodeLen = 9;

/// 账户指纹取 sha256 hex 的前多少个字符。
const int kAccountFingerprintLen = 16;

/// 四重校验的拒绝原因。
///
/// **每一种都必须有不同的 UI 文案**——合并成一句「二维码无效」，用户就永远不知道自己是
/// 扫错了设备、扫了别人的码，还是遇到了钓鱼。
enum PairingQrError {
  /// 魔数不对 / 配对码不是 9 位纯数字 / 根本不是 JSON。
  ///
  /// 文案：「这不是 Lumen 的配对二维码」。语气平实——用户多半只是扫错了东西。
  malformed('Malformed'),

  /// origin 与本机登录的服务器不符。
  ///
  /// ★ **这是钓鱼信号**：文案要是安全警告，并且**绝不提供「是否切换服务器」的选项**。
  /// 提供了它，整条校验就变成一个「点确定即可绕过」的对话框，等于没做。
  foreignServer('ForeignServer'),

  /// 账户指纹不符。文案：「这是别人的配对码」。
  foreignAccount('ForeignAccount'),

  /// 目标设备不符。文案：「与你要连接的设备不符」。
  ///
  /// 多屏 / 多机场景下很容易发生，**不是**攻击——文案要平实、可自行纠正，
  /// 不要用 [foreignServer] 那种安全警告的语气。
  wrongTarget('WrongTarget');

  const PairingQrError(this.code);

  /// 机器可读标识（与 Rust 侧 `PairingQrError::code` 同源，golden 语料按它对账）。
  final String code;
}

/// 账户指纹：`sha256(user_id)` 的前 [kAccountFingerprintLen] 个**小写** hex 字符。
///
/// 与 Rust 侧同源。大小写、截取长度任何一处不同，都会让**所有**扫码判成
/// [PairingQrError.foreignAccount]——而那看起来像「功能没做好」，不像「两端算法不一致」。
String accountFingerprint(String userId) {
  final String hex = sha256.convert(utf8.encode(userId)).toString();
  return hex.substring(0, kAccountFingerprintLen);
}

/// 二维码载荷。字段名是单字母短名——QR 容量是硬约束（目标 < 180 字节）。
final class PairingQrPayload {
  const PairingQrPayload({
    required this.magic,
    required this.origin,
    required this.userFingerprint,
    required this.target,
    required this.code,
    required this.expiresAt,
  });

  /// 线上字段 `m`。
  final String magic;

  /// 线上字段 `o`：规范化服务端 origin。
  final String origin;

  /// 线上字段 `u`：账户指纹。**不是明文 user_id**——码会被拍照、会进录屏。
  final String userFingerprint;

  /// 线上字段 `t`：被控端 device_id。
  final String target;

  /// 线上字段 `c`：9 位配对码。**是字符串**，可能带前导零。
  final String code;

  /// 线上字段 `e`：预计过期 Unix 秒。
  ///
  /// ⚠ **只做软提示，永不硬拒**——服务端的 TTL 才是权威，两端时钟偏差不得造成
  /// 「码明明还在屏幕上、手机却说过期了」这种用户无法自助的故障。[validate] 不看它。
  /// 可以用来画倒计时。
  final int expiresAt;

  factory PairingQrPayload.fromJson(JsonMap m) => PairingQrPayload(
        magic: readString(m, 'm'),
        origin: readString(m, 'o'),
        userFingerprint: readString(m, 'u'),
        target: readString(m, 't'),
        code: readString(m, 'c'),
        expiresAt: readInt(m, 'e'),
      );

  /// 从扫到的文本还原。**不是合法 JSON / 字段缺失都抛 [WireFormatException]**，
  /// 调用方应把它转成 [PairingQrError.malformed]（同一句文案）。
  static PairingQrPayload parse(String text) {
    final Object? json;
    try {
      json = jsonDecode(text);
    } on FormatException catch (e) {
      throw WireFormatException('二维码内容不是 JSON：$e');
    }
    return PairingQrPayload.fromJson(asJsonMap(json, '二维码载荷'));
  }

  JsonMap toJson() => <String, Object?>{
        'm': magic,
        'o': origin,
        'u': userFingerprint,
        't': target,
        'c': code,
        'e': expiresAt,
      };

  /// 四重校验。通过返回 null。
  ///
  /// **顺序有讲究**：先判「形状」（魔数、码格式）再判「身份」（服务器、账户、设备）。
  /// 形状不对说明这根本不是我们的码——此时说「这是别人的服务器」既不准确又平白吓人。
  PairingQrError? validate({
    required String expectedOrigin,
    required String expectedUserFingerprint,
    required String expectedTarget,
  }) {
    if (magic != kPairingQrMagic) return PairingQrError.malformed;
    if (!_isNineDigits(code)) return PairingQrError.malformed;
    if (origin != expectedOrigin) return PairingQrError.foreignServer;
    if (userFingerprint != expectedUserFingerprint) {
      return PairingQrError.foreignAccount;
    }
    if (target != expectedTarget) return PairingQrError.wrongTarget;
    return null;
  }

  /// 9 位纯数字。
  ///
  /// **不能用 `int.tryParse`**：它放过前导正负号与空白，也会在前导零上丢信息
  /// （`"012345678"` 解出来是 `12345678`，再拼回去就少一位）。
  static bool _isNineDigits(String s) {
    if (s.length != kPairingCodeLen) return false;
    for (final int unit in s.codeUnits) {
      if (unit < 0x30 || unit > 0x39) return false;
    }
    return true;
  }

  /// 配对码不进调试渲染：它是一次性口令，进日志没有任何用处。
  @override
  String toString() =>
      'PairingQrPayload($origin, u=$userFingerprint, t=$target, code=<9 位>)';
}
