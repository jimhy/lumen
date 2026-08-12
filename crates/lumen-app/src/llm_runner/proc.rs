//! headless CLI **子进程**：无 PTY、三条管道、进程树清理。
//!
//! # 与 `lumen-pty` / `session.rs` 的关系：刻意不复用
//!
//! 仓库里已有一套成熟的子进程封装（`lumen-pty`），但它是 **ConPTY / pty** 路径：分配伪终端、
//! 有行列尺寸、输出是 VT 字节流。headless CLI 恰恰**不需要**这些——它读 stdin 的 JSON 行、
//! 写 stdout 的 JSON 行，一旦挂上 PTY，CLI 会以为自己在交互终端里，转而输出 ANSI 控制序列与
//! 进度动画，我们要的结构化事件流就没了。
//!
//! 照抄的是另一套范式：`completion_sidecar.rs:220-303` 的
//! 「`std::process::Command` + 三管道 + 读线程 + `EventLoopProxy` 唤醒」。本模块只负责**进程**，
//! 线程与通道拓扑在 [`super`]。
//!
//! # 进程树清理（设计蓝图 §6.5）
//!
//! | 平台 | 手段 | 说明 |
//! |---|---|---|
//! | Windows | **Job Object** + `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` | claude 会 spawn Bash/PowerShell/node，只 kill 父进程会留孤儿**继续改用户文件** |
//! | unix | `CommandExt::process_group(0)` + `child.kill()` | 只把子进程放进独立进程组；**孙进程会残留**，见下方「已知限制」 |
//!
//! ## `KILL_ON_JOB_CLOSE` 的关键价值不是"我们记得杀"
//!
//! 而是**连 Lumen 崩溃 / 被任务管理器强杀都能兜住**：job 句柄随进程终止被 OS 关闭，OS 随即
//! 杀掉 job 里的一切。这是唯一不依赖我们自己的代码跑到的一层保险。
//!
//! ## 两处诚实澄清（不掩饰）
//!
//! 1. **竞态窗口真实存在**：`spawn()` 与 `AssignProcessToJobObject` 之间有一个窗口，此间子进程
//!    spawn 出的孙进程不受 job 管辖。claude 冷启动实测 > 1 s，窗口远小于此，**P0 接受**。
//!    严格无竞态需要 `CREATE_SUSPENDED` + `ResumeThread`，而 `std::process` **不暴露主线程句柄**
//!    （只有 `Child::as_raw_handle()` 给进程句柄），要做就得整套改用 `CreateProcessW` 自己拼
//!    `STARTUPINFO` 与三个继承句柄——那是几百行 unsafe，换一个我们已经能接受的窗口，不划算。
//! 2. **unix 侧的已知限制**：`process_group(0)` 只是把子进程放进独立进程组，真要 `killpg`
//!    需要 `libc`，而 lumen-app 无 `libc` / `nix` 依赖。**P0 在 unix 上只 `child.kill()`，
//!    会残留孙进程。**
//!    - **被否决的替代方案**：`Command::new("kill").arg("-KILL").arg(format!("-{pgid}"))`
//!      ——不引依赖也能 `killpg`。否决理由三条：① 要在 `Drop` 里 fork+exec 一个进程，
//!      不 `wait` 就留僵尸、`wait` 又在析构路径上阻塞；② `kill(1)` 的路径与参数在
//!      macOS / 各 Linux 发行版上不完全一致；③ **本机无 unix 环境可实测**，而未经实测的
//!      清理代码比"已知会漏"更危险（它会让人以为已经清干净了）。要做请连同实测一起做。
//!
//! # Windows：`Command::new("claude")` **找不到 npm 的 `.cmd` shim**（实测，血的教训）
//!
//! 这是本模块被对抗验证抓到的**唯一 blocker**，值得完整记下来：
//!
//! - Windows 上 `std::process::Command` 对无扩展名的 program **只会追加 `.exe`**，
//!   **不查 `PATHEXT`**。而 `claude` 是 npm 装的，`%APPDATA%\npm` 下只有
//!   `claude` / `claude.cmd` / `claude.ps1` 三个 shim，**没有 `claude.exe`**。
//! - 实测（rustc 单文件探针）：`Command::new("claude")` → `NotFound`；
//!   `Command::new("claude.cmd")` → OK，stdout `2.1.226 (Claude Code)`。
//! - 后果：[`super::LlmRunnerManager::start`] 在这台机器上 100% 返回
//!   `RunnerError::CliNotFound`，整条 LLM 链路一行都跑不到。
//! - 仓库里**早有同一个坑的判例**：`links.rs:317-330`「Windows：`code` 是 .cmd 批处理 shim，
//!   须经 cmd 调起」。本模块初稿没复用那份认知。
//!
//! 修法是 [`resolve_program`]：Windows 上自己按 `PATH` × `PATHEXT` 解析出**带扩展名的实体**，
//! 再交给 `Command`。命中 `.cmd` / `.bat` 时 Rust 自 1.77.2（CVE-2024-24576）起会自动经
//! `cmd.exe` 调起**并做参数转义**，我们不必自己拼引号。
//!
//! ## 被否决的替代方案：`Command::new("cmd").args(["/c", program, …])`
//!
//! 就是 `links.rs` 那一套。否决理由两条：① 参数里有 `--add-dir <工作目录>` 与
//! `--append-system-prompt <任意用户文本>`，经 `cmd /c` 就得**我们自己**处理 `^ & | < > %`
//! 这些元字符，而 `links.rs` 那处能直接用是因为「源码路径几乎不含 cmd 元字符」——这里不成立；
//! ② 走 `cmd /c` 后 `RunnerError::CliNotFound` 里的 program 串会变成 `"cmd"`，
//! 报错从「claude 没装」退化成「cmd 没装」。解析实体则两者都不用管：`adapter.program()`
//! 仍是那个逻辑名，错误消息照旧。
//!
//! ## 只认四种可启动的扩展名
//!
//! `PATHEXT` 默认还含 `.VBS/.JS/.WSF/.MSC`，那些**不是** `CreateProcessW` 能直接起的东西
//! （会得到 `ERROR_BAD_EXE_FORMAT`）。故 [`LAUNCHABLE_EXTS`] 与 `PATHEXT` 取交集：认了却起不来，
//! 比没认出来更糟——前者会把「没装」误报成「起不来」。
//!
//! # 依赖改动
//!
//! 根 `Cargo.toml` 的 `windows-sys` features 补了 `Win32_System_JobObjects`。全仓 grep
//! `JobObjects|CreateJobObject|AssignProcessToJobObject` 此前**零命中**，这是全新领域。
//! `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` 在 windows-sys 0.61 里 gated 于
//! `Win32_System_Threading`（其内嵌 `IO_COUNTERS`），该 feature 早已开启，无需再补。
//! 因为 `windows-sys` 在 lumen-app 是 `[target.'cfg(windows)'.dependencies]`，
//! **加 feature 只影响 Windows 目标**。

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};

