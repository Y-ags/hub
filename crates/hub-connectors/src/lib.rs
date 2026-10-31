//! 采集层：把散落各处的原始数据压成 `hub_core::Doc`。
//!
//! 每个 connector 只做三件事：遍历、读出文本、给出 (uri, title, body, mtime)。
//! **不碰索引、不碰 watermark、不算 hash** —— 那是 hub-store 的职责。
//! 这条边界很重要：connector 可以随便重写，核心检索逻辑不受影响。

pub mod files;
pub mod terminal;

pub use files::{FileHit, WalkOptions, WalkStats, read_text, walk};
pub use terminal::{HistoryEntry, default_history_files, is_noise, parse_history};
