package io.github.jimhy.lumen

import io.flutter.embedding.android.FlutterActivity

/// 包名是 `io.github.jimhy.lumen` 而不是 `…lumen_mobile`（`flutter create --project-name
/// lumen_mobile` 的默认派生值）：设计蓝图 §7.1 拍板 applicationId / bundleId 取
/// `io.github.jimhy.lumen`，且**一经上架不可改**。改动落在三处，缺一台设备就装不上：
/// 本文件的 package、`app/build.gradle.kts` 的 namespace + applicationId、以及本文件所在目录。
class MainActivity : FlutterActivity()