/// 子进程的三条管道。`spawn` 成功后立刻交给读 / 写线程，[`ChildProc`] 自己不持有它们。
///
/// **不持有是刻意的**：`ChildStdin` 必须由**单个写线程独占**（§6.4 铁律一），
/// 若管理器也留一份句柄，"多处直写导致半行"这个致命 bug 就在类型上重新变得可能。
pub struct ChildPipes {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// [`ChildProc::try_wait`] 的三态结果。
///
/// # 为什么必须是三态而不是 `Option<ExitStatus>`
///
/// 初稿用 `Option<ExitStatus>`，`try_wait` 的 `Err` 臂注释写「当作已退出，否则这个 runner 会
/// 永远卡在 Busy」，代码却 `self.child = None; None` —— `None` 的含义恰恰是「**还在跑**」。
/// 于是走过一次 `Err` 之后：`child` 已空 ⇒ 之后每次 `try_wait` 都在第一行短路返回 `None` ⇒
/// `exit_seen_at` 永远是 `None` ⇒ 永不报 [`super::event::RunnerEvent::Exited`] ⇒
/// `reapable()` 恒 `false` ⇒ 这个 runner **永久占着 [`super::MAX_HEADLESS_RUNNERS`] 4 个名额之一**，
/// 手机端也永远等不到退出事件。**正是那条注释声称要避免的失败模式**。
///
/// 三态把「还在跑」与「取不到状态」在**类型上**分开，调用方不可能再把它们混为一谈。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryWaitOutcome {
    /// 仍在运行。
    Running,
    /// 已退出并已收尸（unix 上不留僵尸、Windows 上不漏句柄）。
    /// `None` = 被信号杀死等取不到退出码的情形。
    Exited(Option<ExitStatus>),
    /// **句柄异常，状态永远取不到了**。按「已退出、退出码未知」处理。
    ///
    /// 归因会落 [`super::ExitAttribution::Normal`] / `Unattributable`（因为 `code` 是 `None`），
    /// 那**远好过**让 runner 永久挂起。
    Lost,
}

