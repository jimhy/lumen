import java.util.Properties

plugins {
    id("com.android.application")
    id("kotlin-android")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

// ── 上架签名（片 12）─────────────────────────────────────────────────────────
//
// 密钥**绝不入库**（根 .gitignore:72/75/76 挡着 key.properties / *.jks / *.keystore）。
// 出正式包的机器上手工放一份 android/key.properties，格式见 key.properties.example。
//
// 没有那份文件时**回退 debug 签名**：用 debug key 签出来的 APK 装得上、也能跑，
// 但**不能上架、也不能覆盖安装正式版**。这正是「看起来成功了」的降级，
// 协作铁律 6 要求它必须有声。
//
// ⚠ **说这句话的不是这里**：下面那条 `logger.warn` 在 `flutter build` 下会被过滤掉
// （实测一行不出），所以真正的执行者是 `tool/build.sh` / `tool/build.ps1`——
// 它们自己查 key.properties，在构建前后各喊一遍。**出包一律走那两个脚本。**
val keystorePropertiesFile = rootProject.file("key.properties")
val keystoreProperties = Properties().apply {
    if (keystorePropertiesFile.exists()) {
        keystorePropertiesFile.inputStream().use { load(it) }
    }
}
val hasReleaseKeystore = keystorePropertiesFile.exists()

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

    signingConfigs {
        // 只有真放了 key.properties 才建这个配置。无条件建一个「字段全 null」的
        // release 配置，AGP 会在**配置阶段**就报错，连 `flutter test` 前的
        // `flutter pub get` 都可能被牵连——而绝大多数开发机上本来就没有密钥。
        if (hasReleaseKeystore) {
            create("release") {
                keyAlias = keystoreProperties.getProperty("keyAlias")
                keyPassword = keystoreProperties.getProperty("keyPassword")
                // 路径相对 android/ 目录解析（rootProject 就是它）。
                storeFile = rootProject.file(keystoreProperties.getProperty("storeFile"))
                storePassword = keystoreProperties.getProperty("storePassword")
            }
        }
    }

    buildTypes {
        release {
            if (hasReleaseKeystore) {
                signingConfig = signingConfigs.getByName("release")
            } else {
                // 这条降级的后果要说全：debug key 签出来的 APK **不能上架、也不能覆盖安装
                // 正式版**，只够两机肉眼验收（蓝图 §14-5）。发布渠道本身仍是未决问题
                // （§15.2-9：GitHub Release 附 APK / Play 内测轨道 / 国内市场），
                // 定了之后这里还要再动一次。
                //
                // ⚠ **这行 warn 在 `flutter build` 下看不见**（实测：flutter 会过滤 gradle
                // 的输出，只放行它自己认识的那几类）。所以铁律 6 要的「有声」**不靠这里**，
                // 靠 `tool/build.sh` / `tool/build.ps1` —— 它们在 release 模式下自己查
                // key.properties，并在构建结束后再喊一遍。留着这行是为了裸敲 gradlew 的场合。
                logger.warn(
                    "⚠ 找不到 android/key.properties，release 包将用 debug 密钥签名：" +
                        "不能上架、不能覆盖安装正式版。正式出包请按 key.properties.example 放好密钥。",
                )
                signingConfig = signingConfigs.getByName("debug")
            }
        }
    }
}

flutter {
    source = "../.."
}
