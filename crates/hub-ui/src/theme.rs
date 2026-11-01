//! 配色与字号。刻意只放常量，方便一眼看完整体视觉。

/// COLORREF 是 0x00BBGGRR，统一用 `rgb()` 从 R/G/B 构造，别手写十六进制。
pub const BG: u32 = crate::win::rgb(0xFF, 0xFF, 0xFF);
pub const FG: u32 = crate::win::rgb(0x1F, 0x29, 0x37);
pub const SUB: u32 = crate::win::rgb(0x6B, 0x72, 0x80);
pub const LINE: u32 = crate::win::rgb(0xE5, 0xE7, 0xEB);
/// 选中行：淡紫底，不是反白。反白在浅色窗口里太跳，而且第二行的灰字会糊掉
pub const SEL_BG: u32 = crate::win::rgb(0xEE, 0xF0, 0xFF);
pub const SEL_FG: u32 = crate::win::rgb(0x31, 0x2E, 0x81);
/// 选中行左侧的强调竖条
pub const ACCENT: u32 = crate::win::rgb(0x63, 0x66, 0xF1);
/// 输入框里"没结果"这类弱提示
pub const HINT: u32 = crate::win::rgb(0x9C, 0xA3, 0xAF);

/// 逻辑像素尺寸（96 DPI 下的值），实际使用前乘缩放系数。
pub const WIN_W: f32 = 760.0;
pub const WIN_H: f32 = 520.0;
pub const PAD: f32 = 18.0;
pub const INPUT_H: f32 = 44.0;
pub const ROW_H: f32 = 56.0;
pub const STATUS_H: f32 = 28.0;

/// 字号（像素高度，负数表示按字符高度算而不是单元格高度）
pub const FONT_INPUT: f32 = 17.0;
pub const FONT_TITLE: f32 = 14.0;
pub const FONT_SUB: f32 = 12.0;

/// 微软雅黑 UI 是 Win8 以后的中文界面标准字体；
/// 取不到时 CreateFontW 会回退到系统默认，不会花屏。
pub const FACE: &str = "Microsoft YaHei UI";