/// 一个 headless CLI 子进程。**主线程独占**（不跨线程移动，故不需要 `Send`）。
pub struct ChildProc {
    /// `None` = 已收尸或句柄已丢，结论记在 [`Self::outcome`] 里。
    child: Option<Child>,
    /// 收尸结论的**记忆**。一旦定了（`Exited` / `Lost`）就不再变，
    /// 之后每次 [`Self::try_wait`] 都返回同一个答案——这是「不许再退回 `Running`」的类型落点。
    outcome: Option<TryWaitOutcome>,
    pid: Option<u32>,
    /// 我们是否主动杀过它（退出归因要区分"CLI 自己退了"与"我们杀的"）。
    killed: bool,
    #[cfg(windows)]
    job: Option<win::JobHandle>,
}

impl ChildProc {
    /// 起一个**不带 PTY** 的子进程，三条管道全部接成 pipe。
    ///
    /// `env` 是**追加**的环境变量（继承父进程环境后覆盖同名项），不是替换整个环境——
    /// 清空环境会让 CLI 找不到 `PATH` / `USERPROFILE` / 代理配置，实际等于起不来。
    ///
    /// `program` 是**逻辑名**（`"claude"`）：Windows 上先经 [`resolve_program`] 解析成带扩展名的
    /// 实体（见模块文档那条 blocker），unix 上原样交给 `execvp` 走 PATH 查找。
    ///
    /// # Errors
    /// 解析不到可执行文件时返回 [`io::ErrorKind::NotFound`]——调用方
    /// （[`super::LlmRunnerManager::start`]）据此映射成 `RunnerError::CliNotFound`，
    /// 故这个 kind **不能换**。
    pub fn spawn(
        program: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> io::Result<(Self, ChildPipes)> {
        let exe = resolve_program(program)?;
        let mut cmd = Command::new(&exe);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            cmd.env(key, value);
        }

        // Windows：CREATE_NO_WINDOW (0x0800_0000) 阻止子进程弹出控制台窗口。
        // 与 `completion_sidecar.rs:235` 同款——headless CLI 是 node 程序，
        // 不加这个标志每起一个会话就闪一个黑框。
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000u32);
        }

        // unix：放进独立进程组。**这只是清理的下限**（见模块文档「已知限制」），
        // 但它至少保证 Ctrl-C 之类发给 Lumen 进程组的信号不会顺带打到 CLI 上。
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        // Windows：**先建 job 再 spawn**，把竞态窗口压到"spawn 返回 → assign"这一小段。
        // 反过来（spawn 完再建 job）会把 `CreateJobObjectW` 的耗时也算进窗口里。
        #[cfg(windows)]
        let job = win::JobHandle::create_kill_on_close();

        let mut child = cmd.spawn()?;
        let pid = Some(child.id());

        #[cfg(windows)]
        if let Some(job) = job.as_ref() {
            job.assign(&child);
        }

        // 三条管道在 `Stdio::piped()` 下必然是 `Some`；仍然用 `let-else` 而不是 `expect`，
        // 让"平台行为出乎意料"表现为一个可处理的 IO 错误而不是 panic 掉整个 Lumen。
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            return Err(io::Error::other("子进程管道未按 Stdio::piped() 建立"));
        };
        let pipes = ChildPipes {
            stdin,
            stdout,
            stderr,
        };

        Ok((
            Self {
                child: Some(child),
                outcome: None,
                pid,
                killed: false,
                #[cfg(windows)]
                job,
            },
            pipes,
        ))
    }

    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// 我们是否主动杀过它。
    #[must_use]
    pub fn killed(&self) -> bool {
        self.killed
    }

    /// **非阻塞**收尸。三态语义见 [`TryWaitOutcome`]。
    ///
    /// **幂等**：一旦返回过 `Exited` / `Lost`，之后每次调用都返回同一个值（结论存在
    /// [`Self::outcome`] 里）。这条是硬要求——调用方 `pump_once` 每帧都调它，
    /// 若第二次调用退回 `Running`，`exit_seen_at` 之后的整条退出链路就全乱了。
    ///
    /// # 为什么不设专门的 wait 线程
    /// `Child::wait` 要 `&mut self` 且阻塞，会与 `kill` 争同一个 `Child`。`lumen-pty`
    /// 能用独立 wait 线程（`lib.rs:177-184`）是因为它把 `Child` 整个搬进了线程；
    /// 而这里主线程要读退出码做归因与重启决策，`Child` 必须留在管理器里。
    /// 每帧一次 `try_wait()` 的成本可以忽略（`WaitForSingleObject(0)` / `waitpid(WNOHANG)`）。
    ///
    /// # `Err` 臂为什么不补一次阻塞 `wait()` 去收僵尸
    /// unix 上 `Child::try_wait` 报错只可能是 `waitpid` 返回错误，实际就是 `ECHILD`
    /// （这个子进程已经不存在 / 已被别处收走）——**那种情形下本来就没有僵尸可收**。
    /// 而在主线程上补一次阻塞 `wait()` 的代价是「句柄真出问题时整个 UI 线程挂死」，
    /// 拿一个不存在的僵尸去换一次死锁风险不划算。故这里只丢 `Child` + 记 `Lost`。
    pub fn try_wait(&mut self) -> TryWaitOutcome {
        if let Some(memo) = self.outcome {
            return memo;
        }
        let Some(child) = self.child.as_mut() else {
            // 没有 `Child` 又没有结论：只可能是构造后就没 spawn 成功，按「还在跑」处理不安全，
            // 按「丢了」处理才与上面的不变量一致。
            return self.remember(TryWaitOutcome::Lost);
        };
        let outcome = classify_try_wait(child.try_wait());
        if outcome == TryWaitOutcome::Running {
            return TryWaitOutcome::Running;
        }
        // 收尸成功后立刻丢掉 Child：`try_wait` 对已 reap 的进程行为是"再次返回同一个 status"，
        // 但保留 Child 会让 `kill_tree` 之后去 kill 一个不存在的 pid。
        self.child = None;
        self.remember(outcome)
    }

    /// 记住并返回一个终态结论。
    fn remember(&mut self, outcome: TryWaitOutcome) -> TryWaitOutcome {
        self.outcome = Some(outcome);
        outcome
    }

    /// **测试专用**：模拟「`try_wait` 报错、句柄丢失」这条真实进程里造不出来的分支。
    #[cfg(test)]
    pub(super) fn force_lost_for_test(&mut self) {
        self.child = None;
        self.remember(TryWaitOutcome::Lost);
    }

    /// 杀掉**整棵进程树**（Windows）/ 直接子进程（unix，见模块文档已知限制）。
    ///
    /// 幂等：重复调用安全，进程已退出时是 no-op。
    pub fn kill_tree(&mut self) {
        self.killed = true;

        #[cfg(windows)]
        if let Some(job) = self.job.as_ref() {
            // 先 TerminateJobObject：它一次杀掉 job 里的全部进程（含 claude spawn 的
            // bash / node / pwsh 孙进程）。退出码取 1——0 会让归因逻辑把"被我们杀"
            // 误判成"正常退出"。
            job.terminate(1);
        }

        if let Some(child) = self.child.as_mut() {
            // Windows 上 job 已经把它杀了，这一步是 no-op；unix 上这是唯一手段。
            let _ = child.kill();
            // **不 `wait()`**：阻塞主线程。下一帧 `try_wait()` 会收尸。
        }
    }
}

