/// 控制面拒绝 / 失败的**用户文案**，集中一处。
///
/// ## 为什么不散在各页面里
///
/// 这些文案不是装饰：它们是用户唯一能看到的「为什么没连上」。散在各页面就一定会出现
/// 「同一个拒因在两页里说法不同」，而其中两条**说错了会把用户引到错误的方向**：
///
/// - [`DenyReason.alreadyControlled`] 必须**给出具体数字**。只说「已被占用」，
///   用户会去找一个并不存在的占用者——隐藏会话在被控端**没有任何界面痕迹**。
/// - [`DenyReason.offline`] 必须**同时覆盖「版本过低」**。服务端在目标未上报 `hidden`
///   能力位时复用了这条拒因（新增拒因会打掉老客户端唯一的回执），所以「不在线」与
///   「那台 PC 的 Lumen 太旧」在线上是同一个值。
library;

import 'package:lumen_mobile/protocol/pairing_qr.dart';
import 'package:lumen_mobile/protocol/remote_s2c.dart';
import 'package:lumen_mobile/state/link_controller.dart';

/// 一台被控端可并存的隐藏会话上限。
///
/// ⚠ 与 Rust 侧 `MAX_HIDDEN_SESSIONS_PER_TARGET` 同源，**改那边要改这里**。
/// 它出现在文案里而不是只当判据，正是因为「已达上限（2）」比「已被占用」有用得多。
const int kMaxHiddenSessionsPerTarget = 2;

/// 发起被拒的文案。
String denyReasonMessage(DenyReason reason) => switch (reason) {
      // ★ 两种情况在线上是同一个值，文案必须都覆盖到。
      DenyReason.offline => '连不上那台电脑。请确认它的 Lumen 正在运行、已登录，'
          '并且版本不低于 v1.1',
      // ★ 必须给数字：隐藏会话在被控端没有界面痕迹，用户找不到「占用者」。
      DenyReason.alreadyControlled =>
        '该电脑的手机会话已达上限（$kMaxHiddenSessionsPerTarget）。'
            '请先在另一台手机上断开，或在那台电脑上断开。',
      DenyReason.controllerBusy => '这台手机已经连着一台电脑了，请先断开',
      // ★ 不是「已被占用」：配对码是给人念的，同一时刻那台电脑屏幕上只该有一个码。
      DenyReason.targetPairing => '该电脑正在与另一台设备配对，请稍候',
      DenyReason.crossUser => '那台设备不属于当前账户',
      DenyReason.selfControl => '不能连接手机自己',
      DenyReason.rejectedByUser => '对方在那台电脑上拒绝了这次连接',
      DenyReason.controllerLeft => '连接已取消',
      DenyReason.expired => '配对已超时，请重新发起',
      DenyReason.tooManyAttempts => '配对码输错次数过多，请重新发起',
    };

/// 本端判定的失败文案。
String linkFailureMessage(LinkFailure reason) => switch (reason) {
      // ★ 硬验收项：老服务端 / 老 PC 对新变体**完全无回音**，只能靠超时判定。
      // 文案指向版本而**不是网络**——让用户去重启路由器是最坏的引导。
      LinkFailure.serverTooOld => '服务器或那台电脑的版本过低，无法建立手机会话。'
          '请确认两端的 Lumen 都已升级到 v1.1 或更新版本。',
      LinkFailure.pairingExpired => '配对码已过期，请重新发起',
      LinkFailure.pairingTooManyAttempts => '配对码输错次数用尽，请重新发起',
      LinkFailure.pairingNoPending => '这次配对已失效，请重新发起',
      LinkFailure.linkLost => '与服务器的连接断开了，请重新发起',
    };

/// 输码失败的文案（还留在配对页时用，带剩余次数）。
String pairingFailMessage(PairingFailReason reason, int attemptsLeft) =>
    switch (reason) {
      PairingFailReason.invalidCode => '配对码不对，还可以再试 $attemptsLeft 次',
      PairingFailReason.expired => '配对码已过期',
      PairingFailReason.noPending => '这次配对已失效',
      PairingFailReason.tooManyAttempts => '输错次数用尽',
    };

/// 扫码四重校验的文案。
///
/// **每一种都不同**——合并成一句「二维码无效」，用户就永远不知道自己是扫错了设备、
/// 扫了别人的码，还是遇到了钓鱼。
String qrErrorMessage(PairingQrError error) => switch (error) {
      PairingQrError.malformed => '这不是 Lumen 的配对二维码',
      // ★ 钓鱼信号：这条是安全警告，且界面上**绝不能**给「仍要继续 / 切换服务器」的出口。
      PairingQrError.foreignServer => '⚠ 这个二维码指向另一台服务器。'
          '出于安全考虑已拒绝——如果不是你自己换的服务器，请当心。',
      PairingQrError.foreignAccount => '这是别人账户的配对码',
      PairingQrError.wrongTarget => '这个码与你要连接的电脑不符',
    };
