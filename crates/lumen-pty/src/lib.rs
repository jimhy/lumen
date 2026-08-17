//! Lumen 的 PTY 抽象层。
//!
//! 基于 portable-pty 封装：Windows 走 ConPTY，unix 走 openpty，
//! 本 crate 自身不含平台分支。输出读取在独立线程进行，
//! 通过 crossbeam channel 把字节流推给上层（主事件循环）。

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

/// PTY 输出事件，由读线程推送。
#[derive(Debug)]
pub enum PtyEvent {
    /// shell 输出的原始字节（含 VT 转义序列）。
    Data(Vec<u8>),
    /// shell 进程已退出。
    Exited,
}

/// 一个运行中的 shell 会话。
///
/// Drop 时杀掉子进程，避免孤儿 shell。
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    /// 子进程杀手（Drop 时杀进程）。child 本体 move 进 wait 线程阻塞等
    /// 退出（见 [`Self::spawn`]），故这里只留可 clone 的 killer。
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// 进程存活标志：wait 线程在子进程退出时置 false（[`Self::is_alive`] 据此）。
    alive: Arc<AtomicBool>,
    /// shell 子进程 PID（spawn 时捕获，恒定）。用于查会话内前台运行的
    /// 程序（侧栏会话图标读取其 exe 图标，F7②）。spawn 后端取不到时 None。
    shell_pid: Option<u32>,
    /// 写入走独立线程：ConPTY 输入管道在 conhost 繁忙时会反压，
    /// 主线程（UI 事件循环）绝不能阻塞在管道写上。
    write_tx: Sender<Vec<u8>>,
}

impl PtySession {
    /// 启动 shell 并返回会话与输出事件接收端。
    ///
    /// `shell` 为 None 时按平台选默认：Windows 优先 `pwsh.exe`，
    /// 找不到则回退 `powershell.exe`；unix 用 `$SHELL` 或 `/bin/bash`。
    /// `args` 为附加启动参数（如 shell integration 注入）。
    /// `cwd` 为 shell 初始工作目录（会话恢复用，F4）；None 沿用
    /// 本进程当前目录。调用方需保证目录存在——不存在时子进程会
    /// 启动失败。
    /// `extra_env` 为追加到子进程环境的键值对（如网络代理变量
    /// HTTP_PROXY/HTTPS_PROXY/ALL_PROXY），在内置 TERM/COLORTERM 之后
    /// 注入；空切片即无追加。
    pub fn spawn(
        shell: Option<&str>,
        args: &[String],
        rows: u16,
        cols: u16,
        cwd: Option<&Path>,
        extra_env: &[(String, String)],
    ) -> Result<(Self, Receiver<PtyEvent>)> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("打开 PTY 失败")?;

