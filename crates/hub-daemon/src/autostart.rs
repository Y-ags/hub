//! 开机自启。
//!
//! 后台索引的最后一环：`hub daemon start` 让 daemon 活到关机为止，
//! 但重启之后它就没了 —— 用户还是得记得手动敲一次。那不叫"自动更新"，
//! 那叫"自动更新，前提是你每天记得把它叫起来"。
//!
//! ## 为什么写注册表而不是快捷方式 / 计划任务
//!
//! | 方式 | 需要管理员 | 用户可见 | 卸载干净 |
//! |---|---|---|---|
//! | `HKCU\...\CurrentVersion\Run` | 否 | 任务管理器「启动」里能看到并禁用 | 删一个值 |
//! | 启动文件夹快捷方式 | 否 | 要打开 shell:startup 才看得到 | 要找到 .lnk |
//! | 计划任务 (`schtasks`) | 视参数 | 藏得深 | 要额外一条命令 |
//!
//! `Run` 键是这里唯一一个"不需要提权、用户能自己关掉、我们能干净删掉"的方案。
//! 写在 HKCU 下意味着只对当前用户生效，也不需要 UAC —— 这点很重要：
//! 一个个人检索工具没有理由要求管理员权限。
//!
//! ## 命令行长什么样
//!
//! ```text
//! "C:\path\hub.exe" daemon run --data "C:\Users\me\.hub"
//! ```
//!
//! 两个路径都必须带引号（用户名里常有空格），`--data` 必须写绝对路径：
//! 登录时拉起进程的工作目录不是项目目录，相对路径会指到一个随机的地方。

use std::path::Path;

/// 注册表里值的名字。用固定的名字而不是带 pid/时间戳的：
/// 重复 install 应该覆盖同一个值，而不是堆出一串自启项。
///
/// 常驻进程有两个（守护进程、搜索窗），各占一个值 —— 装自启时不该
/// 让对方从"启动"列表里消失，取消时也不该把对方一起删掉。
pub const VALUE_DAEMON: &str = "HubDaemon";
pub const VALUE_UI: &str = "HubUi";

/// `HKEY_CURRENT_USER` 下的自启键
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// 拼出守护进程的自启命令行。抽出来是为了能单独测试引号处理 ——
/// 路径带空格却没加引号，表现是"登录时闪一下什么都没发生"，极难排查。
pub fn command_line(exe: &Path, data: &Path) -> String {
    format!(
        "\"{}\" daemon run --data \"{}\"",
        win_path(exe),
        win_path(data)
    )
}

/// 路径写成 Windows 的样子：反斜杠、不带引号（引号由调用处加）。
///
/// `--data C:/Users/me/.hub` 是合法的用户输入，Windows 也认，但写进注册表
/// 之后就成了一条"看着不像本机路径"的记录 —— 以后排查开机启动问题时，
/// 第一个念头会是"这值是不是手改过"。统一成反斜杠消除这个歧义。
///
/// **不要用 `canonicalize`**：Windows 上它返回 `\\?\C:\...` 的 verbatim 形式，
/// 而某些启动器解析不了这个前缀（文档里已有同样的告诫）。
fn win_path(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\")
}

/// 拼出搜索窗的自启命令行。
///
/// 带 `--hidden`：登录时自动弹一个搜索框出来是骚扰，用户要的是"按热键它就在"。
/// 不带 `--detach`：登录项本来就是脱离于任何终端的，再加这个标志没有意义。
pub fn ui_command_line(exe: &Path, data: &Path, hotkey: Option<&str>) -> String {
    match hotkey {
        Some(h) => format!(
            "\"{}\" ui --hidden --data \"{}\" --hotkey {}",
            win_path(exe),
            win_path(data),
            h
        ),
        None => format!(
            "\"{}\" ui --hidden --data \"{}\"",
            win_path(exe),
            win_path(data)
        ),
    }
}

/// 当前可执行文件的路径。
///
/// 不用 `std::env::current_exe()` 直接落地：它在极少数情况下返回的是
/// 短路径或带 `\\?\` 前缀的形式，写进注册表后某些启动器解析不了。
/// 这里只在失败时退回，不做多余处理。
pub fn current_exe() -> std::io::Result<std::path::PathBuf> {
    std::env::current_exe()
}

