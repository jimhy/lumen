//! Win11 Snap Layouts 支持（M3.8 批2）。
//!
//! 机制：鼠标悬停最大化按钮时 `WM_NCHITTEST` 返回 `HTMAXBUTTON`，
//! Windows Shell 随即弹出 Snap Layouts 浮动菜单（Win11 22H2+）。
//!
//! 由于 winit 0.30 无原生 Snap Layouts 支持（Issue #3884 open），
//! 本模块通过 `SetWindowSubclass`（comctl32.dll）子类化窗口过程，
//! 在 `WM_NCHITTEST` 时将命中最大化按钮区域的鼠标坐标改报为
//! `HTMAXBUTTON`，其余消息透传 `DefSubclassProc`。
//!
//! # NC 点击处理
//!
//! 返回 `HTMAXBUTTON` 后系统把鼠标点击发为 `WM_NCLBUTTONDOWN` /
//! `WM_NCLBUTTONUP`（非客户区消息），egui 收不到这类事件。
//! 本模块在子类过程中捕获这两条消息：
//! - `WM_NCLBUTTONDOWN`（wParam == HTMAXBUTTON）：吞掉，返回 0，
//!   阻止系统默认行为（防止系统自行切换最大化）。
//! - `WM_NCLBUTTONUP`  （wParam == HTMAXBUTTON）：执行最大化切换
//!   （`IsZoomed` 判当前态，分别发 `SW_MAXIMIZE` / `SW_RESTORE`），
//!   返回 0。
//!
//! 依据：Microsoft Learn「Support Snap layouts」示例的 C++ 子类过程即此模式
//! （吞 DOWN、在 UP 执行动作）。
//!
//! # 已知限制
//!
//! 最大化按钮处于非客户区（NC 区）时，egui 收不到 `WM_MOUSEMOVE`，
//! 因此 egui 侧的悬停高亮在 Snap 弹出期间不工作——Snap 弹出与点击
//! 功能优先，视觉高亮限制接受，真机验证项详见模块末尾注释。

#[cfg(target_os = "windows")]
pub use windows_impl::*;

