//! 托盘图标 —— 给这个"故意没有任务栏按钮"的窗口留一个入口。
//!
//! 搜索窗是 `WS_POPUP | WS_EX_TOOLWINDOW`：不出现在任务栏、不进 Alt+Tab，
//! 这是"唤起式浮层"该有的样子。代价是**用户没有任何办法找到它** ——
//! Esc 只是隐藏，进程还在后台等热键。没有托盘图标时，想彻底退出
//! 只能去任务管理器结束进程，这不能算"做完了"。
//!
//! 图标不走资源文件（工程里没有 .rc，也不打算为 16×16 引入 rc.exe 那套），
//! 而是启动时用距离场算出一张 32 位 DIB 再 `CreateIconIndirect`：
//! 只画一个圆环 + 一根斜手柄，几何简单到可以单测。

use std::ffi::c_void;
use std::ptr;

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, HBITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    GetSystemMetrics, PostMessageW, RegisterWindowMessageW, SendMessageW, SetForegroundWindow,
    SetMenuDefaultItem, TrackPopupMenu, HICON, ICONINFO, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    SM_CXSMICON, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_LBUTTONDBLCLK, WM_LBUTTONUP,
    WM_NULL, WM_RBUTTONUP, WM_SETICON,
};

use crate::win::wide;

/// 托盘图标的回调消息（WM_APP + 2）。wParam 是图标 id，lParam 是鼠标消息。
pub const WM_TRAY: u32 = 0x8000 + 2;

const TRAY_ID: u32 = 1;
const CMD_SHOW: usize = 2001;
const CMD_QUIT: usize = 2002;

/// 用户在托盘菜单里选了什么。菜单用 `TPM_RETURNCMD` 直接返回 id，
/// 不走 `WM_COMMAND` —— 少一条消息路径，也就不用和输入框的通知码混在一起判。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    Show,
    Quit,
    None,
}

pub struct Tray {
    icon: HICON,
    /// `RegisterWindowMessageW("TaskbarCreated")`。Explorer 崩溃重启后
    /// 会广播这条消息，**所有托盘图标都会被清空**，收到就得重新加一次。
    /// 不处理的话：用户重启一次资源管理器，图标就永远消失了。
    taskbar_msg: u32,
    added: bool,
}

impl Tray {
    pub fn attach(hwnd: HWND, tip: &str) -> Option<Tray> {
        unsafe {
            let px = GetSystemMetrics(SM_CXSMICON).max(16);
            let icon = make_icon(px);
            if icon.is_null() {
                return None;
            }
            let name = wide("TaskbarCreated");
            let taskbar_msg = RegisterWindowMessageW(name.as_ptr());

            let mut t = Tray {
                icon,
                taskbar_msg,
                added: false,
            };
            // 顺手把窗口图标也设成它。不为好看，是为了**这个图标能被外部验证**：
            // 托盘里的 16px 图标肉眼几乎看不清，而 `WM_GETICON` 能拿到同一个
            // 句柄，外部工具就能把它画出来看 alpha 有没有问题
            SendMessageW(
                hwnd,
                WM_SETICON,
                0, // ICON_SMALL
                icon as isize,
            );
            t.added = t.send(hwnd, NIM_ADD, tip);
            Some(t)
        }
    }

    pub fn taskbar_msg(&self) -> u32 {
        self.taskbar_msg
    }

    /// 系统真的收下这个图标了吗。`Shell_NotifyIconW` 的返回值只有这里能看到，
    /// 出问题时（比如资源管理器正在重启）值得打一行日志。
    pub fn is_added(&self) -> bool {
        self.added
    }

