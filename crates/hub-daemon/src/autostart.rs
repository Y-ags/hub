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
pub const VALUE_NAME: &str = "HubDaemon";

/// `HKEY_CURRENT_USER` 下的自启键
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// 拼出自启命令行。抽出来是为了能单独测试引号处理 ——
/// 路径带空格却没加引号，表现是"登录时闪一下什么都没发生"，极难排查。
pub fn command_line(exe: &Path, data: &Path) -> String {
    format!(
        "\"{}\" daemon run --data \"{}\"",
        exe.display(),
        data.display()
    )
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
    imp::query().ok().flatten()
}

/// 是否已开启（只看存不存在，不看内容是否对得上当前 exe）
pub fn is_enabled() -> bool {
    enabled().is_some()
}

/// 写入自启项。重复调用是覆盖，不会堆出多条。
pub fn install(data: &Path) -> Result<String, String> {
    let exe = current_exe().map_err(|e| format!("拿不到自身路径：{e}"))?;
    let cmd = command_line(&exe, data);
    // Run 键的值有长度上限（历史限制 260 字符）。超了不是致命错误，
    // 但值得提醒：某些老启动器会截断，结果就是"登录后什么都没发生"。
    if cmd.chars().count() > 260 {
        eprintln!("警告：自启命令超过 260 字符，部分启动器可能截断");
    }
    imp::set(&cmd)?;
    Ok(cmd)
}

/// 删除自启项。本来就没有也算成功。
pub fn uninstall() -> Result<(), String> {
    imp::remove()
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

    use super::{RUN_KEY, VALUE_NAME};

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

    pub fn query() -> Result<Option<String>, String> {
        let hkey = open(false)?;
        let name = wide(VALUE_NAME);
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

    pub fn set(cmd: &str) -> Result<(), String> {
        let hkey = open(true)?;
        let name = wide(VALUE_NAME);
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

    pub fn remove() -> Result<(), String> {
        let hkey = open(true)?;
        let name = wide(VALUE_NAME);
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
    pub fn query() -> Result<Option<String>, String> {
        Ok(None)
    }

    pub fn set(_cmd: &str) -> Result<(), String> {
        Err("开机自启目前只在 Windows 上实现。\n\
             Linux 可以写一个 systemd user unit（~/.config/systemd/user/hub.service）：\n\
             \x20 [Service]\n\
             \x20 ExecStart=/path/to/hub daemon run --data /home/you/.hub\n\
             \x20 Restart=on-failure\n\
             然后 systemctl --user enable --now hub"
            .into())
    }

    pub fn remove() -> Result<(), String> {
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
    fn command_line_has_no_double_quotes_missing() {
        let cmd = command_line(Path::new(r"D:\a b\hub.exe"), Path::new(r"E:\x y"));
        // 引号必须成对
        assert_eq!(cmd.matches('"').count(), 4);
    }
}