impl Drop for ChildProc {
    /// 兜底清理。
    ///
    /// [`super::LlmRunnerManager`] 的 `Drop` 已经显式 `kill_tree` 过一遍，这里再来一次是
    /// 为了覆盖"单个 runner 被从 map 里移除"的路径（空闲回收 / 手机 `Close`）——
    /// 那条路径不经过管理器的 `Drop`。
    ///
    /// Windows 上还有最后一层保险：`job` 字段随本结构体一起 drop → `CloseHandle` →
    /// `KILL_ON_JOB_CLOSE` 生效。即使这个 `Drop` 本身没跑到（进程被强杀），OS 也会兜住。
    fn drop(&mut self) {
        self.kill_tree();
    }
}

/// [`Child::try_wait`] 的返回值 → [`TryWaitOutcome`]。
///
/// **抽成纯函数只为可单测**：真进程造不出「句柄异常」这条分支，而它恰恰是初稿写反的那一条。
fn classify_try_wait(res: io::Result<Option<ExitStatus>>) -> TryWaitOutcome {
    match res {
        Ok(Some(status)) => TryWaitOutcome::Exited(Some(status)),
        Ok(None) => TryWaitOutcome::Running,
        Err(e) => {
            log::warn!("LLM runner try_wait 失败（按已退出、退出码未知处理）: {e}");
            TryWaitOutcome::Lost
        }
    }
}