/// 已安装的自启命令。`None` 表示没开。
pub fn enabled() -> Option<String> {
    enabled_as(VALUE_DAEMON)
}

/// 某个自启项的内容。`None` = 没装。
pub fn enabled_as(value: &str) -> Option<String> {
    imp::query(value).ok().flatten()
}

/// 是否已开启（只看存不存在，不看内容是否对得上当前 exe）
pub fn is_enabled() -> bool {
    enabled().is_some()
}

pub fn is_enabled_as(value: &str) -> bool {
    enabled_as(value).is_some()
}

/// 写入守护进程的自启项。重复调用是覆盖，不会堆出多条。
pub fn install(data: &Path) -> Result<String, String> {
    let exe = current_exe().map_err(|e| format!("拿不到自身路径：{e}"))?;
    let cmd = command_line(&exe, data);
    install_as(VALUE_DAEMON, &cmd)?;
    Ok(cmd)
}

/// 写入任意自启项。值名固定，所以重复调用是覆盖而不是堆积。
pub fn install_as(value: &str, cmd: &str) -> Result<(), String> {
    // Run 键的值有长度上限（历史限制 260 字符）。超了不是致命错误，
    // 但值得提醒：某些老启动器会截断，结果就是"登录后什么都没发生"。
    if cmd.chars().count() > 260 {
        eprintln!("警告：自启命令超过 260 字符，部分启动器可能截断");
    }
    imp::set(value, cmd)
}

/// 删除守护进程的自启项。本来就没有也算成功。
pub fn uninstall() -> Result<(), String> {
    uninstall_as(VALUE_DAEMON)
}

pub fn uninstall_as(value: &str) -> Result<(), String> {
    imp::remove(value)
}

// ============================================================
// Windows 实现
// ============================================================

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
        HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
    };

    use super::RUN_KEY;

    /// UTF-16 + NUL。所有注册表 API 都要这个形状。
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 打开自启键。`want_write` 决定申请哪些权限：
    /// 只查询时不要 `KEY_SET_VALUE` —— 有些受限环境下会因此打不开。
    fn open(want_write: bool) -> Result<windows_sys::Win32::System::Registry::HKEY, String> {
        let sub = wide(RUN_KEY);
        let access = if want_write {
            KEY_QUERY_VALUE | KEY_SET_VALUE
        } else {
            KEY_QUERY_VALUE
        };
        let mut hkey = std::ptr::null_mut();
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, access, &mut hkey) };
        if rc != ERROR_SUCCESS {
            return Err(format!("打开注册表键失败（错误码 {rc}）"));
        }
        Ok(hkey)
    }

    pub fn query(value: &str) -> Result<Option<String>, String> {
        let hkey = open(false)?;
        let name = wide(value);
        let mut buf = [0u16; 1024];
        let mut len_bytes: u32 = (buf.len() * 2) as u32;
        let mut ty: u32 = 0;
        let rc = unsafe {
            RegQueryValueExW(
                hkey,
                name.as_ptr(),
                std::ptr::null(),
                &mut ty,
                buf.as_mut_ptr() as *mut u8,
                &mut len_bytes,
            )
        };
        unsafe { RegCloseKey(hkey) };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if rc != ERROR_SUCCESS {
            return Err(format!("读取注册表值失败（错误码 {rc}）"));
        }
        // len_bytes 含结尾的 NUL，按 u16 个数算要除 2
        let units = (len_bytes as usize / 2).min(buf.len());
        let end = buf[..units]
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(units);
        Ok(Some(String::from_utf16_lossy(&buf[..end])))
    }

    pub fn set(value: &str, cmd: &str) -> Result<(), String> {
        let hkey = open(true)?;
        let name = wide(value);
        let data = wide(cmd);
        let rc = unsafe {
            RegSetValueExW(
                hkey,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            )
        };
        unsafe { RegCloseKey(hkey) };
        if rc != ERROR_SUCCESS {
            return Err(format!("写入注册表失败（错误码 {rc}）"));
        }
        Ok(())
    }

    pub fn remove(value: &str) -> Result<(), String> {
        let hkey = open(true)?;
        let name = wide(value);
        let rc = unsafe { RegDeleteValueW(hkey, name.as_ptr()) };
        unsafe { RegCloseKey(hkey) };
        // 本来就没有 → 我们的目标状态已经达成，不该报错
        if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
            return Err(format!("删除注册表值失败（错误码 {rc}）"));
        }
        Ok(())
    }
}

