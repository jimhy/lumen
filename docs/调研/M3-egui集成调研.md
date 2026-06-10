# egui 0.34 嵌入调研纪要（Lumen M3 应用外壳）

版本基线（已在 egui 仓库 0.34.3 的 workspace Cargo.toml 中确认）：egui/egui-wgpu/egui-winit **0.34.3** → 依赖 **wgpu 29.0.1、winit 0.30.13**，与 Lumen 现有栈完全一致，无需任何版本桥接。

---

## 1. 自定义 wgpu 渲染嵌入：Callback 模式 vs 分区方案

### API 现状（源码 `egui-wgpu 0.34.3/src/renderer.rs` 确认）

```rust
// 注册：ui 布局期把回调作为 Shape 加入 painter
ui.painter().add(egui_wgpu::Callback::new_paint_callback(rect, MyCallback { .. }));

pub trait CallbackTrait: Send + Sync {
    fn prepare(&self, device: &wgpu::Device, queue: &wgpu::Queue,
        screen_descriptor: &ScreenDescriptor, egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut CallbackResources) -> Vec<wgpu::CommandBuffer> { .. }
    fn finish_prepare(&self, ..) -> Vec<wgpu::CommandBuffer> { .. }
    fn paint(&self, info: PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,   // 注意 'static（forget_lifetime 而来）
        callback_resources: &CallbackResources);
}
```

**render pass 归属**：`paint` 在 **egui 自己的主 render pass 内** 执行（官方文档原话：draw into "the same RenderPass that is used for all other egui elements"）。硬约束：
- 管线的 color target format 必须等于 egui pass 的目标格式（即 surface format），MSAA 数必须等于 `RendererOptions::msaa_samples`（默认 1），depth 仅当 `depth_stencil_format` 配置了才有；
- `paint` 内**不能**开启自己的 pass；自有 pass（如渲到中间纹理）必须放 `prepare` 里（用 `egui_encoder` 或返回自己的 `CommandBuffer`，会在 egui pass 之前提交）；
- 回调对象每帧重建（Arc 进 Shape），持久资源（TextAtlas/buffer/pipeline）放 `Renderer::callback_resources: type_map::concurrent::TypeMap` 或 App 持有的 `Arc<Mutex<..>>`。

### 对 Lumen 的推荐：**离屏纹理 + register_native_texture（方案 b）**

| 方案 | 评估 |
|---|---|
| a. Callback 直绘 egui pass | 适合"每帧从头画"的 3D 视口（官方 custom3d demo 即此类）。Lumen 的 ESU 部分重绘/damage 优化在 egui 每帧重录整 pass 下全部失效，且 format/MSAA/clip 都要迁就 egui，改造量最大 |
| **b. 离屏纹理（推荐）** | 终端管线**原封不动**：继续自管 pass、`LoadOp::Load`、damage 部分重绘、自有 atlas，只是渲染目标从 surface 换成持久 offscreen texture；egui 侧 `Renderer::register_native_texture(device, &view, FilterMode::Nearest)` 拿到 `TextureId`，工作区 `ui.image()` 画即可。z-order 天然正确（弹窗/菜单/设置页可盖在终端上），裁剪滚动全由 egui 管。尺寸变化重建纹理后用 `update_egui_texture_from_wgpu_texture()` 原地换绑（两个方法均在 0.34.3 renderer.rs:761/782 确认存在）。代价：一张终端区大小的中间纹理 + 一次全屏采样，对 GPU 终端可忽略 |
| c. 分区（egui 画周边、终端直绘 surface） | 可行但最脏：终端 rect 要从 egui 布局反推（本帧 rect 下帧才可用或同帧跑两遍 pass）、egui 弹层与终端区重叠时只能靠"egui 后画 + LoadOp::Load"保证遮挡、滚动/动画手工同步。仅当对延迟有极端要求时再考虑 |

