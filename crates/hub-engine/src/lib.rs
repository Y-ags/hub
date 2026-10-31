//! hub-engine —— 采集 → 索引 的流水线。
//!
//! ## 为什么单独一层
//!
//! 原本这段逻辑写在 `hub-cli::cmd_index` 里。只要引入后台守护进程，
//! 就会立刻出现两条路径：**手动 `hub index`** 和 **daemon 自动扫描**。
//! 两条路径各写一遍"怎么遍历、怎么判重、怎么跳过"，行为必然分叉 ——
//! 第一版的根本问题就是这个形状：`parse_query` 只在 CLI 里被调用，
//! 持久化层自己另搞一套 `QueryParser`，于是那 29 条测试保护的是一个不存在的生产路径。
//!
//! 所以这里把流水线抽成唯一边界：`hub-cli` 和 `hub-daemon` 都只能通过
//! [`Engine`] 写数据，不允许绕过它直接碰 `Indexer`。
//!
//! ## 四个入口
//!
//! | 入口 | 谁用 | 做什么 |
//! |---|---|---|
//! | [`Engine::scan_all`] | `hub index` / daemon 启动 | 全量遍历 roots，增量判定 |
//! | [`Engine::index_paths`] | daemon watcher | 只处理变更到的具体文件 |
//! | [`Engine::remove_paths`] | daemon watcher | 文件被删后打墓碑 |
//! | [`Engine::reconcile`] | daemon 定时任务 | 删除检测 + 补齐漏掉的变更 |
//!
//! ## 一条不可协商的规则
//!
//! **所有路径都用 `/` 分隔的绝对路径做 uri**。Windows 上同一个文件
//! 可能以 `C:\a\b` 和 `C:/a/b` 两种形态出现，混用会让水位表里
//! 同一个文件存两行，`reconcile` 还会把其中一行判成"已删除"。

pub mod config;
pub mod engine;

pub use config::EngineConfig;
pub use engine::{Engine, IngestReport};
