//! Win32 的一点点胶水。只有"把 Rust 字符串喂给宽字符 API"和几个颜色常量。

use std::ffi::c_void;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowTextLengthW, GetWindowTextW};

/// Rust 字符串 → 带 NUL 的 UTF-16。**调用方必须保证返回值活到 API 调用结束**，
/// 所以这里返回 Vec 而不是 `&[u16]`：直接把临时值取指针会立刻悬垂。
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// COLORREF 是 0x00BBGGRR，不是 RGB。写反了颜色会整体偏蓝/偏红，
/// 而且很难一眼看出——这里统一从 RGB 三元组构造，避免各处手写十六进制。
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

pub fn get_text(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h);
        if n <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        if got <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..got as usize])
    }
}

/// 让一个 HWND 通过 USERDATA 挂上任意状态，取回时强转。
/// 只在窗口过程里用，生命周期由窗口本身保证。
pub fn put_state<T>(h: HWND, p: *mut T) {
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
            h,
            windows_sys::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
            p as isize,
        );
    }
}

pub fn state<'a, T>(h: HWND) -> Option<&'a mut T> {
    unsafe {
        let p = windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            h,
            windows_sys::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
        ) as *mut T;
        p.as_mut()
    }
}

/// 空指针常量，省得到处写 `std::ptr::null_mut()`。
pub const NO: *mut c_void = std::ptr::null_mut();