```rust
// 方案 b 骨架
// init: 终端管线照旧，目标换成 offscreen
let tex_id = egui_renderer.register_native_texture(&device, &term_view, wgpu::FilterMode::Nearest);
// PTY 有输出时：terminal.render_damage_to(&term_view); window.request_redraw();
// egui 帧内：
egui::CentralPanel::default().frame(Frame::NONE).show(ctx, |ui| {
    let r = ui.available_rect_before_wrap();
    ui.put(r, egui::Image::new((tex_id, r.size())));
    let resp = ui.interact(r, ui.id().with("term"), egui::Sense::click_and_drag());
    if resp.clicked() { /* 焦点交给终端 */ }
});
// resize: 重建 offscreen → renderer.update_egui_texture_from_wgpu_texture(&device, &new_view, FilterMode::Nearest, tex_id);
```

---

## 2. IME：egui-winit 现状与宿主直通方案

### 现状（egui-winit 0.34.3 lib.rs 源码确认）

- `State::on_window_event` 收到 `WindowEvent::Ime` → `on_ime()` 转成 `egui::Event::Ime(ImeEvent::{Enabled,Preedit,Commit,Disabled})`，返回 `EventResponse { consumed: egui_ctx.egui_wants_keyboard_input(), repaint: true }`（lib.rs:365）。**即 egui 没有文本焦点时 consumed=false，宿主可自行处理同一事件**——这就是直通的钩子。
- `Ime::Preedit(text, Some(cursor))` 才触发 enable；`Commit` 后立即发 `Disabled`；macOS 退格删 preedit 的特例（PR #7973）和 Linux `Ime::Enabled` 语义混乱（winit #2498）都已在 0.34 内处理。
- `handle_platform_output` 按 **egui 自己**的 IME 需求调 `window.set_ime_allowed(allow)` 与 `set_ime_cursor_area(rect)`（仅状态变化时调，lib.rs:~1105）。

### 直通方案与坑

1. **焦点仲裁**：终端区只是 `ui.interact` 的普通区域而非 TextEdit，故终端聚焦时 `egui_wants_keyboard_input()==false`，`Ime`/`KeyboardInput` 的 consumed 均为 false → 在 `if !response.consumed` 分支里走 Lumen 现有 IME/键盘管线即可，无需 fork egui-winit。
2. **最大坑——set_ime_allowed 被 egui 关掉**：用户先点过 egui 的搜索框（egui 内部 `allow_ime=true`）再点回终端，`handle_platform_output` 会 `set_ime_allowed(false)` 关掉整窗 IME，终端中文输入失效。**宿主必须在每次 `handle_platform_output` 之后，若终端持焦点则重新 `window.set_ime_allowed(true)` + `window.set_ime_cursor_area(终端光标矩形)`**（egui-winit 内部有 `allow_ime` 脏标记，外部强设不会被它每帧改回，只在 egui 文本焦点变化时需要再覆盖一次——保险做法是每帧终端聚焦时都设）。
3. 即使 consumed=false，事件也已 push 进 `egui_input.events`——egui 无焦点控件时无害，不必清理。
4. Windows 细节：egui-winit 0.34 维护 `pressed_processed_physical_keys` 防止 IME 确认键（回车/空格）二次注入 egui；宿主直通路径同样要防"Commit 后紧跟的 KeyboardInput 重复"（Lumen M1 已有的处理沿用即可）。
5. `ImePurpose::Terminal` 已有映射（lib.rs:1748），可通过 viewport 命令或宿主直接 `window.set_ime_purpose(ImePurpose::Terminal)` 告知输入法处于终端语境。
6. 历史 issue 参考：egui #248（IME 总跟踪）、PR #5188/#5198（Linux X11 XIM 抢 Backspace/方向键，0.29 后修复）、winit #2888（X11 IME 不触发）、winit #3092（X11 无法禁用 IME/只支持位置不支持区域）。Windows 平台在 0.34 上无未关闭的重大 IME issue。

---

## 3. 主题定制与文件树

### Style/Visuals 能力边界（0.34.3 style.rs 字段确认）

Warp 风深色（扁平、圆角卡片、细描边、低对比分隔）**完全可达**，全部静态配置：

