//! 窗口创建、消息循环、子控件布局与自绘。
//!
//! 这里所有 `unsafe` 都是同一类：调 Win32。约定是**每个 Win32 调用前
//! 都有注释说明为什么这么调**，因为 Win32 的坑几乎都在"参数看着对但语义不对"上。

use std::ffi::c_void;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, UpdateWindow, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetTextExtentPoint32W, SelectObject, SetBkColor, SetBkMode, SetTextColor, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    FF_DONTCARE, FW_NORMAL, HBRUSH, HDC, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::Controls::{DRAWITEMSTRUCT, EM_SETSEL, MEASUREITEMSTRUCT, ODS_SELECTED};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, VK_CONTROL,
    VK_DOWN, VK_ESCAPE, VK_NEXT, VK_PRIOR, VK_RETURN, VK_SHIFT, VK_UP,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW,
    FindWindowW, GetSystemMetrics, LoadCursorW, MoveWindow, PostQuitMessage,
    RegisterClassW, SendMessageTimeoutW, SendMessageW, SMTO_ABORTIFHUNG,
    SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, TranslateMessage,
    KillTimer, CS_HREDRAW, CS_VREDRAW, EN_CHANGE, GWLP_USERDATA, IDC_ARROW, LBN_DBLCLK, LBN_SELCHANGE, LBS_NOTIFY, LBS_NOINTEGRALHEIGHT,
    LBS_OWNERDRAWFIXED, LB_ADDSTRING, LB_GETCURSEL, LB_RESETCONTENT, LB_SETCURSEL,
    LB_SETITEMHEIGHT, LB_SETTOPINDEX, MSG, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_SHOW,
    SW_SHOWNORMAL, WM_CLOSE, WM_COMMAND, WM_CTLCOLOREDIT, WM_DESTROY,
    WM_DRAWITEM, WM_ERASEBKGND, WM_HOTKEY, WM_KEYDOWN, WM_MEASUREITEM, WM_NCDESTROY, WM_PAINT,
    WM_SIZE, WM_TIMER, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_VISIBLE, WS_VSCROLL, HWND_TOPMOST, SWP_NOACTIVATE,
};

use crate::query::{Engine, Row, Via};
use crate::theme as t;
use crate::win::{wide, NO};
use crate::Options;

const CLS: &str = "hub_search_window_v1";
const ID_EDIT: usize = 1001;
const ID_LIST: usize = 1002;
const TIMER_SEARCH: usize = 1;
const HOTKEY_ID: i32 = 0x0B0B;

/// 自定义消息：把窗口叫出来。**不能复用 WM_HOTKEY** —— 热键是"切换"，
/// 而 `hub ui` 第二次执行的意思是"我要用"，用户按下命令时窗口可能已经是
/// 显示的，用 toggle 会把它藏起来，看起来像命令没生效。
const WM_HUB_SHOW: u32 = 0x8000 + 1; // WM_APP + 1

/// 敲字到真正查询之间等多久。太短会每个字符查一次（列表一直闪），
/// 太长会有"打了字没反应"的滞后感。110ms 大约是人停止打字的间隔。
const DEBOUNCE_MS: u32 = 110;

/// 一次最多取几条。窗口高度大约放得下 8 条，取多了纯属浪费带宽。
const LIMIT: usize = 40;

/// `SetBkMode(hdc, TRANSPARENT)`。windows-sys 把这个常量生成在
/// `BACKGROUND_MODE` 里但没导出名字，直接用值（Win32 定义 TRANSPARENT = 1）。
const TRANSPARENT_MODE: i32 = 1;
/// `CF_UNICODETEXT`。同理，它在 `Win32_System_Ole` 里，为了一个常量
/// 把整个 OLE feature 拉起来不划算。
const CF_UNICODE: u32 = 13;
/// `EN_UPDATE`。编辑框内容变化前发一次，比 EN_CHANGE 更早。
const EN_UPDATE: usize = 0x0400;
/// `VK_C`。虚拟键码就是 ASCII 大写字，windows-sys 没为字母单独定义常量。
const VK_C: u16 = b'C' as u16;
/// `WM_CTLCOLORLISTBOX`。windows-sys 0.61 没导出这个名字（0x0134）。
const WM_CTLCOLORLISTBOX: u32 = 0x0134;

pub struct App {
    hwnd: HWND,
    edit: HWND,
    list: HWND,
    engine: Engine,
    rows: Vec<Row>,
    sel: usize,
    scale: f32,
    font_input: HFONT,
    font_title: HFONT,
    font_sub: HFONT,
    br_bg: HBRUSH,
    br_sel: HBRUSH,
    br_line: HBRUSH,
    br_accent: HBRUSH,
    hotkey: Option<(u32, u32, String)>,
    num_docs: usize,
    via: Via,
    elapsed: f32,
    note: String,
    /// 上一次真正执行的查询。用来区分"还没输入"和"输入了但没命中" ——
    /// 这两种情况中间的提示语完全不同
    last_query: String,
}

