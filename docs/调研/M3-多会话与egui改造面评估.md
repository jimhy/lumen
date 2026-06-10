# M3 评估纪要：多会话改造面 + egui 集成接缝

精读范围：`crates/lumen-app/src/main.rs`（735 行全文）、`crates/lumen-pty/src/lib.rs`、`crates/lumen-renderer/src/lib.rs`、`crates/lumen-term/src/term.rs` 公共接口。

---

## 一、单会话 → 多会话改造面

### 1. 字段归属拆分（AppState → `Vec<Session>` + 全局壳）

AppState（main.rs:91-127）需拆成 **per-session** 与 **全局** 两层：

**应进入 `Session` 结构的字段（逐一点名）**
| 字段 | 位置 | 说明 |
|---|---|---|
| `term: Terminal` | main.rs:98 | parser+grid+scrollback+blocks+title+responses 全在 Terminal 内部（term.rs:12-14），天然按会话独立，含滚动位置（grid 的 display_offset） |
| `pty: PtySession` | main.rs:99 | 每会话一个，Drop 自动杀子进程（lumen-pty lib.rs:147-151） |
| `pty_rx: Receiver<PtyEvent>` | main.rs:100 | 见第 2 点，建议合并为全局带 id 通道，此字段消失 |
| `last_esu_mark: u64` | main.rs:112 | 跟随各自 VT 流的 DEC 2026 协议状态，必须 per-session |
| `cursor_displayed` / `cursor_frozen_at` | main.rs:117-119 | 光标防抖是各终端自己的状态 |
| `selection` / `selecting` | main.rs:123-124 | 选区按绝对行号挂在各自 scrollback 上 |
| `selected_block: Option<u64>` | main.rs:126 | 块 id 仅在所属 term 内有效 |
| `redraw_at`/`redraw_hard_at`/`redraw_abs_at` | main.rs:106-110 | 语义上是「该会话的渲染计划」，建议 per-session 保存但**只有激活会话的计划生效**（见第 3 点） |
| `last_key_at` | main.rs:114 | 埋点，跟随激活会话即可，放全局也行 |

**保持全局的字段**：`window`、`renderer`、`wake_pending`（合并通道方案下全局唯一）、`modifiers`、`clipboard`、`mouse_pos`、`perf`/`perf_t0`、`last_render_at`（main.rs:95，是窗口级 8ms 渲染限频基准，main.rs:406-413 用它合并积压帧——单 surface 单窗口，必须全局）。

新增全局字段：`sessions: Vec<Session>`、`active: usize`、自增 `next_session_id: u64`。

### 2. PTY 读线程与唤醒机制

- **读/写线程已经天然 per-session**：`PtySession::spawn` 内部各起 reader/writer 线程（lumen-pty lib.rs:76-109），无需改。
- **转发线程**（main.rs:278-292）目前每会话一条，建议保留 per-session 转发线程，但**汇入同一条全局通道**，事件改为 `(SessionId, PtyEvent)`：
  - `EventLoop<PtyWake>` 的 user event **保持无数据**（main.rs:48-49 的去重设计不变），`wake_pending` 全局一个即可——`user_event` 里一次 drain 全部会话的积压（main.rs:357 的 `try_recv` 循环改为按 sid 路由到 `sessions` 里对应 term）。
  - 这是改动最小的方案：唤醒去重协议零变化，只是 channel 元素加 id。drain 到已关闭会话的 sid 直接丢弃（关 tab 后管道内残留数据）。
- `PtyEvent::Exited`（main.rs:437-440 现在直接退出进程）改为：关闭该 tab；最后一个 tab 关闭才退出（或显示空态页）。

### 3. 各机制的 per-session / 全局归属