```rust
let mut style = (*ctx.style()).clone();
style.spacing.item_spacing = egui::vec2(8.0, 6.0);
style.spacing.button_padding = egui::vec2(10.0, 6.0);
style.spacing.indent = 14.0;
let v = &mut style.visuals;
v.panel_fill   = egui::Color32::from_rgb(0x16, 0x18, 0x21);   // 侧栏底色
v.window_fill  = egui::Color32::from_rgb(0x1d, 0x20, 0x2b);
v.extreme_bg_color = egui::Color32::from_rgb(0x10, 0x12, 0x18);
v.selection.bg_fill = egui::Color32::from_rgb(0x2a, 0x4d, 0x8f);
v.widgets.noninteractive.corner_radius = egui::CornerRadius::same(6); // 0.31 起 Rounding→CornerRadius(u8)
v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(0x23, 0x26, 0x33);
v.window_corner_radius = egui::CornerRadius::same(10);
ctx.set_style(style);
// 容器级再用 egui::Frame::new().fill(..).stroke(..).corner_radius(..).inner_margin(..) 逐面板覆盖
```

- **做不到**：渐变背景、毛玻璃/背景模糊、阴影仅单色 offset/blur/spread、无 CSS 级联（但 `ctx.set_style` 全局 + `ui.style_mut()` 局部覆盖足够）。Warp 截图里的纯色分栏/圆角块/hover 高亮均无障碍。
- **中文字体**：`FontDefinitions` + `FontData`，**`FontData.index: u32` 支持 .ttc 集合**（fonts.rs:124 确认）——可运行时 `from_owned(std::fs::read("C:/Windows/Fonts/msyh.ttc")?)` 选 index，插到 `Proportional`/`Monospace` 族的 fallback 列表；0.34 字体后端换成 skrifa + vello_cpu，带 hinting，小字号 CJK 显著更清晰。注意字体数据全量驻内存（msyh.ttc 约 19MB，建议运行时读而非 include_bytes）。

### 文件树：推荐 egui_ltreeview

- 内置 `CollapsingHeader`/`ui.collapsing` 只有展开折叠，无选中态/键盘导航/统一缩进线，只够原型。
- **egui_ltreeview 0.7.0**（crates.io API 确认：2026-03-29 发布、依赖 `egui = "0.34"`、MIT、累计 2 万+ 下载、15 个版本随 egui 大版本及时跟进）：键盘导航、单/多选、节点激活（双击/回车）、目录开合、拖放、节点右键菜单，数据结构无关（`builder.dir()/leaf()` 即时模式声明）。质量评估：API 干净、附 playground 全功能示例；风险为单一维护者（bus factor=1）、**无虚拟化**——但 M3 文件树是"只读 + 跟随 cwd"，按当前目录懒加载子层即可规避，推荐采用。

```rust
egui_ltreeview::TreeView::new(ui.make_persistent_id("file_tree")).show(ui, |builder| {
    builder.dir(0, "src");
    builder.leaf(1, "main.rs");
    builder.close_dir();
});
```

---

## 4. 事件循环整合：egui-winit + 手写 ApplicationHandler（无 eframe）

签名均经 docs.rs 0.34.3 确认。最小骨架（与 Lumen 现有 winit 0.30 循环同构，直接并入即可）：