#[cfg(target_os = "windows")]
mod windows_impl {
    use std::sync::atomic::{AtomicI32, Ordering};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsZoomed, ShowWindow, HTMAXBUTTON, SW_MAXIMIZE, SW_RESTORE, WM_NCDESTROY, WM_NCHITTEST,
        WM_NCLBUTTONDOWN, WM_NCLBUTTONUP,
    };

    /// 子类 ID（任意非零常量，与本模块唯一绑定）。
    const SUBCLASS_ID: usize = 0x4C534E41; // "LSNA"（Lumen Snap）

    // ── 最大化按钮**屏幕物理像素**矩形（四个原子 i32）─────────────────
    // WM_NCHITTEST 的 lParam 是屏幕坐标，必须用屏幕物理像素进行命中判定。
    // 初始值 0 / 0 / 0 / 0 → right <= left、bottom <= top → 矩形无效，
    // 任意坐标都不会命中，等价于「本帧尚未更新/禁用」。
    static BTN_LEFT: AtomicI32 = AtomicI32::new(0);
    static BTN_TOP: AtomicI32 = AtomicI32::new(0);
    static BTN_RIGHT: AtomicI32 = AtomicI32::new(0);
    static BTN_BOTTOM: AtomicI32 = AtomicI32::new(0);

    /// 原子更新最大化按钮的**屏幕物理像素**矩形。
    ///
    /// 由 main.rs 在每帧 egui 绘制完成后调用：
    /// 逻辑矩形 × pixels_per_point + window.inner_position() 换算。
    ///
    /// # 参数
    /// - `l` / `t` / `r` / `b`：屏幕坐标（物理像素），left ≤ right、
    ///   top ≤ bottom。传入退化矩形（l ≥ r 或 t ≥ b）等价于「禁用」。
    pub fn update_button_rect(l: i32, t: i32, r: i32, b: i32) {
        BTN_LEFT.store(l, Ordering::Relaxed);
        BTN_TOP.store(t, Ordering::Relaxed);
        BTN_RIGHT.store(r, Ordering::Relaxed);
        BTN_BOTTOM.store(b, Ordering::Relaxed);
    }

    /// 安装 Snap Layouts 子类过程。
    ///
    /// 窗口创建后调用一次。失败时由调用方记 warn 日志并继续（Snap 是
    /// 增强功能，不影响应用主体逻辑）。
    ///
    /// # Safety
    ///
    /// `hwnd` 必须是由本进程创建、仍然有效的窗口句柄，且在调用时窗口
    /// 尚未销毁。`SetWindowSubclass` 是线程安全的 Win32 API，但必须从
    /// 创建该窗口的同一线程调用（winit init 即主线程，时序成立）。
    #[allow(clippy::undocumented_unsafe_blocks)] // 注释在 Safety 节
    pub unsafe fn install(hwnd: isize) -> bool {
        // SAFETY: hwnd 来自 winit 刚创建的窗口（init 函数内），调用方
        // 已保证其有效性；SUBCLASS_ID 固定常量，dwRefData 为 0（不传指针）。
        let ok = unsafe {
            SetWindowSubclass(
                hwnd as HWND,
                Some(subclass_proc),
                SUBCLASS_ID,
                0, // dwRefData：不传指针，避免生命周期问题
            )
        };
        ok != 0
    }

    /// 将 WM_NCHITTEST lParam 屏幕坐标解包为 (x, y)。
    ///
    /// lParam 低 16 位为 x、高 16 位为 y；必须经 `as i16` 截断再 `as i32`
    /// 符号扩展，才能在多显示器负坐标场景下正确工作。
    ///
    /// # 参数
    /// - `lparam`：`WM_NCHITTEST` 的 `lParam` 原始值。
    ///
    /// # 返回
    /// `(screen_x, screen_y)` 屏幕物理像素坐标，已做符号扩展。
    pub fn unpack_lparam(lparam: LPARAM) -> (i32, i32) {
        // 低 16 位 → x（as i16 截断符号扩展）
        let x = (lparam & 0xFFFF) as i16 as i32;
        // 高 16 位 → y（as i16 截断符号扩展）
        let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
        (x, y)
    }

    /// 纯函数：判断点 `(px, py)` 是否落在矩形 `[l, r) × [t, b)` 内。
    ///
    /// 矩形退化（`r ≤ l` 或 `b ≤ t`）时始终返回 `false`。
    /// 此函数不依赖任何全局状态，可在测试中直接调用而不影响其他测试。
    ///
    /// # 参数
    /// - `px` / `py`：待判断的点坐标。
    /// - `l` / `t` / `r` / `b`：矩形的左、上、右、下边界（物理像素，屏幕坐标）。
    pub fn hit_rect(px: i32, py: i32, l: i32, t: i32, r: i32, b: i32) -> bool {
        // 退化矩形不命中
        if r <= l || b <= t {
            return false;
        }
        px >= l && px < r && py >= t && py < b
    }

    /// 判断屏幕点 `(px, py)` 是否落在当前最大化按钮矩形内。
    ///
    /// 从全局原子读取矩形后委托 [`hit_rect`] 执行命中判断；
    /// 矩形退化时始终返回 `false`（初始值全零即退化，等价于「禁用」）。
    pub fn hit_maximize_button(px: i32, py: i32) -> bool {
        let l = BTN_LEFT.load(Ordering::Relaxed);
        let t = BTN_TOP.load(Ordering::Relaxed);
        let r = BTN_RIGHT.load(Ordering::Relaxed);
        let b = BTN_BOTTOM.load(Ordering::Relaxed);
        hit_rect(px, py, l, t, r, b)
    }

    /// Snap Layouts 子类窗口过程（comctl32 SetWindowSubclass 回调）。
    ///
    /// # Safety
    ///
    /// 由 Windows 消息循环调用，签名由系统 ABI 保证；hwnd / wparam /
    /// lparam 的有效性由 OS 保证（系统传入的有效消息参数）。
    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        umsg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _uid_subclass: usize,
        _ref_data: usize,
    ) -> LRESULT {
        match umsg {
            WM_NCHITTEST => {
                // 先让系统做标准命中测试。
                // SAFETY: hwnd / wparam / lparam 由系统保证有效。
                let base = unsafe { DefSubclassProc(hwnd, umsg, wparam, lparam) };
                // 解包屏幕坐标（i16 截断符号扩展，多显示器负坐标正确）。
                let (sx, sy) = unpack_lparam(lparam);
                if hit_maximize_button(sx, sy) {
                    // 命中最大化按钮热区 → 返回 HTMAXBUTTON，
                    // 告知系统此处是最大化按钮，Win11 Shell 弹 Snap Layouts 菜单。
                    return HTMAXBUTTON as LRESULT;
                }
                base
            }
            WM_NCLBUTTONDOWN if wparam == HTMAXBUTTON as WPARAM => {
                // 吞掉 DOWN 事件，阻止系统默认的最大化切换行为。
                // 真正的切换在 UP 时执行，与 Microsoft Learn 示例一致。
                0
            }
            WM_NCLBUTTONUP if wparam == HTMAXBUTTON as WPARAM => {
                // UP 时执行最大化切换（IsZoomed 判当前态）。
                // SAFETY: hwnd 由系统保证在消息回调期间有效。
                let maximized = unsafe { IsZoomed(hwnd) } != 0;
                let cmd = if maximized { SW_RESTORE } else { SW_MAXIMIZE };
                // SAFETY: hwnd 有效，cmd 是合法的 SHOW_WINDOW_CMD。
                unsafe { ShowWindow(hwnd, cmd) };
                0
            }
            WM_NCDESTROY => {
                // MSDN「Subclassing Controls」规范：在 WM_NCDESTROY 中移除子类，
                // 防止窗口已销毁后子类过程仍被调用（use-after-free）。
                // 先 RemoveWindowSubclass 注销，再 DefSubclassProc 完成默认销毁。
                //
                // SAFETY: hwnd 在 WM_NCDESTROY 回调时仍处于销毁流程中、尚未
                // 最终无效；SUBCLASS_ID 与 install() 一致，保证移除的是本模块
                // 注册的子类而非其他子类。
                unsafe { RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID) };
                // SAFETY: hwnd / wparam / lparam 由系统保证在此回调期间有效。
                unsafe { DefSubclassProc(hwnd, umsg, wparam, lparam) }
            }
            _ => {
                // 其余消息透传给系统默认子类处理。
                // SAFETY: hwnd / wparam / lparam 由系统保证有效。
                unsafe { DefSubclassProc(hwnd, umsg, wparam, lparam) }
            }
        }
    }

    // ── 单元测试 ─────────────────────────────────────────────────────────
    #[cfg(test)]
    mod tests {
        use super::*;

        // lParam 解包测试

        #[test]
        fn unpack_positive_coords() {
            // x=100 (0x0064), y=200 (0x00C8) → lParam = 0x00C8_0064
            let lp: LPARAM = 0x00C8_0064;
            assert_eq!(unpack_lparam(lp), (100, 200));
        }

        #[test]
        fn unpack_negative_x() {
            // x=-10 → 低16位 = 0xFFF6（i16 = -10），y=50 (0x0032)
            // lParam = 0x0032_FFF6
            let lp: LPARAM = 0x0032_FFF6;
            assert_eq!(unpack_lparam(lp), (-10, 50));
        }

        #[test]
        fn unpack_negative_y() {
            // x=80 (0x0050), y=-5 → 高16位 = 0xFFFB（i16 = -5）
            // lParam = 0xFFFB_0050
            let lp: LPARAM = 0xFFFB_0050_u32 as LPARAM;
            assert_eq!(unpack_lparam(lp), (80, -5));
        }

        #[test]
        fn unpack_both_negative() {
            // x=-1 (0xFFFF), y=-1 (0xFFFF) → lParam = 0xFFFF_FFFF
            let lp: LPARAM = -1_i32 as LPARAM;
            assert_eq!(unpack_lparam(lp), (-1, -1));
        }

        #[test]
        fn unpack_max_positive() {
            // i16 最大正值 32767 (0x7FFF)
            // lParam = 0x7FFF_7FFF
            let lp: LPARAM = 0x7FFF_7FFF;
            assert_eq!(unpack_lparam(lp), (32767, 32767));
        }

        // ── 矩形命中判定测试（测纯函数 hit_rect，不写全局原子）────────────
        // 直接测 hit_rect 纯函数：无全局状态、并行安全、结果确定。

        #[test]
        fn hit_rect_inside() {
            // 左上角、中心、右下角内侧均命中
            assert!(hit_rect(100, 0, 100, 0, 146, 34), "左上角应命中");
            assert!(hit_rect(120, 17, 100, 0, 146, 34), "中心应命中");
            assert!(hit_rect(145, 33, 100, 0, 146, 34), "右下角内侧应命中");
        }

        #[test]
        fn hit_rect_outside() {
            // 左侧、右边界（不含）、下边界（不含）、上方均不命中
            assert!(!hit_rect(99, 17, 100, 0, 146, 34), "左侧不命中");
            assert!(
                !hit_rect(146, 17, 100, 0, 146, 34),
                "right 边界（不含）不命中"
            );
            assert!(
                !hit_rect(120, 34, 100, 0, 146, 34),
                "bottom 边界（不含）不命中"
            );
            assert!(!hit_rect(120, -1, 100, 0, 146, 34), "上方不命中");
        }

        #[test]
        fn hit_rect_zero_rect_never_hits() {
            // all-zero 退化矩形（right == left, bottom == top），任意坐标不命中
            assert!(!hit_rect(0, 0, 0, 0, 0, 0));
            assert!(!hit_rect(50, 50, 0, 0, 0, 0));
        }

        #[test]
        fn hit_rect_degenerate_never_hits() {
            // right <= left
            assert!(!hit_rect(70, 17, 100, 0, 50, 34));
            // bottom <= top
            assert!(!hit_rect(120, 15, 100, 20, 146, 10));
        }

        #[test]
        fn hit_rect_negative_screen_coords() {
            // 多显示器：主屏左侧，屏幕坐标为负
            let (l, r) = (-1024 + 100, -1024 + 146);
            assert!(hit_rect(-924, 17, l, 0, r, 34), "负屏幕坐标应命中");
            assert!(!hit_rect(-1024, 17, l, 0, r, 34), "负屏幕坐标左侧不命中");
        }

        #[test]
        fn hit_rect_boundary_edges() {
            // 左边界：px == l → 命中（>= l && < r）
            assert!(hit_rect(200, 17, 200, 0, 246, 34));
            // 右边界：px == r → 不命中
            assert!(!hit_rect(246, 17, 200, 0, 246, 34));
        }

        // ── 全局原子冒烟测试（串行安全：单个测试写入后立即读取）──────────────
        // 仅验证 update_button_rect + hit_maximize_button 路径可通，
        // 不在此处测矩形逻辑（逻辑已由 hit_rect 测试覆盖）。
        // 注意：本测试写全局原子，需在同一逻辑单元内完成写-读，
        // 不依赖其他测试写入的值，因此与并行测试不互相干扰。
        #[test]
        fn update_and_hit_maximize_button_smoke() {
            // 写一个确定命中的矩形，读回应命中
            update_button_rect(500, 10, 546, 44);
            assert!(
                hit_maximize_button(520, 27),
                "update_button_rect + hit_maximize_button 路径应命中"
            );
        }
    }
}

// ── 真机验证项（屏幕锁定环境无法验证，解锁后人工确认）───────────────────
// 1. 鼠标悬停最大化按钮热区：Win11 22H2+ 弹 Snap Layouts 浮动菜单。
// 2. 点击 Snap Layouts 中的布局方案：窗口按该方案吸附（系统行为，
//    子类过程不干预 WM_NCLBUTTONDOWN/UP 以外的消息）。
// 3. NC 区悬停时 egui 侧按钮高亮不可用——已知限制，Snap 优先，接受。
// 4. 多显示器（主屏右侧）：负屏幕坐标换算正确，Snap 弹出不偏移。
// 5. 窗口最小化再还原后 Snap 仍可用（子类过程随窗口存活，不需重装）。