        let (cmd, shell) = build_command(shell, args, cwd, extra_env);

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("启动 shell 失败: {shell}"))?;
        // slave 端句柄交给子进程后即可关闭，否则读端永远等不到 EOF。
        drop(pair.slave);
        // killer 留作 Drop 杀进程；child 本体稍后 move 进 wait 线程等退出。
        let killer = child.clone_killer();
        // shell PID 在 child move 走前捕获（侧栏会话图标用，F7②）。
        let shell_pid = child.process_id();
        // 进程存活标志：wait 线程在子进程退出时翻转。
        let alive = Arc::new(AtomicBool::new(true));

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("克隆 PTY 读端失败")?;
        let mut writer = pair.master.take_writer().context("获取 PTY 写端失败")?;

        // 写线程：键盘输入量小，无界通道安全；发送端 drop 后线程退出。
        let (write_tx, write_rx) = unbounded::<Vec<u8>>();
        std::thread::Builder::new()
            .name("lumen-pty-writer".into())
            .spawn(move || {
                for data in write_rx {
                    if writer
                        .write_all(&data)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .context("启动 PTY 写线程失败")?;

        // 有界通道形成背压：渲染端消费不过来时读线程会阻塞，
        // 避免 `yes` 这类高速输出把内存撑爆。
        let (tx, rx) = bounded::<PtyEvent>(128);
        // wait 线程先取一份发送端与存活标志（读线程随后 move 走 tx）。
        let wait_tx = tx.clone();
        let wait_alive = alive.clone();
        let mut wait_child = child;
        std::thread::Builder::new()
            .name("lumen-pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(PtyEvent::Exited);
                            break;
                        }
                        Ok(n) => {
                            if tx.send(PtyEvent::Data(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
            .context("启动 PTY 读线程失败")?;

        // wait 线程：直接阻塞等子进程退出再发 Exited，不依赖 ConPTY 的
        // read EOF——Windows ConPTY 下 shell 退出后 master read 常不返回
        // EOF（conhost 保持 pipe 打开），只靠读线程会漏报退出、窗格卡死
        // 无响应（海风哥 2026-06-13 实测 `exit` 无反应的真因）。读线程的
        // EOF 分支仍保留作兜底；两路都可能发 Exited，上层按窗格 id 去重。
        std::thread::Builder::new()
            .name("lumen-pty-wait".into())
            .spawn(move || {
                let _ = wait_child.wait();
                wait_alive.store(false, Ordering::Release);
                let _ = wait_tx.send(PtyEvent::Exited);
            })
            .context("启动 PTY 等待线程失败")?;

        Ok((
            Self {
                master: pair.master,
                killer,
                alive,
                shell_pid,
                write_tx,
            },
            rx,
        ))
    }

    /// 向 shell 写入用户输入（已编码为 VT 序列的字节）。
    /// 实际写入由独立线程完成，本方法不阻塞。
    pub fn write(&self, data: &[u8]) -> Result<()> {
        self.write_tx
            .send(data.to_vec())
            .map_err(|_| anyhow::anyhow!("PTY 写线程已退出"))
    }

    /// 通知 PTY 窗口尺寸变化（行/列）。
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("调整 PTY 尺寸失败")
    }

    /// shell 进程是否仍在运行（wait 线程在子进程退出时翻转此标志）。
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// shell 子进程 PID（spawn 时捕获，恒定）；后端取不到时 None。
    /// 侧栏会话图标据此查前台运行程序的 exe 图标（F7②）。
    pub fn shell_pid(&self) -> Option<u32> {
        self.shell_pid
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

/// 组装子进程的启动命令：解析 shell、声明终端能力、unix locale 兜底、
/// 追加调用方环境。返回 `(命令, 解析后的 shell 路径)`，后者供调用方拼错误信息。
///
/// # 为什么从 [`PtySession::spawn`] 里抽出来
///
/// 「Lumen 到底往子进程环境里塞了什么」是条实打实的产品契约——`ConEmuANSI=ON`
/// 是 TUI 能按协议上报 OSC 9;4（任务进度）/ 9;9（当前目录）的前提。它原本**只**
/// 由一个真机 e2e 测试守着（起 `powershell.exe`、等它把 `$env:ConEmuANSI` 打回来），
/// 而那个测试在 CI 上有约 11% 的偶发失败率：5 秒预算盖不住 `powershell.exe` 在
/// ConPTY 下的冷启动尾部，机器一忙就撞线（本地实测：空闲 0%、CPU 打满 16.7%、
/// 2 倍超订 100%）。
///
/// 抽成纯函数后，这条契约由下面的确定性单测直接断言 `CommandBuilder` 上的环境
/// 变量，微秒级、跨平台、不起进程；那个 e2e 测试随之降级为 `#[ignore]` 的真机
/// 冒烟，与同仓其它 ConPTY 测试（`lumen-term/tests/*`）的待遇一致。
fn build_command(
    shell: Option<&str>,
    args: &[String],
    cwd: Option<&Path>,
    extra_env: &[(String, String)],
) -> (CommandBuilder, String) {
    let shell = shell.map(str::to_owned).unwrap_or_else(default_shell);
    let mut cmd = CommandBuilder::new(&shell);
    cmd.args(args);
    if let Some(dir) = cwd {
        // 会话恢复：shell 在保存的工作目录中启动。
        cmd.cwd(dir);
    }
    // 终端能力声明：上层实现了 256 色与真彩 SGR。
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    // Windows 下声明支持 ConEmu/Windows Terminal 系 OSC 9 扩展。
    // Lumen 已解析 OSC 9;4（任务进度）与 9;9（当前目录）；采用能力
    // 变量而非伪造 WT_SESSION，让 TUI 可按协议启用通用进度上报。
    #[cfg(windows)]
    cmd.env("ConEmuANSI", "ON");
    // unix locale 兜底：macOS GUI app（open 经 launchd 启动）不继承终端 LANG，
    // 子 shell 无 locale → ls 等把非 ASCII 文件名输出成 '?'（中文乱码）。若继承
    // 环境里无任何 locale 变量，补一个 UTF-8 locale（有则尊重用户设置、不覆盖）。
    #[cfg(unix)]
    {
        let has_locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
        if !has_locale {
            // macOS 用 en_US.UTF-8（必装）；其它 unix 用 C.UTF-8（现代 glibc 具备）。
            #[cfg(target_os = "macos")]
            cmd.env("LANG", "en_US.UTF-8");
            #[cfg(not(target_os = "macos"))]
            cmd.env("LANG", "C.UTF-8");
        }
    }
    // 调用方追加的环境（网络代理等）：覆盖同名内置/继承值。
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    (cmd, shell)
}

/// 按平台返回默认 shell。
fn default_shell() -> String {
    #[cfg(windows)]
    {
        // pwsh（PowerShell 7+）体验更好，装了就优先用。
        if which_in_path("pwsh.exe") {
            "pwsh.exe".into()
        } else {
            "powershell.exe".into()
        }
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
    }
}

/// 在 PATH 中查找可执行文件是否存在。
#[cfg(windows)]
fn which_in_path(exe: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(exe).is_file()))
        .unwrap_or(false)
}

// 整模块原本挂 `cfg(all(test, windows))`——因为当时唯一的测试是 Windows ConPTY
// 真机 e2e。现在 `build_command` 的确定性单测在三个平台都有意义（unix 那段 locale
// 兜底此前零覆盖），故放开到 `cfg(test)`，改由**单条**测试自己挂平台 cfg。
#[cfg(test)]
mod tests {
    use super::*;

    /// 终端能力声明必须真的进到启动命令里——这是 OSC 9;4 / 9;9 能被 TUI 主动
    /// 触发的前提。断言 `CommandBuilder` 上的环境变量，不起进程、不看时序，
    /// 所以是确定性的：这几行被误删或被后面的 `extra_env` 循环意外覆盖时必红。
    #[test]
    fn 启动命令带终端能力声明() {
        // 不起进程，shell 名只验透传，故取一个平台无关的占位名。
        let (cmd, shell) = build_command(Some("some-shell"), &["-NoLogo".to_owned()], None, &[]);
        assert_eq!(shell, "some-shell", "显式指定的 shell 应原样透传");
        assert_eq!(
            cmd.get_env("TERM"),
            Some(std::ffi::OsStr::new("xterm-256color"))
        );
        assert_eq!(cmd.get_env("COLORTERM"), Some(std::ffi::OsStr::new("truecolor")));
        // ★ 这条替代了下面那个真机 e2e 测试的实际被测意图。
        #[cfg(windows)]
        assert_eq!(
            cmd.get_env("ConEmuANSI"),
            Some(std::ffi::OsStr::new("ON")),
            "ConEmuANSI=ON 是 TUI 上报 OSC 9;4 任务进度 / 9;9 当前目录的前提"
        );
    }

    /// 调用方追加的环境（网络代理等）必须覆盖同名内置值——顺序写反了就会被
    /// 内置值反向覆盖，而那种错在真机测试里表现为「代理莫名其妙不生效」。
    #[test]
    fn 追加环境覆盖同名内置值() {
        let (cmd, _) = build_command(
            None,
            &[],
            None,
            &[
                ("TERM".to_owned(), "dumb".to_owned()),
                ("HTTPS_PROXY".to_owned(), "http://127.0.0.1:7890".to_owned()),
            ],
        );
        assert_eq!(
            cmd.get_env("TERM"),
            Some(std::ffi::OsStr::new("dumb")),
            "extra_env 必须压过内置 TERM"
        );
        assert_eq!(
            cmd.get_env("HTTPS_PROXY"),
            Some(std::ffi::OsStr::new("http://127.0.0.1:7890"))
        );
    }

    /// 真机冒烟：起一个 shell，确认能力声明**确实**穿过 portable-pty 落到子进程
    /// 环境块里（上面那两条只覆盖到 `CommandBuilder`，管不到这一段第三方投递）。
    ///
    /// `#[ignore]` 的理由：这条测试实际上是三件事的合取——(a) 我们的 env 注入
    /// 生效【唯一的被测意图】、(b) conhost + `powershell.exe` 能在 5 秒内冷启动
    /// 并走完 ConPTY 握手、(c) ConPTY 把 `O`/`N` 相邻送到读端。(b)(c) 是搭便车
    /// 混进来的环境断言，对产品契约零价值，却贡献了全部的偶发失败：CI 上 18 次
    /// 实跑挂了 2 次（≈11%，两次都恰好烧满 5 秒），本地实测空闲 0%、CPU 打满
    /// 16.7%、2 倍超订 100%——失败时子进程一个字节都没吐出来，纯粹是没启动完。
    /// (a) 现由上面的 `启动命令带终端能力声明` 确定性覆盖，故这里跳过 CI、
    /// 需要时真机手跑（`cargo test -p lumen-pty -- --ignored`）。
    ///
    /// 与同仓其它真机 ConPTY 测试（`lumen-term/tests/req6_reflow_conpty.rs` 等）
    /// 的处置一致——本测试此前是唯一在 CI 里真跑 ConPTY 的漏网之鱼。
    ///
    /// 手跑时注意：`--exact` 要用全限定名 `tests::子进程收到终端进度能力声明`，
    /// 只写函数名会匹配不到、静默变成「0 tests run」的假绿。
    #[cfg(windows)]
    #[ignore = "真机 ConPTY + powershell.exe 冷启动，时序不确定；契约已由 启动命令带终端能力声明 覆盖"]
    #[test]
    fn 子进程收到终端进度能力声明() {
        use std::time::{Duration, Instant};

        let args = vec![
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            "$env:ConEmuANSI; Start-Sleep -Seconds 1".to_owned(),
        ];
        let (pty, rx) =
            PtySession::spawn(Some("powershell.exe"), &args, 24, 80, None, &[])
                .expect("启动 PowerShell PTY");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        let mut replied = false;

        while Instant::now() < deadline {
            let timeout = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(timeout) {
                Ok(PtyEvent::Data(bytes)) => {
                    output.extend_from_slice(&bytes);
                    if !replied
                        && (output.windows(4).any(|w| w == b"\x1b[6n")
                            || output.windows(3).any(|w| w == b"\x1b[c"))
                    {
                        // ConPTY 启动会先查询光标位置与主设备属性；应用正常
                        // 运行时由 lumen-term 应答。收到查询后回写等价响应。
                        pty.write(b"\x1b[1;1R\x1b[?62;22c")
                            .expect("应答 ConPTY 初始化查询");
                        replied = true;
                    }
                    if String::from_utf8_lossy(&output).contains("ON") {
                        break;
                    }
                }
                Ok(PtyEvent::Exited) => {}
                Err(_) => break,
            }
        }

        assert!(
            String::from_utf8_lossy(&output).contains("ON"),
            "子进程未收到 ConEmuANSI=ON，实际输出: {:?}",
            String::from_utf8_lossy(&output)
        );
    }
}