    /// Explorer 重启了：系统那边的图标清单已经清空，**必须无条件重加一次**。
    ///
    /// 这里踩过两个坑：
    /// 1. 原来写的是 `if self.added { return; }`，看着像在防重复，实际上
    ///    `added` 只表示"我们加过"，不代表**新的** explorer 认识它。
    ///    Explorer 一重启，标记还是 true，于是永远跳过重加 —— 图标再也回不来。
    /// 2. 直接重发 `NIM_ADD` 会返回失败：同一个 hWnd + uID 还没删时，系统
    ///    不会覆盖。所以先删再加，删不掉也无所谓（真重启时本来就没了）。
    pub fn reattach(&mut self, hwnd: HWND, tip: &str) {
        self.send(hwnd, NIM_DELETE, "");
        self.added = self.send(hwnd, NIM_ADD, tip);
    }

    pub fn detach(&mut self, hwnd: HWND) {
        if !self.added {
            return;
        }
        // 进程退出时系统也会清，但显式删掉能让图标立刻消失，
        // 而不是留个"点了没反应"的幽灵图标在托盘里挂几秒
        self.send(hwnd, NIM_DELETE, "");
        self.added = false;
    }

    fn send(&self, hwnd: HWND, op: u32, tip: &str) -> bool {
        let d = nid(hwnd, self.icon, tip);
        unsafe { Shell_NotifyIconW(op, &d) != 0 }
    }

    /// 右键菜单。`hotkey` / `status` 是只读信息行，灰掉不可点。
    pub fn popup(&self, hwnd: HWND, hotkey: &str, status: &str) -> Cmd {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return Cmd::None;
            }

            let show = wide("打开搜索窗");
            AppendMenuW(menu, MF_STRING, CMD_SHOW, show.as_ptr());
            // 标成默认项（回车落到它身上）。**不要指望它加粗**：
            // Win11 那套加粗是 XAML 上下文菜单的效果，经典菜单里
            // MF_DEFAULT / SetMenuDefaultItem 都试过，字重没有变化
            SetMenuDefaultItem(menu, CMD_SHOW as u32, 0);

            AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
            if !hotkey.is_empty() {
                // 全角空格分隔：半角空格在中文之间挤成一团
                let s = wide(&format!("快捷键　{hotkey}"));
                AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, s.as_ptr());
            }
            if !status.is_empty() {
                let s = wide(status);
                AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, s.as_ptr());
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
            let quit = wide("退出");
            AppendMenuW(menu, MF_STRING, CMD_QUIT, quit.as_ptr());

            // 弹菜单前必须先把窗口提到前台，否则点空白处菜单不收起，
            // 要点两次才消失 —— MSDN 在 TrackPopupMenu 里专门写了这一条。
            SetForegroundWindow(hwnd);
            let mut p: POINT = std::mem::zeroed();
            GetCursorPos(&mut p);

            let id = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                p.x,
                p.y,
                0,
                hwnd,
                ptr::null(),
            );
            DestroyMenu(menu);
            // 配套动作：把窗口重新交回消息循环，菜单才会正常响应"点到别处"
            PostMessageW(hwnd, WM_NULL, 0, 0);

            match id as usize {
                CMD_SHOW => Cmd::Show,
                CMD_QUIT => Cmd::Quit,
                _ => Cmd::None,
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        if !self.icon.is_null() {
            unsafe {
                DestroyIcon(self.icon);
            }
        }
    }
}

/// 填 `NOTIFYICONDATAW`。`cbSize` 必须正好是这个结构体的大小，
/// 系统靠它判断调用方认识哪些字段；写小了后面几个字段会被静默忽略。
fn nid(hwnd: HWND, icon: HICON, tip: &str) -> NOTIFYICONDATAW {
    let mut d: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    d.hWnd = hwnd;
    d.uID = TRAY_ID;
    d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    d.uCallbackMessage = WM_TRAY;
    d.hIcon = icon;
    // szTip 是定长 [u16; 128]，多一个字就会写穿到 dwState。
    // 中英混排数不清长度，统一截到 127 再交给系统补 NUL
    let w: Vec<u16> = tip.encode_utf16().take(127).collect();
    d.szTip[..w.len()].copy_from_slice(&w);
    d
}

// ============================================================
// 图标：距离场画一个放大镜
// ============================================================