| 机制 | 归属 | 说明 |
|---|---|---|
| resize（main.rs:481-490） | **全部会话** | 所有 tab 共享同一终端视口矩形，窗口/面板宽度变化时须对每个会话做 `term.resize + pty.resize`（懒 resize 会让后台 TUI 在切换瞬间花屏，不推荐）。注意 M3 下行列数不再来自整窗，而是 egui 布局算出的终端区矩形（见二.2） |
| ESU 直渲（main.rs:400-419） | per-session 判定，**仅激活会话触发 request_redraw** | `esu_mark()`/`is_synchronized()` 是 per-term 状态（term.rs:154-171）；后台会话完成同步帧不应打扰渲染，只更新自己的 `last_esu_mark` |
| 冻结超时 CURSOR_FREEZE_CAP（main.rs:42, 690-703） | per-session | 防抖状态在 Session 里；RedrawRequested 只对激活会话执行该逻辑 |
| Mailbox 限频 / 8ms 合帧（main.rs:406-413；renderer lib.rs:92-96） | **全局** | 单 surface 单 present 流水线 |
| about_to_wait 调度（main.rs:443-472） | 全局收敛 | 只看激活会话的 `redraw_at/hard/abs` 三元组；切 tab 时清掉旧计划、立即 request_redraw |
| `take_responses` 回写（main.rs:381-384） | **每个会话每次 drain 都必须执行** | 后台会话的 DSR/DA 应答不回写，里面跑的程序会卡死——这是后台消化的硬要求 |
| 窗口标题（main.rs:390-394） | 激活会话 | `term.title()` 改为喂 tab 标签 |

### 4. 后台会话「消化不渲染」

改造点很集中，现有架构已经把「喂数据」和「渲染调度」分在两段：
- main.rs:357-377 的 drain + `term.advance` 对所有会话照常执行（含 take_responses、alt-screen 清选中块 main.rs:387-389）。
- main.rs:379-436 的渲染调度段加一个 `if sid == active` 闸门即可；后台会话只更新 `last_esu_mark`，可顺带记 `has_unseen_output` 给 tab 加小红点。
- **背压天然隔离**：每会话独立 bounded(128)+bounded(256) 通道（lumen-pty lib.rs:90、main.rs:274），后台刷屏只阻塞它自己的读线程。
- **风险**：`advance()` 在主线程跑，后台 `yes` 级输出会抢占主线程拖慢前台打字。缓解：每次 drain 给后台会话设字节上限（剩余留到下一个 wake）。
- renderer 的 `row_segs` 行缓存（lib.rs:42-45）按行哈希命中，切 tab 后全部 miss、整屏重排一次（约几十行），可接受；若要丝滑可把缓存搬进 Session 或按 sid 建 map。

### 5. 工作量与风险

- **工作量**：约 2-3 人日。主体是机械重构（`state.term` → `state.active_mut().term`，main.rs 全文约 60 处引用）+ 通道加 id + tab 生命周期。风险低，M1/M2 核心逻辑不动。
- **风险**：(a) 关 tab 时 PtySession Drop 杀进程与读线程退出的竞态——已有 Exited 路径兜底；(b) 多会话同时高产出时 drain 循环时长涨，REDRAW_HARD_CAP（main.rs:33）的 30fps 保障可能被挤压；(c) 切 tab 瞬间 `cursor_frozen_at`/`redraw_*` 残留要清理，否则借用上个会话的冻结计划。

---

## 二、egui 集成接缝

### 1. 事件路由（window_event 接入点）

- 在 `window_event`（main.rs:474）入口先调 `egui_winit::State::on_window_event(&window, &event)`，拿 `EventResponse { consumed, repaint }`：
  - `repaint` → request_redraw；`consumed` → 跳过终端处理（但 `Resized`/`CloseRequested`/`RedrawRequested` 永远自己处理）。
  - **键盘**：`consumed` 对键盘事件等价于 `ctx.wants_keyboard_input()`（egui 有文本控件聚焦时）。终端无 egui 焦点时 `consumed=false`，现有键→PTY 路径（main.rs:491-604）原样保留。注意 egui 的 Tab 键焦点导航可能偷走终端的 Tab，需要在无控件聚焦时绕过 egui 或禁用其 Tab 导航。
  - **鼠标**：`consumed` 在指针悬于 egui 面板上时为 true；终端选区/块点击/滚轮（main.rs:605-675）以 `!consumed` 为闸。另需把 `cell_at` 改为相对终端区矩形（renderer lib.rs:164-172 目前以整窗+padding 计算）。
  - 自有快捷键（Ctrl+↑/↓ 跳块、Ctrl+C/V，main.rs:533-581）应在 egui 未聚焦文本框时优先于 egui 处理。

### 2. 渲染合流

现有 `Renderer::render`（lib.rs:190-533）一手包办 acquire→pass→present，必须拆开。推荐**方案 A（两 pass 同 surface，先终端后 egui）**，改动小：

