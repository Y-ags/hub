//! 唤起式搜索窗。
//!
//! 用的是 **Win32 标准控件**，不是自绘引擎。这个选择不是图省事：
//!
//! 1. **中文输入法**。用户要用中文搜中文，而 IME 在自绘窗口上要自己接
//!    `WM_IME_COMPOSITION`、自己画候选窗、自己处理光标位置。Edit 控件这些全都有，
//!    且跟着系统输入法版本走。
//! 2. **零新依赖**。crates.io 不在手边的时候，egui 拉不下来；而 `windows-sys`
//!    本来就在依赖树里，UI 只是把它的 feature 打开。
//! 3. **滚动、选中、键盘导航、无障碍**都是现成的。自己实现一遍只会更差。
//!
//! 代价是外观受限于原生控件，自绘的部分只有结果列表（`LBS_OWNERDRAWFIXED`）。

#[cfg(windows)]
mod theme;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod wnd;

pub mod query;

pub use query::{Engine, Reply, Row, Via};

/// 校验 `--hotkey` 参数，返回规范化后的写法（`None` = 这个组合不能用）。
#[cfg(windows)]
pub use wnd::validate_hotkey;

/// 搜索窗是否已经存在（不管当前可见与否）。
#[cfg(windows)]
pub fn window_exists() -> bool {
    wnd::window_exists()
}

#[cfg(not(windows))]
pub fn window_exists() -> bool {
    false
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// 全局热键，形如 `ctrl+alt+space`。`None` = 用默认值
    pub hotkey: Option<String>,
    /// 启动后立刻显示（默认就显示；`--hidden` 时只注册热键、窗口藏在托盘外）
    pub hidden: bool,
}

/// 跑起来，直到窗口被销毁。**必须在主线程调用** —— Win32 消息循环是和
/// 创建窗口的线程绑定的，换个线程就得处理跨线程 PostMessage，没必要。
#[cfg(windows)]
pub fn run(data: &std::path::Path, opts: Options) -> Result<(), String> {
    wnd::run(data, opts)
}

#[cfg(not(windows))]
pub fn run(_data: &std::path::Path, _opts: Options) -> Result<(), String> {
    Err("界面目前只实现了 Windows 版本".to_string())
}
