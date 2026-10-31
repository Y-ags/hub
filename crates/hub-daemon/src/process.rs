//! 进程级操作：后台脱离启动、强制结束。
//!
//! 这些放这里而不是 CLI，是为了让 `windows-sys` 只出现在一个 crate 的依赖里。
//! CLI 需要的只是一个 `force_kill(pid) -> bool`。

use std::path::Path;

/// 启动一个脱离当前终端的守护进程。
///
/// Windows 上三个标志缺一不可：
///
/// | 标志 | 作用 | 不加会怎样 |
/// |---|---|---|
/// | `DETACHED_PROCESS` | 不继承父进程的控制台 | 父进程退出（或终端关闭）时子进程被一起带走 |
/// | `CREATE_NEW_PROCESS_GROUP` | 自己的进程组 | 父进程收到的 Ctrl-Break 会传给子进程 |
/// | `CREATE_BREAKAWAY_FROM_JOB` | 逃出父进程所在的 Job Object | 父进程被放进 Job 且退出时，整个 Job 的东西都被杀 —— 表现是"start 报告成功，但 status 立刻显示已停止" |
///
/// 最后一个不是所有环境都允许（Job 必须设了 `JOB_OBJECT_LIMIT_BREAKAWAY_OK`），
/// 不允许时 `CreateProcess` 直接返回 `ERROR_ACCESS_DENIED`，所以这里降级重试一次。
///
/// 三个标准流全部重定向到 null：daemon 自己往日志文件写，
/// 保留继承的 stdout 句柄在脱离控制台后写入会失败。
pub fn spawn_detached(data: &Path) -> std::io::Result<u32> {
    #[cfg(windows)]
    {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

        let try_flags = |flags: u32| -> std::io::Result<u32> {
            let mut cmd = base_command(data)?;
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(flags);
            }
            cmd.spawn().map(|c| c.id())
        };

        match try_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB) {
            Ok(pid) => Ok(pid),
            // Job 不允许 breakaway：退回不带该标志（此时只能期望父进程不被杀 Job）
            Err(_) => try_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP),
        }
    }
    #[cfg(not(windows))]
    {
        // Unix 上父进程退出不会带走子进程，重定向标准流即可
        base_command(data).and_then(|mut c| c.spawn().map(|x| x.id()))
    }
}

fn base_command(data: &Path) -> std::io::Result<std::process::Command> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    // run 是前台实现，被 detach 之后它就是后台进程
    cmd.arg("daemon")
        .arg("run")
        .arg("--data")
        .arg(data)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(cmd)
}

/// 进程是否还在运行。
///
/// 为什么不能只看心跳：心跳每 15 秒才写一次，进程在两次心跳之间死掉时，
/// `hub daemon stop` 会白等 90 秒才确认它已经不在了。
///
/// 为什么不能只看 pid：pid 会被系统复用，几天前的 pid 很可能已经被别的进程占住。
/// 所以 Windows 上用 `OpenProcess` + `WaitForSingleObject(0)`：
/// 句柄能打开且没收到"已退出"信号，才是真的活着。
pub fn pid_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        unsafe {
            let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if h.is_null() {
                return false;
            }
            // 0 = 立刻返回，不做等待
            let r = WaitForSingleObject(h, 0);
            CloseHandle(h);
            // WAIT_TIMEOUT 表示还在跑；WAIT_OBJECT_0 表示已退出
            r == WAIT_TIMEOUT
        }
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// 强制结束进程。只在 `hub daemon stop --force` 时使用。
///
/// 常规停止是写标记文件让主循环自己收尾（会把攒着的变更写完）。
/// 强杀是兜底：进程卡在某个系统调用里、或标记文件因为权限写不进去时用。
/// 代价是可能留下半写的索引段，下次启动 Tantivy 会做一次恢复。
pub fn force_kill(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if h.is_null() {
                return false;
            }
            let ok = TerminateProcess(h, 1) != 0;
            CloseHandle(h);
            ok
        }
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 用 SIGKILL
        std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}