```rust
struct App { /* window, surface, device, queue, ... */
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
}

impl winit::application::ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        // 创建 window/surface/device 后：
        self.egui_ctx = egui::Context::default();
        self.egui_state = egui_winit::State::new(self.egui_ctx.clone(),
            egui::ViewportId::ROOT, &window, /*native_pixels_per_point*/ None,
            /*theme*/ None, /*max_texture_side*/ Some(device.limits().max_texture_dimension_2d as usize));
        self.egui_renderer = egui_wgpu::Renderer::new(&device, surface_format,
            egui_wgpu::RendererOptions { msaa_samples: 1, ..Default::default() });
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let resp = self.egui_state.on_window_event(&self.window, &event); // 先喂 egui
        if resp.repaint { self.window.request_redraw(); }
        if !resp.consumed {
            // 终端直通：KeyboardInput / Ime / MouseWheel(在终端 rect 内) → Lumen 现有输入管线
        }
        if let WindowEvent::RedrawRequested = event {
            let raw_input = self.egui_state.take_egui_input(&self.window);
            let output = self.egui_ctx.run(raw_input, |ctx| { /* 三栏 UI + 终端纹理 */ });
            self.egui_state.handle_platform_output(&self.window, output.platform_output);
            if self.terminal_focused { self.window.set_ime_allowed(true); /* + set_ime_cursor_area */ }

            let clipped = self.egui_ctx.tessellate(output.shapes, output.pixels_per_point);
            let desc = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [config.width, config.height],
                pixels_per_point: output.pixels_per_point,
            };
            let frame = self.surface.get_current_texture().unwrap();
            let view = frame.texture.create_view(&Default::default());
            let mut enc = device.create_command_encoder(&Default::default());
            for (id, delta) in &output.textures_delta.set {
                self.egui_renderer.update_texture(&device, &queue, *id, delta);
            }
            let user_cmds = self.egui_renderer.update_buffers(&device, &queue, &mut enc, &clipped, &desc);
            {
                let pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view, depth_slice: None, resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(bg), store: wgpu::StoreOp::Store },
                    })], ..Default::default()
                });
                let mut pass = pass.forget_lifetime();          // render() 要求 'static
                self.egui_renderer.render(&mut pass, &clipped, &desc);
            }
            for id in &output.textures_delta.free { self.egui_renderer.free_texture(id); }
            queue.submit(user_cmds.into_iter().chain([enc.finish()]));
            frame.present();
            // 重绘调度：PTY 输出到达 → request_redraw；动画 → 看 output.viewport_output[&ROOT].repaint_delay
            // 与 Lumen 现有 Mailbox/限频机制在此汇合
        }
    }
}
```

要点：`update_buffers` 必须先于 `render`（否则 panic）；`forget_lifetime` 后不得再操作父 encoder（运行期才报错）；`textures_delta.free` 放 render 之后；egui 重绘是整窗的——终端高频输出走方案 b 的离屏纹理可让"终端纹理更新"与"egui 帧"解耦，沿用现有限频。

---

## 来源

- [docs.rs egui-wgpu 0.34.3 CallbackTrait](https://docs.rs/egui-wgpu/latest/egui_wgpu/trait.CallbackTrait.html) / [Renderer](https://docs.rs/egui-wgpu/latest/egui_wgpu/struct.Renderer.html) / [RendererOptions](https://docs.rs/egui-wgpu/latest/egui_wgpu/struct.RendererOptions.html)
- [docs.rs egui-winit 0.34.3 State](https://docs.rs/egui-winit/latest/egui_winit/struct.State.html)
- 源码（tag 0.34.3）：[egui-winit/src/lib.rs](https://github.com/emilk/egui/blob/0.34.3/crates/egui-winit/src/lib.rs)（IME 处理/consumed 逻辑/set_ime_allowed）、[egui-wgpu/src/renderer.rs](https://github.com/emilk/egui/blob/0.34.3/crates/egui-wgpu/src/renderer.rs)（Callback/register_native_texture）、[egui/src/style.rs](https://github.com/emilk/egui/blob/0.34.3/crates/egui/src/style.rs)、[epaint/src/text/fonts.rs](https://github.com/emilk/egui/blob/0.34.3/crates/epaint/src/text/fonts.rs)（FontData.index）
- [egui Releases / 0.34 发布说明](https://github.com/emilk/egui/releases)、[CHANGELOG](https://github.com/emilk/egui/blob/main/CHANGELOG.md)（skrifa 字体后端）
- IME issues：[egui #248](https://github.com/emilk/egui/issues/248)、[egui PR #5198](https://github.com/emilk/egui/pull/5198)、[winit #2498](https://github.com/rust-windowing/winit/issues/2498)、[winit #2888](https://github.com/rust-windowing/winit/issues/2888)、[winit #3092](https://github.com/rust-windowing/winit/issues/3092)
- 文件树：[egui_ltreeview GitHub](https://github.com/LennysLounge/egui_ltreeview)、[crates.io](https://crates.io/crates/egui_ltreeview)、[内置方案讨论 egui #2999](https://github.com/emilk/egui/discussions/2999)
- 集成样板参考：[hasenbanck/egui_example](https://github.com/hasenbanck/egui_example)、[egui Discussion #3067](https://github.com/emilk/egui/discussions/3067)、[官方 custom3d_wgpu demo](https://github.com/emilk/egui/blob/main/crates/egui_demo_app/src/apps/custom3d_wgpu.rs)
