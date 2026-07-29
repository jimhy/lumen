//! Windows 安装后首启同步。
//!
//! Inno Setup 的 `[Run]` 在 Setup 进程彻底退出前执行。Lumen 一启动就会
//! 并发恢复多个 PowerShell，会让 Scoop shim 在安装事务仍在收尾的极短
//! 窗口内访问 `current` junction。安装器通过 `--wait-for-installer=<pid>`
//! 传入自身 PID；这里在初始化日志、单实例锁、窗口和 PTY 之前等待它退出。

use std::time::Duration;

const WAIT_ARG_PREFIX: &str = "--wait-for-installer=";
const INSTALLER_WAIT_TIMEOUT_MS: u32 = 30_000;
const SETTLE_DELAY: Duration = Duration::from_millis(750);

fn installer_pid<I>(args: I) -> Option<u32>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    args.into_iter().find_map(|arg| {
        arg.as_ref()
            .strip_prefix(WAIT_ARG_PREFIX)
            .and_then(|pid| pid.parse::<u32>().ok())
            .filter(|pid| *pid != 0 && *pid != std::process::id())
    })
}

pub fn wait_for_installer_if_requested() {
    let Some(pid) = installer_pid(std::env::args().skip(1)) else {
        return;
    };

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };

    // SAFETY: 只申请等待权限，不读写目标进程。句柄非空时在本函数内关闭一次。
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if !handle.is_null() {
        // Setup 的 [Run] 使用 nowait，因此不会与这里互相等待。
        // SAFETY: handle 是上面成功打开的进程句柄。
        unsafe {
            WaitForSingleObject(handle, INSTALLER_WAIT_TIMEOUT_MS);
            CloseHandle(handle);
        }
    }

    // Setup 退出后再留一小段时间，让 Restart Manager、临时文件清理和
    // 安全软件的安装后扫描完成；只影响带专用参数的安装后首启。
    std::thread::sleep(SETTLE_DELAY);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_installer_pid() {
        assert_eq!(
            installer_pid(["--other", "--wait-for-installer=1234"]),
            Some(1234)
        );
    }

    #[test]
    fn rejects_invalid_installer_pid() {
        assert_eq!(installer_pid(["--wait-for-installer="]), None);
        assert_eq!(installer_pid(["--wait-for-installer=abc"]), None);
        assert_eq!(installer_pid(["--wait-for-installer=0"]), None);
        assert_eq!(installer_pid(["--wait-for-installer-id=1234"]), None);
    }
}
