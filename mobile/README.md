# `mobile/` —— Lumen 移动端（Flutter）

把手机变成 Lumen 远程控制协议的**第三种控制端**：不看终端画面，只看结构化的 AI 对话。
里程碑 M7，设计蓝图见 `docs/M7-移动端LLM远程控制-设计蓝图-2026-08-08.md`。

**当前状态：M7 片 0b「工程地基」。** 只有工程骨架、依赖锁版、脚本与 CI，**没有任何业务功能**。
业务分片：片 1-Dart（协议层）→ 片 5（登录/设备/配对/WS）→ 片 6（对话归约）→ 片 7（渲染）。

---

## ⚠ 本机环境三个坑（不看这段，下一个人一定会卡住）

作者的开发机上有一个**常驻 http 代理**（`all_proxy=http://127.0.0.1:7897`）。它对 Flutter
工具链有三个已知影响，每一个的症状都是「卡住不动、没有报错」，靠猜是猜不出来的：

| # | 什么时候 | 症状 | 解法 |
|---|---|---|---|
| 1 | `flutter test` | **无输出卡死**，不超时，Ctrl-C 才停。`flutter test` 起一条**本机** socket 让测试宿主与被测 isolate 通信，代理变量被继承进去之后这条本机连接也去走代理，永远等不到握手 | 把代理变量从子进程环境里摘掉。`env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY -u all_proxy flutter test`，或直接用 **`tool/test.ps1`**（已包掉） |
| 2 | `flutter pub add` / `flutter pub get` | 解析依赖时长时间无响应 | 走镜像：`PUB_HOSTED_URL=https://pub.flutter-io.cn`（引擎产物另有一个源：`FLUTTER_STORAGE_BASE_URL=https://storage.flutter-io.cn`）。两个都要设，缺哪个就在对应步骤上卡 |
| 3 | 用 `curl` 打**本机**服务（调服务端时） | 请求被送去代理，打不到 `127.0.0.1` | `curl --noproxy "*" ...` |

**CI 上没有代理**，所以 `.github/workflows/mobile.yml` 直接跑 `flutter analyze` / `flutter test`，
不照抄这里的 `env -u`。两边都成立是刻意的：`env -u` 在没有代理的机器上是无操作，
所以不存在「本地能过、CI 不能过」的分叉。

### 还有一个与代理无关、但同样只在本机出现的坑

**PATH 里有两个 `dart`**：独立的 `D:\backup\flutterSdk\...\dart.exe` 是 **3.4.3**，
而 Flutter 3.38.3 自带的是 **Dart 3.10.1**。用前者跑 `dart pub` 会解析出与 `pubspec.yaml`
的 `sdk: ^3.10.1` 不符的依赖版本，并报**误导性**的错误。
⇒ **一律用 `flutter pub` / `flutter test`，不要直接敲 `dart pub` / `dart test`。**

---

## 常用命令

```powershell
# 装依赖（第一次 clone 之后，或改过 pubspec.yaml）
.\tool\test.ps1 -Pub

# 静态分析（CI 第一道门，同参数）
.\tool\test.ps1 -Analyze

# 跑测试
.\tool\test.ps1
.\tool\test.ps1 test/protocol/golden_corpus_path_test.dart     # 只跑一个文件

# 协议对齐守卫：先跑 Rust 侧 golden，再跑 Dart 侧
.\tool\gen_golden_check.ps1
```

bash / WSL 用 `./tool/test.sh`（`--pub` / `--analyze` 同义）。

原始命令（不想用脚本时照抄，**三段前缀缺一不可**）：

```bash
# 装依赖
env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY -u all_proxy \
    PUB_HOSTED_URL=https://pub.flutter-io.cn \
    FLUTTER_STORAGE_BASE_URL=https://storage.flutter-io.cn \
    flutter pub get

# 跑测试
env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY -u all_proxy flutter test
```

---

## 目录树（§7.2）

```text
mobile/
├─ pubspec.yaml            # 依赖分层选型，每条都写了「为什么是它 / 否决了谁」
├─ pubspec.lock            # ★ 入库（见下）
├─ analysis_options.yaml   # flutter_lints + strict-casts/inference/raw-types
├─ tool/
│  ├─ test.ps1             # 包掉代理坑的 flutter test / analyze / pub get
│  ├─ test.sh              # 同上，bash 版
│  └─ gen_golden_check.ps1 # 先 Rust 侧 golden 再 Dart 侧
├─ lib/
│  ├─ main.dart            # 只做 ProviderScope + LumenApp，不放初始化逻辑
│  ├─ app.dart             # MaterialApp.router + 主题
│  ├─ router.dart          # go_router（片 0b 只有一条占位路由）
│  ├─ core/                # env / log / Result（sealed Ok|Err，禁 throw 穿层）
│  ├─ protocol/            # ★ 零 Flutter 依赖，纯 dart:core/convert，可单测
│  ├─ net/                 # rest_client / ws_client / backoff
│  ├─ data/                # chat_db(_sqflite) / token_store / device_identity
│  ├─ domain/              # chat_item / tool_render / line_diff
│  ├─ state/               # providers / link_controller / conversation_controller
│  └─ ui/                  # login devices pair pick chat common
├─ test/                   # 纯 Dart + widget 测试
├─ integration_test/       # 真机集成（CI 不跑）
└─ android/  ios/          # 平台壳
```

