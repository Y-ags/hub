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
    let mut cmd = base_command(data)?;
    spawn_detached_command(&mut cmd).map(|c| c.id())
}

/// 给任意一条已拼好参数的命令加上"脱离父进程"的处理，然后启动。
///
/// 从 `spawn_detached` 里抽出来，是因为 GUI 那条路（`hub ui --detach`）要起的是
/// 另一个子命令，而它第一版把这几个标志位**又抄了一遍**、恰好漏了
/// `CREATE_BREAKAWAY_FROM_JOB` —— 表现是命令打印"已在后台启动"、任务管理器里
/// 也真能看到进程，一秒后它自己没了。抄常量这种事只该有一份。
pub fn spawn_detached_command(
    cmd: &mut std::process::Command,
) -> std::io::Result<std::process::Child> {
    use std::process::Stdio;
    // daemon 自己往日志文件写、界面自己开窗口，留着继承来的标准流只会
    // 在控制台消失后写入失败
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // 必须先清掉"可继承"标记，再 spawn。原因见函数注释 —— 这一步不做，
    // `hub daemon start | tail` 会一直挂着。
    clear_std_handle_inheritance();

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            Ok(c) => Ok(c),
            // Job 不允许 breakaway（ERROR_ACCESS_DENIED）：退回不带该标志。
            // 此时只能指望父进程不在一个"退出就杀光"的 Job 里。
            Err(_) => {
                cmd.creation_flags(flags);
                cmd.spawn()
            }
        }
    }
    #[cfg(not(windows))]
    {
        // Unix 上父进程退出不会带走子进程，重定向标准流就够了
        cmd.spawn()
    }
}

/// 把本进程的 stdin / stdout / stderr 标成"不可继承"。
///
/// ## 为什么必须做（实测出来的，不是理论洁癖）
///
/// Windows 的 `CreateProcess` 一旦 `bInheritHandles = TRUE`（`Command::spawn`
/// 在需要重定向标准流时必然如此），就会把**当前进程里所有带继承标记的句柄**
/// 复制给子进程 —— 不只是我们显式指定的那三个。
///
/// 于是 `hub daemon start ~/code | tail -3` 这种写法会这样：
/// 1. shell 给管道两端建了可继承的句柄；
/// 2. `hub` 进程启动 daemon 子进程，子进程顺手继承了**管道写端**；
/// 3. `hub` 退出，但管道的写端还被后台的 daemon 拿着；
/// 4. `tail` 等不到 EOF，永远不结束 —— 命令看起来"卡死了"，其实 daemon 早已就位。
///
/// 实测：不加这一步，`hub daemon start | tail` 100% 挂住（等了十分钟也没返回）。
/// 加了之后立刻正常。同样的坑对 `hub ui --detach | ...` 一样成立。
///
/// 反过来不成立：我们自己的标准流不需要被任何子进程继承（子进程的流是
/// `Stdio::null()`），所以清掉标记没有副作用。
fn clear_std_handle_inheritance() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{
            SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };

        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = GetStdHandle(which);
            // 没有控制台时（比如从计划任务里跑）这里会是 NULL，
            // 句柄无效时 SetHandleInformation 会失败，直接跳过
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                continue;
            }
            let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
        }
    }
}

fn base_command(data: &Path) -> std::io::Result<std::process::Command> {    use std::process::{Command, Stdio};
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