impl Drop for App {
    fn drop(&mut self) {
        unsafe {
            for h in [self.font_input, self.font_title, self.font_sub] {
                if !h.is_null() {
                    DeleteObject(h as *mut c_void);
                }
            }
            for b in [self.br_bg, self.br_sel, self.br_line, self.br_accent] {
                if !b.is_null() {
                    DeleteObject(b as *mut c_void);
                }
            }
        }
    }
}

/// 窗口是否已经存在（不管可不可见）。
///
/// 给 `hub ui --detach` 用：脱离启动以后父进程还活着、但子进程可能自己退出了
/// （端口/热键/DPI 各种失败都发生在子进程里，父进程看不到）。只报"已启动"
/// 等于骗用户，所以父进程等一小会儿再问这一句。
pub fn window_exists() -> bool {
    unsafe {
        let cls = wide(CLS);
        !FindWindowW(cls.as_ptr(), ptr::null()).is_null()
    }
}

pub fn run(data: &Path, opts: Options) -> Result<(), String> {
    unsafe {
        // Per-monitor v2：Win10 1703+。取不到就算了，进程按系统 DPI 走，
        // 只是跨屏拖动时可能模糊一下。
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // 单实例：已经有一个在跑就把它唤出来。
        // 第二个实例不只是多余 —— 它会注册同一个全局热键失败，然后
        // 用户按快捷键时唤出的是"另一个窗口"，看起来像随机失灵。
        //
        // 用 SendMessageTimeout 而不是 PostMessage：PostMessage 只要句柄
        // 有效就返回成功，**哪怕对端进程已经卡死**。那样用户会永远启动不了
        // 新窗口，只能去任务管理器杀残留进程。带超时的同步消息能分辨这两种情况。
        {
            let cls = wide(CLS);
            let existing = FindWindowW(cls.as_ptr(), ptr::null());
            if !existing.is_null() {
                let mut out: usize = 0;
                let replied = SendMessageTimeoutW(
                    existing,
                    WM_HUB_SHOW,
                    0,
                    0,
                    SMTO_ABORTIFHUNG,
                    800,
                    &mut out,
                );
                if replied != 0 {
                    SetForegroundWindow(existing);
                    return Ok(());
                }
                // 没回应：旧进程卡死或正在退出。往下走，自己开一个。
                // 窗口类是进程私有的，同名注册不冲突；FindWindow 会优先
                // 返回先创建的那个，所以这里必须往后走而不是重复唤出。
            }
        }

        let hinst = GetModuleHandleW(ptr::null());
        if hinst.is_null() {
            return Err("GetModuleHandle 失败".into());
        }

        let class = wide(CLS);
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(main_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: NO as _,
            // 光标只在外框上用；Edit/ListBox 会自己设自己的
            hCursor: LoadCursorW(NO as _, IDC_ARROW),
            // 自己擦背景：系统刷子会是灰色，闪一下很难看
            hbrBackground: NO as _,
            lpszMenuName: ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            return Err(format!(
                "注册窗口类失败：{}",
                std::io::Error::last_os_error()
            ));
        }

        let scale = {
            let dpi = GetDpiForSystem();
            if dpi == 0 {
                1.0
            } else {
                dpi as f32 / 96.0
            }
        };

        let (x, y, w, h) = geometry(scale);
        let title = wide("Hub 搜索");

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_CLIPCHILDREN,
            x,
            y,
            w,
            h,
            NO as _,
            NO as _,
            hinst,
            ptr::null(),
        );
        if hwnd.is_null() {
            return Err(format!(
                "创建窗口失败：{}",
                std::io::Error::last_os_error()
            ));
        }

        // `into_raw`：所有权交给窗口，由 WM_NCDESTROY 收回。
        // 之前写 `Box::new` 再 `&mut *app`，run() 返回时 Box drop 一次、
        // NCDESTROY 又 drop 一次 —— 双重释放，退出时必崩。
        let app_ptr = Box::into_raw(Box::new(App {
            hwnd,
            edit: NO as _,
            list: NO as _,
            engine: Engine::new(data),
            rows: Vec::new(),
            sel: 0,
            scale,
            font_input: NO as _,
            font_title: NO as _,
            font_sub: NO as _,
            br_bg: CreateSolidBrush(t::BG),
            br_sel: CreateSolidBrush(t::SEL_BG),
            br_line: CreateSolidBrush(t::LINE),
            br_accent: CreateSolidBrush(t::ACCENT),
            hotkey: None,
            num_docs: 0,
            via: Via::Local,
            elapsed: 0.0,
            note: String::new(),
            last_query: String::new(),
        }));
        let app = &mut *app_ptr;
        app.build_children(hinst)?;
        crate::win::put_state(hwnd, app_ptr as *mut App);

        // 圆角是 Win11 才有，失败说明系统更老 —— 方角也不难看
        let round: i32 = 2; // DWMWCP_ROUND
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &round as *const i32 as *const c_void,
            std::mem::size_of::<i32>() as u32,
        );

        app.register_hotkey(opts.hotkey.as_deref());
        app.layout();
        app.run_query();

        if !opts.hidden {
            app.present();
        }
        UpdateWindow(hwnd);

        let mut msg: MSG = Default::default();
        // GetMessage 返回 0 是 WM_QUIT，-1 是错误；两者都该退出
        while GetMessageW(&mut msg, NO as _, 0, 0) > 0 {
            // 按键在消息循环里拦，**不**子类化输入框。
            //
            // 子类化看着更"正统"，但这里踩过一个坑：`SetWindowLongPtrW` 换掉
            // Edit 的窗口过程后，那段代码要能拿到 App 才能把消息转回原过程；
            // 而 Edit 是在 App 指针写进父窗口 USERDATA **之前**创建的，
            // 于是那段时间 Edit 的所有消息都落到 DefWindowProc —— 结果就是
            // 输入框能存文本却画不出来、也从不发 EN_CHANGE。
            // 在消息循环里拦，Edit 的窗口过程一行不动，行为 100% 原生。
            if msg.message == WM_KEYDOWN {
                if let Some(app) = crate::win::state::<App>(msg.hwnd) {
                    if app.handle_key(msg.wParam as u16) {
                        continue;
                    }
                } else if let Some(app) = crate::win::state::<App>(hwnd) {
                    // 焦点在输入框上，句柄属于子窗口 —— 用主窗口的 state
                    if msg.hwnd == app.edit && app.handle_key(msg.wParam as u16) {
                        continue;
                    }
                }
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}

fn geometry(scale: f32) -> (i32, i32, i32, i32) {
    unsafe {
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let w = (t::WIN_W * scale).round() as i32;
        let h = (t::WIN_H * scale).round() as i32;
        let x = (sw - w) / 2;
        // 放在偏上而不是正中：搜索结果往下长，视线起点在屏幕上部更顺手
        let y = (sh / 6).min((sh - h - 40).max(0));
        (x, y, w, h)
    }
}

impl App {
    fn build_children(&mut self, hinst: *mut c_void) -> Result<(), String> {
        unsafe {
            let s = self.scale;
            self.font_input = make_font(t::FONT_INPUT * s, FW_NORMAL as i32);
            self.font_title = make_font(t::FONT_TITLE * s, FW_NORMAL as i32);
            self.font_sub = make_font(t::FONT_SUB * s, FW_NORMAL as i32);

            let c_edit = wide("EDIT");
            let c_list = wide("LISTBOX");
            let edit = CreateWindowExW(
                0,
                c_edit.as_ptr(),
                ptr::null(),
                // 无 WS_BORDER：边框由父窗口画，这样能控制颜色
                WS_CHILD | WS_VISIBLE | 0x0080 /* ES_AUTOHSCROLL */ | 0x0000,
                0,
                0,
                10,
                10,
                self.hwnd,
                ID_EDIT as _,
                hinst,
                ptr::null(),
            );
            if edit.is_null() {
                return Err("创建输入框失败".into());
            }
            SendMessageW(edit, 0x0030 /* WM_SETFONT */, self.font_input as WPARAM, 1);
            // 左右各留一点内边距，否则光标贴着窗口边缘
            let pad = (t::PAD * s) as i32;
            SendMessageW(
                edit,
                0x00D3, /* EM_SETMARGINS */
                0x0003, /* EC_LEFTMARGIN | EC_RIGHTMARGIN */
                ((pad as isize) << 16) | pad as isize,
            );

            let list = CreateWindowExW(
                0,
                c_list.as_ptr(),
                ptr::null(),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_VSCROLL
                    | (LBS_NOTIFY as u32)
                    | (LBS_OWNERDRAWFIXED as u32)
                    | (LBS_NOINTEGRALHEIGHT as u32),
                0,
                0,
                10,
                10,
                self.hwnd,
                ID_LIST as _,
                hinst,
                ptr::null(),
            );
            if list.is_null() {
                return Err("创建结果列表失败".into());
            }
            SendMessageW(list, 0x0030, self.font_title as WPARAM, 1);

            self.edit = edit;
            self.list = list;
            Ok(())
        }
    }

    fn register_hotkey(&mut self, spec: Option<&str>) {
        let candidates: Vec<(u32, u32, String)> = match spec.and_then(parse_hotkey) {
            Some(h) => vec![h],
            // 默认 Ctrl+Alt+Space。被占就退到 Ctrl+Alt+K ——
            // 一个热键注册失败不该让整个界面不可用，窗口还在，手动点开也能用
            None => vec![
                (MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, ' ' as u32, "Ctrl+Alt+Space".into()),
                (MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'K' as u32, "Ctrl+Alt+K".into()),
            ],
        };
        unsafe {
            for (mods, vk, label) in candidates {
                if RegisterHotKey(self.hwnd, HOTKEY_ID, mods, vk) != 0 {
                    self.hotkey = Some((mods, vk, label));
                    return;
                }
            }
        }
        self.note = "全局热键被占用".into();
    }

    fn layout(&self) {
        unsafe {
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(self.hwnd, &mut rc);
            let s = self.scale;
            let pad = (t::PAD * s) as i32;
            let input_h = (t::INPUT_H * s) as i32;
            let status_h = (t::STATUS_H * s) as i32;
            let w = rc.right;
            let h = rc.bottom;

            MoveWindow(self.edit, pad, pad, w - pad * 2, input_h, 1);

            let gap = (8.0 * s) as i32;
            let top = pad + input_h + gap;
            let list_h = (h - top - status_h).max(0);
            MoveWindow(self.list, pad, top, w - pad * 2, list_h, 1);


            // owner-draw 的行高不会因为窗口变大自动重算，
            // 但 DPI 变了（拖动到另一块屏）就得重设
            let row_h = (t::ROW_H * s) as isize;
            SendMessageW(self.list, LB_SETITEMHEIGHT, 0, row_h);
        }
    }

    /// 处理输入框里的特殊键。返回 true 表示"已消费，别再发给 Edit"。
    ///
    /// 只认方向键 / 回车 / Esc：其余键（包括所有 IME 消息）必须原样交给
    /// 输入框自己，否则中文输入法就没法用了。
    fn handle_key(&mut self, vk: u16) -> bool {
        // 修饰键状态要现问系统：WM_KEYDOWN 的 lParam 只带扫描码，
        // 不带 Ctrl/Shift，所以只能 GetKeyState
        let ctrl = unsafe { GetKeyState(VK_CONTROL as i32) } < 0;
        let shift = unsafe { GetKeyState(VK_SHIFT as i32) } < 0;

        match vk {
            k if k == VK_RETURN && ctrl => {
                self.reveal_selected();
                true
            }
            // Ctrl+Shift+C 而不是 Ctrl+C：后者在输入框里是"复制我打的字"，
            // 抢过来会让用户没法复制查询词
            k if k == VK_C && ctrl && shift => {
                let text = self
                    .rows
                    .get(self.sel)
                    .map(|r| match &r.path {
                        Some(p) => p.to_string_lossy().to_string(),
                        None => r.title.clone(),
                    })
                    .unwrap_or_default();
                if !text.is_empty() {
                    self.copy(&text);
                }
                true
            }
            k if k == VK_DOWN => {
                self.move_sel(1);
                true
            }
            k if k == VK_UP => {
                self.move_sel(-1);
                true
            }
            k if k == VK_NEXT => {
                self.move_sel(8);
                true
            }
            k if k == VK_PRIOR => {
                self.move_sel(-8);
                true
            }
            k if k == VK_RETURN => {
                self.open_selected();
                true
            }
            k if k == VK_ESCAPE => {
                self.hide();
                true
            }
            _ => false,
        }
    }

    fn run_query(&mut self) {
        let q = crate::win::get_text(self.edit);
        self.last_query = q.trim().to_string();
        trace(format!("run_query q={q:?} edit={:?}", self.edit));
        match self.engine.search(&q, LIMIT) {
            Ok(rep) => {
                self.rows = rep.rows;
                self.num_docs = rep.num_docs;
                self.via = rep.via;
                self.elapsed = rep.elapsed_ms;
                self.note.clear();
            }
            Err(e) => {
                self.rows.clear();
                self.note = e;
            }
        }
        self.sel = 0;
        self.refresh_list();
        self.repaint();
    }

    fn refresh_list(&self) {
        unsafe {
            SendMessageW(self.list, LB_RESETCONTENT, 0, 0);
            let empty = wide("");
            for _ in &self.rows {
                SendMessageW(self.list, LB_ADDSTRING, 0, empty.as_ptr() as isize);
            }
            SendMessageW(self.list, LB_SETCURSEL, self.sel as WPARAM, 0);
            self.scroll_to_sel();
            // 空列表就把控件藏起来：ListBox 是不透明的子窗口，
            // 留着它父窗口就没法在中间画"没有匹配"这类提示
            ShowWindow(
                self.list,
                if self.rows.is_empty() { SW_HIDE } else { SW_SHOW },
            );
        }
    }

    fn scroll_to_sel(&self) {
        unsafe {
            // 让选中项落在可见区中间附近，比"只保证可见"更好用：
            // 连续按方向键时列表不会一跳一跳
            let page = 6usize;
            let top = self.sel.saturating_sub(page / 2);
            SendMessageW(self.list, LB_SETTOPINDEX, top as WPARAM, 0);
        }
    }

    fn move_sel(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as i32 - 1;
        let next = (self.sel as i32 + delta).clamp(0, last);
        if next as usize == self.sel {
            return;
        }
        self.sel = next as usize;
        unsafe {
            SendMessageW(self.list, LB_SETCURSEL, self.sel as WPARAM, 0);
        }
        self.scroll_to_sel();
    }

    fn open_selected(&mut self) {
        let Some(row) = self.rows.get(self.sel) else {
            return;
        };
        match &row.path {
            Some(p) => unsafe {
                let op = wide("open");
                let file = wide(&p.to_string_lossy());
                let ret = ShellExecuteW(
                    self.hwnd,
                    op.as_ptr(),
                    file.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    SW_SHOWNORMAL,
                );
                // ShellExecute 的返回值 <= 32 才是错误码，这是它独有的约定
                if ret as isize <= 32 {
                    self.note = format!("打开失败（{}）", ret as isize);
                    self.repaint();
                }
            },
            // 终端历史没有文件可开，退一步把命令复制走
            None => {
                let text = row.title.clone();
                self.copy(&text);
            }
        }
    }

    /// 在资源管理器里定位（相当于"打开所在文件夹并选中"）。
    /// 终端历史没有文件，就把命令复制走。
    fn reveal_selected(&mut self) {
        let Some(row) = self.rows.get(self.sel) else {
            return;
        };
        match &row.path {
            Some(p) => unsafe {
                let exe = wide("explorer.exe");
                let arg = wide(&format!("/select,\"{}\"", p.to_string_lossy()));
                ShellExecuteW(
                    self.hwnd,
                    ptr::null(),
                    exe.as_ptr(),
                    arg.as_ptr(),
                    ptr::null(),
                    SW_SHOWNORMAL,
                );
            },
            None => {
                let text = row.title.clone();
                self.copy(&text);
            }
        }
    }

    fn copy(&mut self, s: &str) {
        if clipboard_set(self.hwnd, s) {
            self.note = "已复制".into();
        } else {
            self.note = "复制失败（剪贴板被占用）".into();
        }
        self.repaint();
    }

    /// 状态文案。拼一次给重绘用；分离出来是因为它同时决定
    /// "列表为空时中间显示什么"，两处必须一致。
    fn status_line(&self) -> String {
        if !self.note.is_empty() && self.rows.is_empty() {
            return self.note.clone();
        }
        if self.rows.is_empty() {
            if self.num_docs == 0 {
                "索引是空的 · 先运行 hub daemon start <目录>".to_string()
            } else {
                format!(
                    "索引 {} 篇 · 输入关键词开始搜索（支持「上个月」「代码片段」-排除）",
                    self.num_docs
                )
            }
        } else {
            format!(
                "{} 条 · {:.1} ms · {} · 索引 {} 篇",
                self.rows.len(),
                self.elapsed,
                self.via.label(),
                self.num_docs
            )
        }
    }

    fn repaint(&self) {
        unsafe {
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(self.hwnd, &mut rc);
            windows_sys::Win32::Graphics::Gdi::InvalidateRect(self.hwnd, ptr::null(), 0);
            let _ = rc;
        }
    }

    fn present(&mut self) {
        unsafe {
            // 每次唤出重新算位置：用户可能拔了外接屏、改了分辨率
            let (x, y, w, h) = geometry(self.scale);
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE,
            );
            ShowWindow(self.hwnd, SW_SHOW);
            SetForegroundWindow(self.hwnd);
            windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(self.edit);
            // 全选：接着输入就替换，想改就按一下左/右
            SendMessageW(self.edit, EM_SETSEL as u32, 0, -1);
        }
        self.repaint();
    }

    fn hide(&self) {
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn toggle(&mut self) {
        unsafe {
            if windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(self.hwnd) != 0 {
                self.hide();
            } else {
                self.present();
            }
        }
    }
}

fn make_font(height: f32, weight: i32) -> HFONT {
    unsafe {
        let face = wide(t::FACE);
        CreateFontW(
            -(height.round() as i32), // 负 = 字符高度，正 = 单元格高度。用负值才对
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            FF_DONTCARE as u32,
            face.as_ptr(),
        )
    }
}

/// `ctrl+alt+space` 这种写法 → (MOD_*, VK)。
fn parse_hotkey(spec: &str) -> Option<(u32, u32, String)> {
    let mut mods = MOD_NOREPEAT;
    let mut vk = None;
    let mut label: Vec<String> = Vec::new();
    for part in spec.split('+').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => {
                mods |= MOD_CONTROL;
                label.push("Ctrl".into());
            }
            "alt" => {
                mods |= MOD_ALT;
                label.push("Alt".into());
            }
            "shift" => {
                mods |= windows_sys::Win32::UI::Input::KeyboardAndMouse::MOD_SHIFT;
                label.push("Shift".into());
            }
            "win" => {
                mods |= windows_sys::Win32::UI::Input::KeyboardAndMouse::MOD_WIN;
                label.push("Win".into());
            }
            key => {
                let c = key.chars().next()?;
                let code = match key {
                    "space" => ' ' as u32,
                    "enter" => VK_RETURN as u32,
                    _ if key.len() == 1 && c.is_ascii_alphanumeric() => c.to_ascii_uppercase() as u32,
                    _ => return None,
                };
                label.push(key.to_ascii_uppercase());
                vk = Some(code);
            }
        }
    }
    // 没有修饰键的热键会抢走全系统的这个键，不做
    if mods & (MOD_CONTROL | MOD_ALT | windows_sys::Win32::UI::Input::KeyboardAndMouse::MOD_SHIFT)
        == 0
    {
        return None;
    }
    Some((mods, vk?, label.join("+")))
}