`lib/` 下每个子目录都有一份 `README.md`，写了该层的职责、计划文件、以及**该层特有的坑**。
动手写代码前先读对应那份。

### 分层铁律：`protocol/` 零 Flutter 依赖

`lib/protocol/` 只能用 `dart:core` / `dart:convert` / `dart:typed_data`。两个理由：
① 这一层要能在纯 Dart 测试环境跑；② 这是「P2+ 若改成 Rust 内核 + 窄门面」的**隔离边界**
（§7.10）——届时只换 `protocol/` 与 `net/` 两层，`domain/` / `state/` / `ui/` 一行不动。
Flutter 类型漏进来这条边界就没了，而且是悄悄没的。

---

## 拍板记录（这几条不要再重新讨论）

### `pubspec.lock` **入库**（§15.2-6 的未决项在此拍板）

这是 App 不是 library：lock 入库才有可复现构建，出问题时能确定「昨天能跑的那份依赖」到底是哪份。
库才需要靠 caret 让下游自由解析，App 没有下游。
⇒ 根 `.gitignore` 里**刻意没有** `mobile/pubspec.lock` 这条规则。

#### ⚠ lock 里的 `url` 必须是 `https://pub.dev`，一条都不许留镜像地址

`PUB_HOSTED_URL` 该做的事是**改下载源**，但 `flutter pub get` 会把当前值**写回 lock 的每一条
`url`**。而 pub 把 hosted URL 当作依赖身份的一部分——url 不同即视为不同来源。CI 跑的是
`flutter pub get --enforce-lockfile` 且刻意不设 `PUB_HOSTED_URL`，于是：

```
Would change 117 dependencies.
Unable to satisfy `pubspec.yaml` using `pubspec.lock`.   # exit 65
```

版本号一个没变，**唯一的差异就是 URL**——`git diff` 看上去像「没变化」，而 `mobile.yml`
第一步 100% 必红，且要推上去才发现。所以：

- **别裸敲 `flutter pub get`**，走 `.\tool\test.ps1 -Pub` / `./tool/test.sh --pub`
  ——两个脚本在 `pub get` 成功之后会立刻把 lock 归一化回 `pub.dev`；
- 兜底断言在 `test/pubspec_lock_test.dart`，本地跑 `.\tool\test.ps1` 就会红；
- 手动修也行：把 117 处 `https://pub.flutter-io.cn` 全量换成 `https://pub.dev`，
  **`sha256` 不用改**（镜像与源站的归档字节一致，本机 Pub 缓存照样命中）。

### 包名 `io.github.jimhy.lumen`（§7.1，**一经上架不可改**）

`flutter create --org io.github.jimhy --project-name lumen_mobile` 的默认派生值是
`io.github.jimhy.lumen_mobile`（iOS 侧是 `io.github.jimhy.lumenMobile`），**已在片 0b 改掉**。
改动落在四处，漏一处就装不上／签名对不上：

- `android/app/build.gradle.kts` 的 `namespace` 与 `applicationId`
- `android/app/src/main/kotlin/io/github/jimhy/lumen/MainActivity.kt` 的 `package`
- 上面那个文件**所在的目录**（Kotlin 要求目录与包名一致）
- `ios/Runner.xcodeproj/project.pbxproj` 的 6 处 `PRODUCT_BUNDLE_IDENTIFIER`

### `mobile/` 下**不放任何 `Cargo.toml`**（§7.1 拍板 K11）

根 `[workspace]` 段只有 `resolver` + `members`，**既无 `exclude` 也无 `default-members`**。
任何 `Cargo.toml` 掉进 `mobile/` 都会二选一踩坑：加进 `members` → CI 的
`cargo clippy --workspace --all-targets -- -D warnings` 与发版的**无 `-p` 的 `cargo build --release`**
会在桌面目标上编它；不加又不写 `exclude` → 该目录下跑任何 cargo 命令直接报
「current package believes it's in a workspace when it's not」。

这条同时是「不用 flutter_rust_bridge」的落地形态之一，完整的账见 §4.2。

### 平台：P0 只发 Android，`ios/` 建了但不配签名