// ── 可执行文件解析 ────────────────────────────────────────────────────────────

/// Windows 上认为「`CreateProcessW` 起得来」的扩展名，**与 `PATHEXT` 取交集**（见模块文档）。
///
/// `.bat` / `.cmd` 由 Rust 标准库自 1.77.2 起转交 `cmd.exe` 并自动转义参数；
/// `.com` / `.exe` 是真正的 PE。`.ps1` **刻意不在表内**——它得经 `powershell -File` 才跑得起来，
/// 认下来只会把「起不来」伪装成「找到了」。
#[cfg(windows)]
pub const LAUNCHABLE_EXTS: &[&str] = &[".exe", ".com", ".bat", ".cmd"];

/// 把逻辑可执行名解析成一个真正能交给 `Command` 的路径。
///
/// unix：**原样返回**，由 `execvp` 自己走 `PATH`（unix 没有 `PATHEXT` 这套东西）。
///
/// # Errors
/// 找不到时返回 [`io::ErrorKind::NotFound`]（调用方据此映射 `RunnerError::CliNotFound`）。
#[cfg(not(windows))]
pub fn resolve_program(program: &str) -> io::Result<PathBuf> {
    Ok(PathBuf::from(program))
}

/// Windows 版：按 `PATH` × (`PATHEXT` ∩ [`LAUNCHABLE_EXTS`]) 找出带扩展名的实体。
///
/// # Errors
/// 找不到时返回 [`io::ErrorKind::NotFound`]。
#[cfg(windows)]
pub fn resolve_program(program: &str) -> io::Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let pathext = std::env::var_os("PATHEXT")
        .unwrap_or_else(|| std::ffi::OsString::from(".COM;.EXE;.BAT;.CMD"));
    resolve_program_in(program, &path, &pathext).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "PATH 上没有可启动的同名文件（已试 .exe/.com/.bat/.cmd）",
        )
    })
}

/// [`resolve_program`] 的**纯**内核：`PATH` 与 `PATHEXT` 由调用方给，便于单测不碰进程环境。
///
/// 查找顺序刻意与 `cmd.exe` 一致：**目录优先**（外层遍历 `PATH`，内层遍历扩展名），
/// 于是靠前目录里的 `claude.cmd` 会胜过靠后目录里的 `claude.exe`——这正是用户在命令行敲
/// `claude` 时实际跑到的那一个。反过来（扩展名优先）会起到另一个文件，排障时极难发现。
#[cfg(windows)]
fn resolve_program_in(
    program: &str,
    path: &std::ffi::OsStr,
    pathext: &std::ffi::OsStr,
) -> Option<PathBuf> {
    let exts: Vec<String> = pathext
        .to_string_lossy()
        .split(';')
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| LAUNCHABLE_EXTS.contains(&e.as_str()))
        .collect();
    // `PATHEXT` 被人改坏（或只剩 `.VBS`）时不能就此瘫掉：退回内置表。
    let exts: Vec<&str> = if exts.is_empty() {
        LAUNCHABLE_EXTS.to_vec()
    } else {
        exts.iter().map(String::as_str).collect()
    };

    let raw = Path::new(program);
    let has_launchable_ext = raw.extension().is_some_and(|e| {
        let dotted = format!(".{}", e.to_string_lossy().to_ascii_lowercase());
        LAUNCHABLE_EXTS.contains(&dotted.as_str())
    });
    // 调用方给了路径（含分隔符）或已带扩展名：不在 `PATH` 上乱找，只确认它存在。
    let explicit = raw.components().count() > 1;
    if explicit || has_launchable_ext {
        if has_launchable_ext {
            return raw.is_file().then(|| raw.to_path_buf());
        }
        return try_exts(raw, &exts);
    }

    std::env::split_paths(path).find_map(|dir| try_exts(&dir.join(program), &exts))
}

