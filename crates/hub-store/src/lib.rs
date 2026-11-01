//! hub-store —— 持久化层：SQLite 水位表 + Tantivy 倒排。
//!
//! 职责分层（这一条不能搞混）：
//! - **SQLite** = 事实源的水位表：content_hash / mtime / tombstone / doc_id 分配。
//!   它需要强事务与崩溃恢复。
//! - **Tantivy** = 可重建的索引缓存：倒排 / BM25 / 段合并。
//!   删了能从 SQLite + 原始数据源重建，所以它不是事实源。
//!
//! ⚠️ 与 hub-core 的关系（第一版最大的缺陷就在这里）：
//! `parse_query` 是整套语义的唯一入口，**索引层不许自己造一套解析**。
//! 第一版的 `HubIndex::search(&str)` 把原始字符串直接丢给 Tantivy 的 QueryParser，
//! 结果时间 / 来源 / 短语 / 排除四条语义在生产路径上一条都没生效，
//! hub-core 那 30 多条测试保护的是一个永远不会被执行的函数。
//! 现在 `HubIndex::search` 只接受 `&ParsedQuery`，并且评分常数（衰减、惩罚、短语加成）
//! 全部从 hub-core 导入，不在这里重定义。
//!
//! 分词也必须同源：`hub_word` / `hub_bigram` 两个自定义分词器直接调用
//! `hub_core::word_tokens` / `bigram_tokens`，索引与查询共用一份词表。

pub mod index;
pub mod indexer;
pub mod meta;

use std::fmt;

pub use index::{Hit, HubIndex, HUB_BIGRAM, HUB_WORD};
pub use indexer::{meta_path, Indexer, IndexerStats, META_FILE};
pub use meta::{MetaRow, MetaStore};

/// 存储层统一错误。混用 tantivy::Result 和 rusqlite::Result 会让上层写出一堆
/// `map_err(|e| e.to_string())`，所以这里收敛成一个枚举。
#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Tantivy(tantivy::TantivyError),
    /// 目录 / schema 这类需要人类可读解释的错误
    Message(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "io: {e}"),
            StoreError::Sql(e) => write!(f, "sqlite: {e}"),
            StoreError::Tantivy(e) => write!(f, "tantivy: {e}"),
            StoreError::Message(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Io(e) => Some(e),
            StoreError::Sql(e) => Some(e),
            StoreError::Tantivy(e) => Some(e),
            StoreError::Message(_) => None,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}

impl From<tantivy::TantivyError> for StoreError {
    fn from(e: tantivy::TantivyError) -> Self {
        StoreError::Tantivy(e)
    }
}

impl From<tantivy::directory::error::OpenDirectoryError> for StoreError {
    fn from(e: tantivy::directory::error::OpenDirectoryError) -> Self {
        StoreError::Message(format!("打开索引目录失败: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;