// ============================================================
// 非 Windows：给出可执行的指引，而不是一句"不支持"
// ============================================================

#[cfg(not(windows))]
mod imp {
    pub fn query(_value: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    pub fn set(_value: &str, _cmd: &str) -> Result<(), String> {
        Err("开机自启目前只在 Windows 上实现。\n\
             Linux 可以写一个 systemd user unit（~/.config/systemd/user/hub.service）：\n\
             \x20 [Service]\n\
             \x20 ExecStart=/path/to/hub daemon run --data /home/you/.hub\n\
             \x20 Restart=on-failure\n\
             然后 systemctl --user enable --now hub"
            .into())
    }

    pub fn remove(_value: &str) -> Result<(), String> {
        Err("非 Windows 平台没有注册表自启项可删".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_quotes_both_paths() {
        let cmd = command_line(
            Path::new(r"C:\Program Files\hub\hub.exe"),
            Path::new(r"C:\Users\Some User\.hub"),
        );
        // 两个路径都必须带引号，否则 Program Files 会被拆成两个参数
        assert_eq!(
            cmd,
            r#""C:\Program Files\hub\hub.exe" daemon run --data "C:\Users\Some User\.hub""#
        );
        // 参数顺序是 daemon run，不是 run daemon：写反了登录时静默失败
        assert!(cmd.contains("daemon run"));
    }

    #[test]
    fn ui_command_line_is_hidden_and_quoted() {
        let cmd = ui_command_line(
            Path::new(r"C:\Program Files\hub\hub.exe"),
            Path::new(r"C:\Users\Some User\.hub"),
            None,
        );
        assert_eq!(
            cmd,
            r#""C:\Program Files\hub\hub.exe" ui --hidden --data "C:\Users\Some User\.hub""#
        );
        assert_eq!(cmd.matches('"').count(), 4);
        // 登录时自己弹一个搜索框出来是骚扰，用户要的是"按热键它就在"
        assert!(cmd.contains("--hidden"));
        // 登录项本来就没有父终端，--detach 在这里没有意义
        assert!(!cmd.contains("--detach"));
    }

    #[test]
    fn forward_slashes_become_backslashes() {
        // 用户完全可以输 `--data C:/Users/me/.hub`，Windows 也认；
        // 但注册表里留一条正斜杠路径会让人以为是手改的
        let cmd = ui_command_line(Path::new("D:/a/hub.exe"), Path::new("C:/Users/me/.hub"), None);
        assert_eq!(cmd, r#""D:\a\hub.exe" ui --hidden --data "C:\Users\me\.hub""#);
        assert!(!cmd.contains('/'), "不该残留正斜杠：{cmd}");
    }

    #[test]
    fn ui_command_line_carries_custom_hotkey() {
        let cmd = ui_command_line(Path::new(r"D:\hub.exe"), Path::new(r"E:\data"), Some("Ctrl+Alt+J"));
        // 热键必须跟在最后且不加引号：加引号也能解析，但会和路径的引号
        // 混在一起，以后想用正则改这条命令行的人会很难受
        assert!(cmd.ends_with("--hotkey Ctrl+Alt+J"), "实际：{cmd}");
        assert_eq!(cmd.matches('"').count(), 4);
    }

    #[test]
    fn command_line_has_no_double_quotes_missing() {
        let cmd = command_line(Path::new(r"D:\a b\hub.exe"), Path::new(r"E:\x y"));
        // 引号必须成对
        assert_eq!(cmd.matches('"').count(), 4);
    }
}