/// 放大镜的着色。深靛蓝在浅色任务栏和深色任务栏上都够亮，
/// 而且是界面强调色，视觉上跟搜索窗是一套的。
const INK: (u8, u8, u8) = (0x63, 0x66, 0xF1);

/// 4×4 超采样：每个像素采 16 个点，边缘就有 16 级透明度，
/// 16px 下也看不出锯齿。
const SS: u32 = 4;

fn make_icon(px: i32) -> HICON {
    unsafe {
        let (color, pixels) = dib(px, 32);
        if color.is_null() || pixels.is_null() {
            return ptr::null_mut();
        }

        let buf = std::slice::from_raw_parts_mut(pixels as *mut u8, (px * px * 4) as usize);
        paint_pixels(buf, px);

        // 掩码：1bpp 全 0（= 处处"不透明"）。带 alpha 的图标系统只看
        // alpha 通道，掩码留着只是为了满足 ICONINFO 非空的要求
        let (mask, _) = dib(px, 1);
        if mask.is_null() {
            DeleteObject(color as *mut c_void);
            return ptr::null_mut();
        }

        let info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info);
        // CreateIconIndirect 会另存一份，原图可以立刻删
        DeleteObject(color as *mut c_void);
        DeleteObject(mask as *mut c_void);
        icon
    }
}

/// 把放大镜写进一块 BGRA 缓冲。
///
/// 和 GDI 建位图那步分开，是因为**通道顺序最容易写反而最难看出来** ——
/// 32 位 DIB 的内存顺序是 BGRA 而不是 RGBA，写反了图标会偏色，
/// 而偏色的图标在托盘里看着"就是有点怪"，不会有人立刻发现。
/// 拆出来就能直接对着字节断言。
fn paint_pixels(buf: &mut [u8], px: i32) {
    let n = px as f32;
    for y in 0..px {
        for x in 0..px {
            // 4×4 超采样：每个像素采 16 个点，边缘就有 16 级透明度，
            // 16px 下也看不出锯齿。（GDI 的 Ellipse 在这么小的尺寸上
            // 毛刺明显，而且只能给硬边掩码，拿不到 alpha）
            let mut hit = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let fy = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    if inside(fx, fy, n) {
                        hit += 1;
                    }
                }
            }
            let a = (hit as f32 / (SS * SS) as f32 * 255.0).round() as u8;
            let i = ((y * px + x) * 4) as usize;
            buf[i] = INK.2;
            buf[i + 1] = INK.1;
            buf[i + 2] = INK.0;
            buf[i + 3] = a;
        }
    }
}

fn header(px: i32, bits: u16) -> BITMAPINFO {
    let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: px,
        // 负数 = 自上而下存。正数（自下而上）也行，但那样写像素要自己翻行，
        // 而这类代码翻错方向几乎看不出来
        biHeight: -px,
        biPlanes: 1,
        biBitCount: bits,
        biCompression: BI_RGB,
        ..unsafe { std::mem::zeroed() }
    };
    bmi
}

/// 建一张 DIB，返回 (位图句柄, 像素指针)。`CreateDIBSection` 给的是
/// 可直接写的内存，不用 `GetDIBits` 再拷一遍。
fn dib(px: i32, bits: u16) -> (HBITMAP, *mut c_void) {
    unsafe {
        let bmi = header(px, bits);
        let mut p: *mut c_void = ptr::null_mut();
        let h = CreateDIBSection(
            ptr::null_mut(),
            &bmi,
            DIB_RGB_COLORS,
            &mut p,
            ptr::null_mut(),
            0,
        );
        (h, p)
    }
}

/// 这个点落在放大镜里吗。
///
/// 图形 = 一个圆环（镜框）+ 一根 45° 的圆头粗线（手柄）。手柄起点选在
/// 圆环的环带内部，两者自然连成一体，不需要再做布尔合并。
fn inside(x: f32, y: f32, n: f32) -> bool {
    let (cx, cy) = (n * 0.40, n * 0.40);
    let r_out = n * 0.30;
    let r_in = n * 0.185;

    let d = ((x - cx) * (x - cx) + (y - cy) * (y - cy)).sqrt();
    if d <= r_out && d >= r_in {
        return true;
    }

    let ax = cx + n * 0.184;
    let ay = cy + n * 0.184;
    let bx = n * 0.87;
    let by = n * 0.87;
    seg_dist(x, y, ax, ay, bx, by) <= n * 0.072
}

