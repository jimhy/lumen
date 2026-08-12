plugins {
    id("com.android.application")
    id("kotlin-android")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

android {
    // 包名取 `io.github.jimhy.lumen`（设计蓝图 §7.1 拍板），**不是** flutter create 由
    // `--org io.github.jimhy` + `--project-name lumen_mobile` 派生的 `io.github.jimhy.lumen_mobile`。
    // 一经上架不可改，故在片 0b 就改掉；同步改动见 MainActivity.kt 的 package 与其所在目录、
    // 以及 ios/Runner.xcodeproj 的 PRODUCT_BUNDLE_IDENTIFIER。
    namespace = "io.github.jimhy.lumen"
    compileSdk = flutter.compileSdkVersion
    ndkVersion = flutter.ndkVersion

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = JavaVersion.VERSION_17.toString()
    }

    defaultConfig {
        // 已拍板并写死，勿改（设计蓝图 §7.1：包名一经上架不可改）。
        applicationId = "io.github.jimhy.lumen"
        // You can update the following values to match your application needs.
        // For more information, see: https://flutter.dev/to/review-gradle-config.
        minSdk = flutter.minSdkVersion
        targetSdk = flutter.targetSdkVersion
        versionCode = flutter.versionCode
        versionName = flutter.versionName
    }

    buildTypes {
        release {
            // ⚠ P0 刻意仍用 debug 密钥签 release：正式上架密钥（*.jks / key.properties）
            // 绝不入库（公开仓库，根 .gitignore 已挡），签名配置留到发布流水线里做。
            // 现状的后果要说清楚：**用 debug key 签出来的 APK 不能上架、也不能覆盖安装
            // 正式版**，只够两机肉眼验收（设计蓝图 §14-5）。发布渠道本身是未决问题
            // （§15.2-9：GitHub Release 附 APK / Play 内测轨道 / 国内市场），定了再补这里。
            signingConfig = signingConfigs.getByName("debug")
        }
    }
}

flutter {
    source = "../.."
}