`flutter create --platforms=android,ios`，**没有生成 web / windows / linux / macos**。
理由：手机伴侣 App 用不上桌面壳，而 Flutter 的 Windows 产物还会撞上根 `.gitignore` 里
既有的无锚点规则 `*.pdb`。

iOS 出不了包是因为**作者本机是 Windows**（R12）：iOS 构建产物只能在 macOS 上产出。
`ios/` 先建好保持可编译，但不进发布流水线，也不配签名。

### `release` 用 debug 密钥签（临时）

正式签名密钥（`*.jks` / `key.properties`）绝不入库（公开仓库）。当前 `release` buildType 沿用
debug 签名，**后果要说清楚：签出来的 APK 不能上架、也不能覆盖安装正式版**，只够两机肉眼验收。
发布渠道本身仍是未决问题（§15.2-9）。

### 推迟的依赖（**不是漏了**）

- **二维码扫描**（`mobile_scanner` 等）：配对页要不要扫码取决于片 5 的交互定稿；它会引入相机权限
  与 minSdk 抬升，在功能定稿前装进来是净负债。**配对本身用 9 位数字码就能完成**
  （`SubmitPairing{code}`），扫码是加速器不是必需品。
- **推送**（FCM / 厂商通道）：P2 的事，且国内 Android 短期做不到（R13：FCM 大陆不可用、
  厂商通道要求已上架 + 企业资质）。**别把「手机后台收 LLM 完成通知」写进承诺。**

---

## 依赖清单（选型理由写在 `pubspec.yaml` 的注释里）

| 层 | 包 | 版本 |
|---|---|---|
| 状态管理 | `flutter_riverpod` | `^3.3.2` |
| 路由 | `go_router` | `^17.4.0` |
| REST | `dio` | `^5.11.0` |
| WS | `web_socket_channel` | `^3.0.3` |
| 连通性 | `connectivity_plus` | `^7.3.1` |
| 持久化 | `sqflite` / `path` | `^2.4.2+1` / `^1.9.1` |
| 偏好 | `shared_preferences` | `^2.5.5` |
| 凭据 | `flutter_secure_storage` | `^11.0.0` |
| 设备身份 | `device_info_plus` / `crypto` / `uuid` | `^13.2.0` / `^3.0.7` / `^4.6.0` |
| 工具 | `collection` | `^1.19.1` |
| **Markdown** | `flutter_markdown_plus` | **`1.0.12`（精确锁版，无 caret）** |
| dev | `flutter_lints` / `sqflite_common_ffi` | `^6.0.0` / `^2.4.0+3` |

Markdown 那条为什么单独锁死：官方 `flutter_markdown` 已归档，社区 fork 生态不稳（R14），
一个 patch 版就可能改渲染行为。全 App 只有 `lib/ui/common/markdown_view.dart` 一个文件
import 它，换实现只改那一处。

---

## 协议对齐（**上手前必读**）

移动端**不用** flutter_rust_bridge，手写 Dart 模型，靠 golden 语料锁死两端不漂移（§4.2）。

- 语料在 `crates/lumen-protocol/tests/golden/mobile/`（**61 个文件**）。
  **不要复制一份进 `mobile/`**——复制品会漂移，直接按相对路径读。
- 那个目录的 `README.md` 是**给 Dart 实现者的契约说明书**，逐条写了等价判定规则、
  四个 Dart 专属的坑、以及「往返断言测不出来的那一半」。**动手写协议模型前先读完它。**
- 权威定义是 `crates/lumen-protocol/src/llm.rs`（3271 行）。设计蓝图 §5.11 的骨架是草案、
  可能落后于代码——**冲突时以 `llm.rs` 为准**。
- 加新变体的顺序是硬的：**先加语料（两侧同时红）→ 改 `llm.rs` → 改 Dart 模型**，
  三处必须在同一个 PR 里。

---

## CI

| workflow | 触发 | 跑什么 |
|---|---|---|
| `.github/workflows/mobile.yml` | `mobile/**` 或 `crates/lumen-protocol/tests/golden/**` 变更 | `flutter pub get --enforce-lockfile` → `flutter analyze --fatal-infos` → `flutter test` |
| `.github/workflows/ci.yml` | 除 `mobile/**` / `docs/**` 之外的变更 | Rust 三平台 build + test + clippy |

**第一步 `flutter pub get --enforce-lockfile` 是一道严格校验**：`pubspec.lock` 里的 hosted
`url` 必须是 `https://pub.dev`，留了镜像地址这一步就红（详见上面「lock 里的 `url` 必须是
`https://pub.dev`」）。

Flutter 版本在 `mobile.yml` 里 **pin 死 3.38.3**，与 Rust 工具链 pin 到 1.97.0 同一个理由：
滚动 stable 会因新版工具引入的 lint 让 main 无端变红。要升级：改那里的版本号 + 本地同版验证后再提。