fn seg_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (vx, vy) = (bx - ax, by - ay);
    let (wx, wy) = (px - ax, py - ay);
    let len2 = vx * vx + vy * vy;
    // 退化成一个点时别除零，直接算点距
    let t = if len2 <= f32::EPSILON {
        0.0
    } else {
        ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0)
    };
    let (dx, dy) = (wx - vx * t, wy - vy * t);
    (dx * dx + dy * dy).sqrt()
}

/// 鼠标消息 → 该干什么。单独拎出来是因为这是托盘唯一的逻辑分支，
/// 而它埋在 `unsafe` 消息过程里没法测。
pub fn click_action(msg: u32) -> Click {
    match msg {
        WM_LBUTTONUP | WM_LBUTTONDBLCLK => Click::Show,
        WM_RBUTTONUP => Click::Menu,
        _ => Click::Ignore,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Click {
    Show,
    Menu,
    Ignore,
}

#[cfg(test)]
mod tests {
    use super::*;

    // 32px 下的关键坐标：中心 (12.8, 12.8)，外径 9.6，内径 5.92
    const N: f32 = 32.0;

    #[test]
    fn 镜片中间是空的() {
        // 放大镜的"孔"必须透明，否则 16px 下就是一团实心色块，
        // 看不出是个放大镜
        assert!(!inside(12.8, 12.8, N));
        assert!(!inside(11.0, 11.0, N));
    }

    #[test]
    fn 环带和手柄是实心的() {
        // 正右方的环带（0°，离 45° 的手柄足够远）
        assert!(inside(12.8 + 7.7, 12.8, N), "环带正右方");
        // 手柄中段
        assert!(inside(26.0, 26.0, N), "手柄中段");
        // 手柄末端是圆头，端点本身也该实心
        assert!(inside(0.87 * N, 0.87 * N, N), "手柄末端");
    }

    #[test]
    fn 四角是透明的() {
        // 图标不能是个方块：托盘里方块底色会盖住任务栏
        for (x, y) in [(0.5, 0.5), (31.5, 0.5), (0.5, 31.5), (31.5, 31.5)] {
            assert!(!inside(x, y, N), "角 ({x},{y}) 应该透明");
        }
    }

    #[test]
    fn 像素是_bgra_并且真带透明度() {
        let px = N as i32;
        let mut buf = vec![0u8; (px * px * 4) as usize];
        paint_pixels(&mut buf, px);
        let at = |x: i32, y: i32| {
            let i = ((y * px + x) * 4) as usize;
            [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
        };

        // 环带内部（20,12）：完全不透明，且内存顺序是 B,G,R
        let p = at(20, 12);
        assert_eq!([p[0], p[1], p[2]], [INK.2, INK.1, INK.0], "通道顺序应为 BGRA");
        assert_eq!(p[3], 255, "环带内部应当完全不透明");

        // 左上角：全透明。图案要是漏到这里，图标就成了带底色的方块
        assert_eq!(at(0, 0)[3], 0, "角落必须完全透明");
    }

    #[test]
    fn 左键唤出右键弹菜单() {
        assert_eq!(click_action(WM_LBUTTONUP), Click::Show);
        assert_eq!(click_action(WM_LBUTTONDBLCLK), Click::Show);
        assert_eq!(click_action(WM_RBUTTONUP), Click::Menu);
        // 鼠标移进移出（WM_MOUSEMOVE / 鼠标离开）不能触发任何动作，
        // 否则鼠标划过托盘就会弹窗
        assert_eq!(click_action(0x0200), Click::Ignore);
        assert_eq!(click_action(0), Click::Ignore);
    }
}