1. surface/device/queue 所有权上移：Renderer 拆出 `GpuContext`（instance/device/queue/surface/config，lib.rs:32-35），终端画笔与 `egui_wgpu::Renderer` 共用（lumen-renderer 已 `pub use wgpu`，lib.rs:16，保证 wgpu 29 同版本同类型）。
2. 帧流程改为：app 层 acquire frame → 终端 pass（LoadOp::Clear，只画终端区）→ egui pass（LoadOp::Load，画左 tab 栏/文件树/顶栏/弹窗）→ present。中央终端区用 `CentralPanel + Frame::NONE` 透出底下的终端画面。
3. `render()` 签名加矩形：`render(&mut self, term, …, rect: Rect, encoder: &mut CommandEncoder, view: &TextureView)`；内部 `padding` 起点（lib.rs:215-216 等约 15 处坐标计算）改为 `rect.origin + padding`；`grid_size()`（lib.rs:155-161）改为按 rect 宽高算行列。
4. **布局→行列数回路**：终端矩形由 egui 当帧布局决定（面板可拖宽），帧末检测矩形变化 → `term.resize + pty.resize`（全部会话）→ 再补一帧。会有一帧延迟，可接受。
5. 帧调度合流：`about_to_wait`（main.rs:443-472）里把 egui 的 `ctx.requested_repaint_after` 与 `redraw_at` 取 min；ESU 直渲路径不变（request_redraw 即整帧 egui run + 终端 paint）。**代价**：每个终端帧都多跑一次 egui 布局（约 0.1-1ms），打字延迟路径变重，需用 LUMEN_PERF 埋点（main.rs:131-137）回归验证。

方案 B（egui paint callback 把终端画进 CentralPanel 的 clip rect）更正统但要把 glyphon prepare/render 拆进 `CallbackTrait` 的 prepare/paint 两段、资源挪进 `CallbackResources`，重构量大，建议 M3 后期再演进。

### 3. IME 冲突点

- **冲突一（最关键）**：egui-winit 会按「是否有 egui 文本控件聚焦」自动调用 `window.set_ime_allowed(...)`，无聚焦时**关掉 IME**——直接打死终端的中文输入（现在 main.rs:255 全局开启）。必须在每帧 `handle_platform_output` 之后、终端持有焦点时强制 `set_ime_allowed(true)` 复位。
- **冲突二**：`Ime::Commit` 现在无条件写 PTY（main.rs:658-663）。登录/设置弹窗的输入框聚焦时 commit 会被双投（egui 收一份、PTY 收一份）。按 `consumed`/`wants_keyboard_input` 路由：egui 聚焦则不写 PTY。
- **冲突三（候选框定位）**：egui-winit 会为自己的文本控件调 `set_ime_cursor_area`；终端目前从未设置，候选框会停在 egui 上次设置的位置。需在终端聚焦时按光标格子坐标（cell_size × cursor 位置 + 终端区原点）调用 `set_ime_cursor_area`，且在 egui 之后调用以覆盖。
- 现状备注：终端只处理 `Ime::Commit`、不渲染 Preedit（main.rs:658，无 Preedit 分支），与 egui 无绘制冲突；M3 可顺带补终端 Preedit 内联显示。

### 4. 风险清单

1. **IME allowed 被 egui-winit 反复翻转**：行为随 egui 版本变化，需在 0.34 上实测验证复位时序（最高风险项）。
2. **键盘焦点模型**：终端是「无 egui 控件的区域」，egui 不会为它持有焦点；需要自建「终端聚焦」布尔（点击终端区获焦/点击面板失焦），所有键盘与 IME 路由都依赖它，漏一处就出现按键双投或失灵。
3. **打字延迟回归**：每帧叠加 egui 布局成本，可能侵蚀此前 ESU 直渲 + 8ms 合帧换来的手感（main.rs:400-419），需埋点对比。
4. **ControlFlow 粘性陷阱重现**：egui 动画的 repaint_after 与现有 WaitUntil 调度（main.rs:447-471 注释里记载过空转事故）合流时，遗漏复位 Wait 会再次单核拉满。
5. **srgb/混合差异**：egui 与 rect 管线（rect.rs）同一 surface format 下的半透明叠加（选中块 tint lib.rs:236-240）颜色可能偏差，需目检。
6. **版本锁**：egui-wgpu 依赖的 wgpu 必须与 workspace 的 wgpu 29 严格同版本，否则类型不互通（编译期即暴露，风险可控）。

### 工作量（M3 外壳整体）

多会话重构 2-3 天；Renderer 拆分 + egui 两 pass 合流 2-3 天；输入/IME 路由与手感回归 1.5-2 天；tab 栏/文件树/头像/设置 UI 本体 3-4 天。合计约 **9-12 人日**，最大不确定性在 IME 复位时序与打字延迟回归。
