//! 采集配置。CLI 参数与 daemon 配置文件最终都收敛到这里。

use std::path::PathBuf;

/// 默认单文件上限：2MB。超过的基本是日志/数据文件，索引进来的信噪比很低。
pub const DEFAULT_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// 要扫描的根目录。同一份配置同时被 `scan_all` 与 `reconcile` 使用。
    pub roots: Vec<PathBuf>,
    /// 单文件大小上限。0 表示用默认值。
    pub max_file_bytes: u64,
    /// 是否把终端历史也纳入
    pub with_history: bool,
    /// 每多少篇文档提交一次。太小则 commit 次数过多（每次 commit 都要写段文件），
    /// 太大则内存里堆太多待写文档、崩溃时丢的多。
    pub batch: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            with_history: false,
            batch: 500,
        }
    }
}

impl EngineConfig {
    pub fn max_bytes(&self) -> u64 {
        if self.max_file_bytes == 0 {
            DEFAULT_MAX_FILE_BYTES
        } else {
            self.max_file_bytes
        }
    }
}
