package io.github.jimhy.lumen

import android.provider.Settings
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

/// 包名是 `io.github.jimhy.lumen` 而不是 `…lumen_mobile`（`flutter create --project-name
/// lumen_mobile` 的默认派生值）：设计蓝图 §7.1 拍板 applicationId / bundleId 取
/// `io.github.jimhy.lumen`，且**一经上架不可改**。改动落在三处，缺一台设备就装不上：
/// 本文件的 package、`app/build.gradle.kts` 的 namespace + applicationId、以及本文件所在目录。
class MainActivity : FlutterActivity() {
    /// 唯一一条自定义 platform channel，只为拿 `ANDROID_ID`。
    ///
    /// **常量要与 `lib/data/platform_device.dart` 里的 `kDeviceChannelName` /
    /// `kAndroidIdMethod` 逐字对齐**。对不上的表现是 Dart 侧收 `MissingPluginException`
    /// → `hw_id` 恒 null → 幽灵设备回来，而**两侧的测试都是绿的**（Dart 测试里 channel
    /// 是假的，Kotlin 侧根本没有测试）。这是本文件唯一的风险点。
    private companion object {
        const val CHANNEL = "io.github.jimhy.lumen/device"
        const val METHOD_ANDROID_ID = "androidId"
    }

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, CHANNEL)
            .setMethodCallHandler { call, result ->
                when (call.method) {
                    // 为什么不用 device_info_plus：它从 4.0.0 起移除了 androidId，
                    // 剩下的 `AndroidDeviceInfo.id` 是 Build.ID（构建标签），同型号同 ROM
                    // 的机器完全相同——拿它当 hw_id 会让两台手机认领到同一行设备。
                    // 完整理由在 lib/data/platform_device.dart 的文件头。
                    //
                    // ANDROID_ID 不需要任何权限；Android 8.0 起按「签名密钥 + 用户 + 设备」
                    // 分配，卸载重装同签名的 App 值不变。取不到时回 null，Dart 侧会
                    // 按「无 hw_id」上报（服务端退化回按 device_id 处理，功能不受影响）。
                    METHOD_ANDROID_ID -> result.success(
                        Settings.Secure.getString(
                            contentResolver,
                            Settings.Secure.ANDROID_ID,
                        ),
                    )
                    else -> result.notImplemented()
                }
            }
    }
}