/// 给一个无扩展名的候选路径逐个试扩展名。
#[cfg(windows)]
fn try_exts(stem: &Path, exts: &[&str]) -> Option<PathBuf> {
    exts.iter().find_map(|ext| {
        let mut name = stem.as_os_str().to_os_string();
        name.push(ext);
        let candidate = PathBuf::from(name);
        candidate.is_file().then_some(candidate)
    })
}

// ── Windows Job Object ────────────────────────────────────────────────────────

#[cfg(windows)]
mod win {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// 一个匿名 Job Object 的句柄。
    ///
    /// **刻意不实现 `Send`**（`HANDLE` 是裸指针，自动就不是 `Send`）：本类型只在主线程的
    /// [`super::ChildProc`] 里存活。若哪天需要跨线程，请连同"谁负责 `CloseHandle`"一起重新
    /// 设计——`KILL_ON_JOB_CLOSE` 让"多一份句柄"与"少一份句柄"都会改变杀进程的时机。
    pub struct JobHandle(HANDLE);

    impl JobHandle {
        /// 建一个带 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的匿名 job。
        ///
        /// 失败返回 `None` 并只记一条 warn：**job 拿不到不该阻止会话启动**，
        /// 降级后果是"Lumen 崩溃时可能残留 claude 进程树"，而不是"功能不可用"。
        pub fn create_kill_on_close() -> Option<Self> {
            // SAFETY: 两个参数都传 null = 默认安全属性 + 匿名 job，这是 Win32 文档给出的
            // 合法调用形式；返回值按文档在失败时为 NULL。
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                log::warn!("CreateJobObjectW 失败，LLM runner 退化为只杀直接子进程");
                return None;
            }
            let job = Self(handle);