fn clipboard_set(hwnd: HWND, s: &str) -> bool {
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return false;
        }
        EmptyClipboard();
        let w = wide(s);
        let bytes = w.len() * 2;
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if h.is_null() {
            CloseClipboard();
            return false;
        }
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            CloseClipboard();
            return false;
        }
        ptr::copy_nonoverlapping(w.as_ptr() as *const u8, p, bytes);
        GlobalUnlock(h);
        // 成功后内存所有权归系统，不能再 GlobalFree
        if SetClipboardData(CF_UNICODE, h).is_null() {
            CloseClipboard();
            return false;
        }
        CloseClipboard();
        true
    }
}

// ============================================================
// 窗口过程
// ============================================================

unsafe extern "system" fn main_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    let app = crate::win::state::<App>(hwnd);

    match msg {
        WM_ERASEBKGND => 1, // 背景自己画，别让系统刷一遍灰

        WM_PAINT => {
            if let Some(app) = app {
                paint(app);
                0
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }

        WM_SIZE => {
            if let Some(app) = app {
                app.layout();
            }
            0
        }

        // 输入框和状态栏的背景色：不接管就会是系统灰（比窗口白底暗一档）
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            if let Some(app) = app {
                let hdc = wp as HDC;
                SetBkColor(hdc, t::BG);
                SetTextColor(hdc, t::FG);
                app.br_bg as LRESULT
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }

        WM_MEASUREITEM => {
            if let Some(app) = app {
                let mis = lp as *mut MEASUREITEMSTRUCT;
                if !mis.is_null() {
                    (*mis).itemHeight = (t::ROW_H * app.scale).round() as u32;
                    return 1;
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }

        WM_DRAWITEM => {
            if let Some(app) = app {
                let dis = lp as *const DRAWITEMSTRUCT;
                if !dis.is_null() {
                    draw_row(app, &*dis);
                    return 1;
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }

        WM_COMMAND => {
            trace(format!(
                "WM_COMMAND id={} code={:#x}",
                wp & 0xFFFF,
                (wp >> 16) & 0xFFFF
            ));
            if let Some(app) = app {
                let id = wp & 0xFFFF;
                let code = (wp >> 16) & 0xFFFF;
                // EN_CHANGE（0x0300）是"内容变了"，EN_UPDATE（0x0400）是"马上要重画"。
                // 用户手打时两个都来；但程序化 SetWindowTextW **只发后者**，
                // 只认 EN_CHANGE 会让脚本 / 输入法上屏 / 粘贴这些路径全部失效。
                if id == ID_EDIT && (code == EN_CHANGE as usize || code == EN_UPDATE) {
                    // 每次变更重置定时器：连续打字时只在停手后查一次
                    let got = SetTimer(hwnd, TIMER_SEARCH, DEBOUNCE_MS, None);
                    trace(format!("SetTimer -> {got}"));
                    return 0;
                }
                if id == ID_LIST {
                    if code == LBN_DBLCLK as usize {
                        app.open_selected();
                        return 0;
                    }
                    if code == LBN_SELCHANGE as usize {
                        let cur = SendMessageW(app.list, LB_GETCURSEL, 0, 0);
                        if cur >= 0 {
                            app.sel = cur as usize;
                        }
                        return 0;
                    }
                }
            }
            0
        }

        WM_TIMER => {
            trace(format!("WM_TIMER id={wp}"));
            if wp == TIMER_SEARCH {
                KillTimer(hwnd, TIMER_SEARCH);
                if let Some(app) = app {
                    app.run_query();
                }
            }
            0
        }

        WM_HOTKEY => {
            if let Some(app) = app {
                app.toggle();
            }
            0
        }

        WM_HUB_SHOW => {
            if let Some(app) = app {
                app.present();
            }
            0
        }

        WM_CLOSE => {
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            0
        }

        WM_DESTROY => {
            if let Some(app) = app {
                if let Some(_) = app.hotkey {
                    UnregisterHotKey(hwnd, HOTKEY_ID);
                }
            }
            PostQuitMessage(0);
            0
        }

        // 窗口已销毁，把 Box 收回来。**必须在 NCDESTROY 而不是 DESTROY**：
        // DESTROY 之后子窗口还在销毁过程中，仍可能有消息回调到这里。
        WM_NCDESTROY => {
            let p = windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
                hwnd,
                GWLP_USERDATA,
            ) as *mut App;
            windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                hwnd,
                GWLP_USERDATA,
                0,
            );
            let r = DefWindowProcW(hwnd, msg, wp, lp);
            if !p.is_null() {
                drop(Box::from_raw(p));
            }
            return r;
        }

        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 调试探针：设了 `HUB_UI_TRACE` 才写。平时只在消息处理里多一次 getenv。
fn trace(line: impl AsRef<str>) {
    if std::env::var_os("HUB_UI_TRACE").is_none() {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        // 写到临时目录而不是工程目录：万一忘了关，也不会把调试产物留在仓库里
        .open(std::env::temp_dir().join("hub-ui-trace.txt"))
    {
        let _ = writeln!(f, "{}", line.as_ref());
    }
}

fn paint(app: &App) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(app.hwnd, &mut ps);
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(app.hwnd, &mut rc);

        FillRect(hdc, &rc, app.br_bg);
        SetBkMode(hdc, TRANSPARENT_MODE);

        let s = app.scale;
        let pad = (t::PAD * s) as i32;
        let input_h = (t::INPUT_H * s) as i32;
        let status_h = (t::STATUS_H * s) as i32;

        // ---- 外框 ----
        // 无边框窗口在浅色桌面上就是一块白，边界完全看不出来。
        // 四条 1px 线比 WS_BORDER 更好控制颜色，也不影响圆角。
        let one = (1.0 * s).max(1.0) as i32;
        for r in [
            RECT { left: 0, top: 0, right: rc.right, bottom: one },
            RECT { left: 0, top: rc.bottom - one, right: rc.right, bottom: rc.bottom },
            RECT { left: 0, top: 0, right: one, bottom: rc.bottom },
            RECT { left: rc.right - one, top: 0, right: rc.right, bottom: rc.bottom },
        ] {
            FillRect(hdc, &r, app.br_line);
        }

        // ---- 输入框下的分隔线 ----
        let y1 = pad + input_h + (6.0 * s) as i32;
        FillRect(
            hdc,
            &RECT { left: pad, top: y1, right: rc.right - pad, bottom: y1 + one },
            app.br_line,
        );

        // ---- 状态栏 ----
        let status_top = rc.bottom - status_h;
        FillRect(
            hdc,
            &RECT { left: 0, top: status_top - one, right: rc.right, bottom: status_top },
            app.br_line,
        );

        let old = SelectObject(hdc, app.font_sub as _);

        // 左边：规模 / 命中 / 耗时
        let line = wide(&app.status_line());
        SetTextColor(hdc, if app.rows.is_empty() { t::SUB } else { t::FG });
        let mut left = RECT {
            left: pad,
            top: status_top,
            right: rc.right * 3 / 5,
            bottom: rc.bottom,
        };
        DrawTextW(
            hdc,
            line.as_ptr(),
            -1,
            &mut left,
            DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        // 右边：固定的快捷键说明。它不随状态变化，所以颜色一直很淡
        let hint = wide("↑↓ 选择 · Enter 打开 · Ctrl+Enter 定位 · Esc 收起");
        SetTextColor(hdc, t::HINT);
        let mut right = RECT {
            left: rc.right * 3 / 5,
            top: status_top,
            right: rc.right - pad,
            bottom: rc.bottom,
        };
        DrawTextW(
            hdc,
            hint.as_ptr(),
            -1,
            &mut right,
            DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_RIGHT | DT_END_ELLIPSIS,
        );

        // ---- 列表为空时的中间提示 ----
        // 列表控件这时是隐藏的（见 refresh_list），这里才画得出来
        if app.rows.is_empty() {
            let (msg, sub) = if app.num_docs == 0 {
                (
                    "索引还是空的",
                    "先运行 hub daemon start <你的目录>，之后改动会自动进索引",
                )
            } else if app.last_query.is_empty() {
                (
                    "输入关键词开始搜索",
                    "支持「上个月」「代码片段」\"精确短语\" -排除词，例如：上个月的 rust 内存笔记",
                )
            } else {
                (
                    "没有匹配的内容",
                    "换个说法，或去掉时间 / 来源约束试试",
                )
            };
            let list_top = y1 + one;
            let mid = (list_top + status_top) / 2;
            let mut r1 = RECT {
                left: pad,
                top: mid - (20.0 * s) as i32,
                right: rc.right - pad,
                bottom: mid + (4.0 * s) as i32,
            };
            let mut r2 = RECT {
                left: pad,
                top: mid + (6.0 * s) as i32,
                right: rc.right - pad,
                bottom: mid + (30.0 * s) as i32,
            };

            let big = wide(msg);
            let old2 = SelectObject(hdc, app.font_title as _);
            SetTextColor(hdc, t::SUB);
            DrawTextW(
                hdc,
                big.as_ptr(),
                -1,
                &mut r1,
                DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX,
            );
            SelectObject(hdc, old2);

            let small = wide(sub);
            SetTextColor(hdc, t::HINT);
            DrawTextW(
                hdc,
                small.as_ptr(),
                -1,
                &mut r2,
                DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        }

        SelectObject(hdc, old);
        EndPaint(app.hwnd, &ps);
    }
}

fn draw_row(app: &App, dis: &DRAWITEMSTRUCT) {
    unsafe {
        let idx = dis.itemID as usize;
        let Some(row) = app.rows.get(idx) else {
            return;
        };
        let hdc = dis.hDC;
        let selected = dis.itemState & ODS_SELECTED != 0;
        let s = app.scale;
        let pad = (t::PAD * s) as i32;

        FillRect(hdc, &dis.rcItem, if selected { app.br_sel } else { app.br_bg });

        // 选中行左侧一条 3px 竖条。只靠淡紫底色区分选中，在一屏白底里
        // 太弱了 —— 用户按方向键时看不出焦点在哪
        if selected {
            FillRect(
                hdc,
                &RECT {
                    left: dis.rcItem.left,
                    top: dis.rcItem.top,
                    right: dis.rcItem.left + (3.0 * s).max(2.0) as i32,
                    bottom: dis.rcItem.bottom,
                },
                app.br_accent,
            );
        }

        SetBkMode(hdc, TRANSPARENT_MODE);

        let mut rc = dis.rcItem;
        rc.left += pad;
        rc.right -= pad;

        // 两行文字整体在行内垂直居中：先算内容总高，再把剩余空间对半分。
        // 之前是"按行高的一半硬切"，结果标题贴顶、底部留一大片空白。
        let title_h = (20.0 * s) as i32;
        let gap = (3.0 * s) as i32;
        let sub_h = (17.0 * s) as i32;
        let total = title_h + gap + sub_h;
        let top = rc.top + ((rc.bottom - rc.top - total) / 2).max(0);

        let mut title_rc = RECT {
            left: rc.left,
            top,
            right: rc.right,
            bottom: top + title_h,
        };
        let mut sub_rc = RECT {
            left: rc.left,
            top: top + title_h + gap,
            right: rc.right,
            bottom: top + title_h + gap + sub_h,
        };

        let title = wide(&row.title);
        let old = SelectObject(hdc, app.font_title as _);
        SetTextColor(hdc, if selected { t::SEL_FG } else { t::FG });
        DrawTextW(
            hdc,
            title.as_ptr(),
            -1,
            &mut title_rc,
            DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS | DT_VCENTER,
        );
        SelectObject(hdc, old);

        let old = SelectObject(hdc, app.font_sub as _);
        SetTextColor(hdc, if selected { t::SEL_FG } else { t::SUB });
        // 副标题不交给 `DT_PATH_ELLIPSIS`。它的约定是"按反斜杠分段省略中间"，
        // 而终端历史的 uri 里一个反斜杠都没有（`terminal://C:/Users/...`），
        // 于是它只会把开头换成 "..."、剩下的照画 —— 直接画到窗口外面去。
        // 自己按像素宽度量一遍，正斜杠反斜杠都当分隔符。
        let fitted = {
            let maxw = (sub_rc.right - sub_rc.left).max(0);
            let f = fit_path(hdc, &row.subtitle, maxw);
            wide(&f)
        };
        DrawTextW(
            hdc,
            fitted.as_ptr(),
            -1,
            &mut sub_rc,
            DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS | DT_VCENTER,
        );
        SelectObject(hdc, old);
    }
}

/// 一段文本在当前字体下的像素宽。
unsafe fn text_px(hdc: HDC, s: &str) -> i32 {
    let mut sz: SIZE = std::mem::zeroed();
    let w = wide(s);
    let n = w.len() as i32 - 1; // 去掉结尾 NUL
    if GetTextExtentPoint32W(hdc, w.as_ptr(), n, &mut sz) == 0 {
        return 0;
    }
    sz.cx
}

/// 把长路径塞进 `maxw` 像素：保留开头（盘符 / 协议）与末尾几级，
/// 中间换成 `…`。
///
/// 先按段试，段数从多到少；连最后一段都放不下时按字符从后往前留。
/// 之所以要"从后往前留"：文件名末尾（尤其是扩展名）比开头有用。
unsafe fn fit_path(hdc: HDC, s: &str, maxw: i32) -> String {
    if maxw <= 0 || text_px(hdc, s) <= maxw {
        return s.to_string();
    }
    let parts: Vec<&str> = s
        .split(['\\', '/'])
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 3 {
        // 段太少（`cargo test   3 分钟前` 这种根本没有分隔符），
        // 没有"中间"可省，交给 END_ELLIPSIS
        return s.to_string();
    }
    let head = parts[0];
    for keep in (1..parts.len()).rev() {
        let tail = parts[parts.len() - keep..].join("\\");
        let cand = format!("{head}\\…\\{tail}");
        if text_px(hdc, &cand) <= maxw {
            return cand;
        }
    }
    let chars: Vec<char> = parts[parts.len() - 1].chars().collect();
    for keep in (1..chars.len()).rev() {
        let tail: String = chars[chars.len() - keep..].iter().collect();
        let cand = format!("{head}\\…\\{tail}");
        if text_px(hdc, &cand) <= maxw {
            return cand;
        }
    }
    "…".to_string()
}

/// 让 CLI 能用同一套热键解析校验参数。
pub fn validate_hotkey(spec: &str) -> Option<String> {
    parse_hotkey(spec).map(|(_, _, label)| label)
}