            // 用结构体字面量而不是 `default()` 后改字段：后者会触发
            // `clippy::field_reassign_with_default`，且字面量能一眼看出"只设了这一位"。
            let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                    LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    ..Default::default()
                },
                ..Default::default()
            };
            // SAFETY: `info` 是本地 `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`，类型与
            // `JobObjectExtendedLimitInformation` 匹配，长度用 size_of 取自同一类型。
            let ok = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                        .unwrap_or(u32::MAX),
                )
            };
            if ok == 0 {
                // 没设上 KILL_ON_JOB_CLOSE 的 job 仍然有用（`TerminateJobObject` 照样杀整树），
                // 丢的只是"Lumen 被强杀时的兜底"。故保留句柄、只 warn。
                log::warn!("SetInformationJobObject(KILL_ON_JOB_CLOSE) 失败，崩溃兜底失效");
            }
            Some(job)
        }

        /// 把已 spawn 的子进程放进本 job。
        ///
        /// 失败只 warn：Win8+ 支持嵌套 job，但在某些容器 / 调试器环境下仍可能被拒。
        /// 降级后果是这棵树不受 job 管辖，`kill_tree` 退化成只杀直接子进程。
        pub fn assign(&self, child: &Child) {
            // `RawHandle` 与 windows-sys 的 `HANDLE` 都是 `*mut core::ffi::c_void`，
            // 是**同一个类型**，故这里不需要（也不该加）任何转换。
            let process: HANDLE = child.as_raw_handle();
            // SAFETY: `process` 来自存活的 `Child`（本函数持有其借用，句柄在调用期间有效），
            // `self.0` 是本类型构造时校验过非空的 job 句柄。
            let ok = unsafe { AssignProcessToJobObject(self.0, process) };
            if ok == 0 {
                log::warn!(
                    "AssignProcessToJobObject 失败（pid={}），进程树清理退化为只杀直接子进程",
                    child.id()
                );
            }
        }

        /// 杀掉 job 里的**全部**进程。
        pub fn terminate(&self, exit_code: u32) {
            // SAFETY: `self.0` 是构造时校验过的 job 句柄；对已空的 job 调用是合法 no-op。
            let ok = unsafe { TerminateJobObject(self.0, exit_code) };
            if ok == 0 {
                log::debug!("TerminateJobObject 返回失败（job 可能已空）");
            }
        }
    }

    impl Drop for JobHandle {
        /// 关句柄。**这一步本身就会杀进程**（`KILL_ON_JOB_CLOSE`），
        /// 所以绝不能为了"提前释放句柄"而单独 drop 它——那等于杀掉还在跑的会话。
        fn drop(&mut self) {
            // SAFETY: `self.0` 由 `CreateJobObjectW` 得到且只在此处关闭一次。
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// 本次测试专用的临时目录（不引 tempfile 依赖；`Drop` 里清）。
    #[cfg_attr(not(windows), allow(dead_code))]
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let dir = std::env::temp_dir().join(format!("lumen-llm-{tag}-{nanos}"));
            std::fs::create_dir_all(&dir).expect("建临时目录");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 一个「打印一行就退出」的真进程。**不是 claude**——冒烟测要的是走通
    /// `Command::spawn` → 三条管道 → `try_wait`，不是验证 CLI 本身。
    #[cfg(windows)]
    fn echo_spec() -> (&'static str, Vec<String>) {
        ("cmd", vec!["/c".into(), "echo".into(), "hello".into()])
    }

    #[cfg(not(windows))]
    fn echo_spec() -> (&'static str, Vec<String>) {
        ("sh", vec!["-c".into(), "echo hello".into()])
    }

    /// 轮询到进程退出（带上限，卡住就失败而不是挂死整个测试进程）。
    fn wait_exit(proc: &mut ChildProc) -> TryWaitOutcome {
        for _ in 0..600 {
            let outcome = proc.try_wait();
            if outcome != TryWaitOutcome::Running {
                return outcome;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("子进程 6 秒内没有退出");
    }

    /// **这条测试就是那个 blocker 的哨兵**：片 2 的 96 条单测全是纯函数驱动，
    /// 唯独 `ChildProc::spawn` 零覆盖，而它恰恰是坏的那一步。
    #[test]
    fn 真实spawn冒烟_三条管道通且能收到退出码() {
        let (program, args) = echo_spec();
        let cwd = std::env::temp_dir();
        let (mut proc, mut pipes) =
            ChildProc::spawn(program, &args, &[], &cwd).expect("必须能起得来");
        assert!(proc.pid().is_some());
        // 立刻 drop stdin，让子进程即使读 stdin 也会拿到 EOF。
        drop(pipes.stdin);

        let mut out = String::new();
        pipes.stdout.read_to_string(&mut out).expect("读 stdout");
        assert!(out.contains("hello"), "stdout 实际是 {out:?}");

        let outcome = wait_exit(&mut proc);
        let TryWaitOutcome::Exited(status) = outcome else {
            panic!("正常退出必须是 Exited，实际 {outcome:?}");
        };
        assert_eq!(status.and_then(|s| s.code()), Some(0));
        // 幂等：再问一次仍是同一个答案，绝不退回 Running。
        assert_eq!(proc.try_wait(), outcome);
    }

    #[test]
    fn 找不到的可执行名必须报_not_found() {
        let cwd = std::env::temp_dir();
        // 不用 `expect_err`：`ChildProc` / `ChildPipes` 刻意不实现 `Debug`（它们持有的是
        // 管道句柄与 job 句柄，打出来没有意义且容易被顺手写进日志）。
        let Err(err) = ChildProc::spawn("lumen-绝对不存在的-cli-9f3a", &[], &[], &cwd) else {
            panic!("不存在的程序不该起得来");
        };
        assert_eq!(
            err.kind(),
            io::ErrorKind::NotFound,
            "kind 变了会让 start() 把它报成 Spawn 而不是 CliNotFound"
        );
    }

    #[test]
    fn try_wait的错误路径必须报已退出_而不是还在跑() {
        // 这是初稿写反的那一条：注释说「当作已退出」，代码返回的却是「还在跑」。
        assert_eq!(
            classify_try_wait(Err(io::Error::other("句柄异常"))),
            TryWaitOutcome::Lost
        );
        assert_eq!(classify_try_wait(Ok(None)), TryWaitOutcome::Running);
    }

    #[test]
    fn 句柄丢失后每次询问都仍然是已退出() {
        let (program, args) = echo_spec();
        let cwd = std::env::temp_dir();
        let (mut proc, pipes) = ChildProc::spawn(program, &args, &[], &cwd).expect("必须能起得来");
        drop(pipes);
        proc.force_lost_for_test();
        for _ in 0..3 {
            assert_eq!(
                proc.try_wait(),
                TryWaitOutcome::Lost,
                "一旦丢了句柄就必须**永远**报终态；退回 Running 会让 runner 永久占名额"
            );
        }
    }

    // ── Windows 可执行名解析（那个 blocker 的正面覆盖）──────────────────────

    #[cfg(windows)]
    #[test]
    fn windows_能解析出npm风格的_cmd_shim() {
        // 复刻本机现场：`%APPDATA%\npm` 下只有 `claude`（无扩展名的 sh 脚本）与 `claude.cmd`，
        // **没有 claude.exe**。`Command::new("claude")` 在这种目录里必然 NotFound。
        let dir = TempDir::new("resolve");
        std::fs::write(dir.path().join("claude"), b"#!/bin/sh\n").expect("写无扩展名 shim");
        std::fs::write(dir.path().join("claude.cmd"), b"@echo off\r\n").expect("写 cmd shim");
        std::fs::write(dir.path().join("claude.ps1"), b"# ps\r\n").expect("写 ps1 shim");

        let path = dir.path().as_os_str().to_os_string();
        let got = resolve_program_in("claude", &path, std::ffi::OsStr::new(".COM;.EXE;.BAT;.CMD"))
            .expect("必须解析到 claude.cmd");
        assert_eq!(got, dir.path().join("claude.cmd"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_不认_pathext_里起不来的扩展名() {
        let dir = TempDir::new("resolve-bad");
        std::fs::write(dir.path().join("thing.vbs"), b"' vbs\r\n").expect("写 vbs");
        let path = dir.path().as_os_str().to_os_string();
        // `.VBS` 在 PATHEXT 里，但 CreateProcessW 起不来 ⇒ 必须当作没找到。
        assert!(
            resolve_program_in("thing", &path, std::ffi::OsStr::new(".VBS;.EXE")).is_none(),
            "认下一个起不来的扩展名，会把「没装」误报成「起不来」"
        );
        // PATHEXT 被改坏（一个可启动扩展名都不剩）时退回内置表，而不是整个瘫掉。
        std::fs::write(dir.path().join("thing.cmd"), b"@echo off\r\n").expect("写 cmd");
        assert_eq!(
            resolve_program_in("thing", &path, std::ffi::OsStr::new(".VBS")),
            Some(dir.path().join("thing.cmd"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_按目录优先查找_靠前目录的cmd胜过靠后目录的exe() {
        let first = TempDir::new("resolve-a");
        let second = TempDir::new("resolve-b");
        std::fs::write(first.path().join("tool.cmd"), b"@echo off\r\n").expect("写 cmd");
        std::fs::write(second.path().join("tool.exe"), b"MZ").expect("写 exe");
        let path = std::env::join_paths([first.path(), second.path()]).expect("拼 PATH");
        assert_eq!(
            resolve_program_in("tool", &path, std::ffi::OsStr::new(".EXE;.CMD")),
            Some(first.path().join("tool.cmd")),
            "顺序必须与 cmd.exe 一致，否则我们起的不是用户敲 `tool` 时跑到的那一个"
        );
    }

    /// **真的把一个 `.cmd` 批处理 shim 起起来**——blocker 修复的端到端证明。
    ///
    /// Rust 自 1.77.2（CVE-2024-24576）起对 `.bat`/`.cmd` 自动转交 `cmd.exe` 并转义参数，
    /// 故这条同时也在守「标准库那条行为还在」。
    #[cfg(windows)]
    #[test]
    fn windows_真的能起一个_cmd_shim() {
        let dir = TempDir::new("shim");
        let shim = dir.path().join("fake-cli.cmd");
        std::fs::write(&shim, b"@echo off\r\necho shim-ok %1\r\n").expect("写 shim");
        let program = shim.to_string_lossy().into_owned();
        let (mut proc, mut pipes) =
            ChildProc::spawn(&program, &["arg with space".to_owned()], &[], dir.path())
                .expect(".cmd shim 必须能起得来");
        drop(pipes.stdin);
        let mut out = String::new();
        pipes.stdout.read_to_string(&mut out).expect("读 stdout");
        assert!(out.contains("shim-ok"), "stdout 实际是 {out:?}");
        assert!(
            out.contains("arg with space"),
            "带空格的参数必须原样送达（标准库负责转义），实际 {out:?}"
        );
        assert!(matches!(wait_exit(&mut proc), TryWaitOutcome::Exited(_)));
    }
}
