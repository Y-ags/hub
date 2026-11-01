//! daemon 配置。落在 `<data>/daemon.conf`，格式就是 `key=value`。
//!
//! 不用 TOML / JSON 是有意的：这里只有十来个字段，读它的人和写它的代码
//! 都是自己，引入 toml 依赖只会让离线构建多一个变数。
//! 真要复杂配置，那时候再换成正经格式也不迟。

use std::io;
use std::path::{Path, PathBuf};

use hub_engine::EngineConfig;

/// 静默多久算"这批变更结束了"。默认 800ms：
/// 编辑器保存一个文件通常会产生 2–5 条事件（truncate / write / close / attrs），
/// 800ms 足够把它们合并成一次索引，又不至于让用户觉得"改了没反应"。
pub const DEFAULT_DEBOUNCE_MS: u64 = 800;

/// 周期性全量对账的间隔。默认 10 分钟。
///
/// watcher 会漏事件（睡眠、缓冲区溢出、进程不在跑的那段时间），
/// 这个任务就是补漏的。间隔太短会白白消耗 IO，太长则幽灵文档存活太久。
pub const DEFAULT_RECONCILE_SECS: u64 = 600;

/// 心跳间隔。`hub daemon status` 靠它判断进程是真活着还是已经崩了。
pub const DEFAULT_HEARTBEAT_SECS: u64 = 15;

/// 心跳超过这个时长没更新，就认为进程已死
pub const HEARTBEAT_STALE_SECS: i64 = 90;

/// 段合并的检查周期。默认 3 小时。
///
/// 每批写入都会新开一个段，段太多会让每次查询变慢（要逐段打分）；
/// 删掉的文档也只是打了墓碑标记，空间不合并就永远占着。
/// 但合并不是免费的（重写段文件），所以拉长到小时级，只在空闲时做。
pub const DEFAULT_MERGE_SECS: u64 = 3 * 3600;

/// 距上次文件变更至少静默这么久，才允许合并。
///
/// 合并会短暂独占写入器；正在连续编辑时切进去，会让"改完 1 秒内能搜到"
/// 这个体验直接失效。默认 2 分钟。
pub const DEFAULT_MERGE_IDLE_SECS: u64 = 120;

/// 段数达到这个值才因为"段太多"而合并。
///
/// 只有 1–2 个段时合并纯属重写一遍文件，没有任何收益。
pub const DEFAULT_MERGE_MIN_SEGMENTS: usize = 8;

/// 终端历史的检查间隔。默认 5 秒。
///
/// 历史文件不归 watcher 管（它监听的是用户选的目录，不是 `%APPDATA%`），
/// 所以只能轮询。代价极低：文件大小和 mtime 都没变时，一次轮询只花几次 `stat`。
/// 5 秒是"敲完命令切到搜索窗口就能搜到"的量级。
/// 设为 0 表示只在启动时读一次（老行为）。
pub const DEFAULT_HISTORY_POLL_SECS: u64 = 5;

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub engine: EngineConfig,
    pub debounce_ms: u64,
    pub reconcile_secs: u64,
    pub heartbeat_secs: u64,
    /// 多久检查一次"要不要合并段"
    pub merge_secs: u64,
    /// 距上次变更静默多久才允许合并
    pub merge_idle_secs: u64,
    /// 段数达到多少才因为段多而合并
    pub merge_min_segments: usize,
    /// 多久检查一次终端历史文件。0 = 关闭轮询
    pub history_poll_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            engine: EngineConfig::default(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            reconcile_secs: DEFAULT_RECONCILE_SECS,
            heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
            merge_secs: DEFAULT_MERGE_SECS,
            merge_idle_secs: DEFAULT_MERGE_IDLE_SECS,
            merge_min_segments: DEFAULT_MERGE_MIN_SEGMENTS,
            history_poll_secs: DEFAULT_HISTORY_POLL_SECS,
        }
    }
}

impl DaemonConfig {
    pub fn conf_path(data: &Path) -> PathBuf {
        data.join("daemon.conf")
    }

    /// 读配置。文件不存在时返回默认值（首次启动不该报错）。
    pub fn load(data: &Path) -> io::Result<Self> {
        let p = Self::conf_path(data);
        if !p.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&p)?;
        Ok(Self::parse(&text))
    }

    pub fn parse(text: &str) -> Self {
        let mut cfg = Self::default();
        let mut roots: Vec<PathBuf> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                // 多个 root 用 `root=` 重复多行，比一行塞分号好读也好改
                "root" => {
                    if !v.is_empty() {
                        roots.push(PathBuf::from(v));
                    }
                }
                "max_file_bytes" => {
                    if let Ok(n) = v.parse() {
                        cfg.engine.max_file_bytes = n;
                    }
                }
                "with_history" => cfg.engine.with_history = v == "1" || v.eq_ignore_ascii_case("true"),
                "batch" => {
                    if let Ok(n) = v.parse() {
                        cfg.engine.batch = n;
                    }
                }
                "debounce_ms" => {
                    if let Ok(n) = v.parse() {
                        cfg.debounce_ms = n;
                    }
                }
                "reconcile_secs" => {
                    if let Ok(n) = v.parse() {
                        cfg.reconcile_secs = n;
                    }
                }
                "heartbeat_secs" => {
                    if let Ok(n) = v.parse() {
                        cfg.heartbeat_secs = n;
                    }
                }
                "merge_secs" => {
                    if let Ok(n) = v.parse() {
                        cfg.merge_secs = n;
                    }
                }
                "merge_idle_secs" => {
                    if let Ok(n) = v.parse() {
                        cfg.merge_idle_secs = n;
                    }
                }
                "merge_min_segments" => {
                    if let Ok(n) = v.parse() {
                        cfg.merge_min_segments = n;
                    }
                }
                "history_poll_secs" => {
                    if let Ok(n) = v.parse() {
                        cfg.history_poll_secs = n;
                    }
                }
                _ => {}
            }
        }
        cfg.engine.roots = roots;
        if cfg.debounce_ms == 0 {
            cfg.debounce_ms = DEFAULT_DEBOUNCE_MS;
        }
        if cfg.reconcile_secs == 0 {
            cfg.reconcile_secs = DEFAULT_RECONCILE_SECS;
        }
        if cfg.merge_secs == 0 {
            cfg.merge_secs = DEFAULT_MERGE_SECS;
        }
        if cfg.merge_min_segments == 0 {
            cfg.merge_min_segments = DEFAULT_MERGE_MIN_SEGMENTS;
        }
        cfg
    }

    pub fn save(&self, data: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data)?;
        let mut s = String::new();
        s.push_str("# hub daemon 配置（由 `hub daemon start` 写入，可手工编辑）\n");
        for r in &self.engine.roots {
            s.push_str(&format!("root={}\n", r.display()));
        }
        s.push_str(&format!("with_history={}\n", self.engine.with_history as u8));
        s.push_str(&format!("max_file_bytes={}\n", self.engine.max_file_bytes));
        s.push_str(&format!("batch={}\n", self.engine.batch));
        s.push_str(&format!("debounce_ms={}\n", self.debounce_ms));
        s.push_str(&format!("reconcile_secs={}\n", self.reconcile_secs));
        s.push_str(&format!("heartbeat_secs={}\n", self.heartbeat_secs));
        s.push_str(&format!("merge_secs={}\n", self.merge_secs));
        s.push_str(&format!("merge_idle_secs={}\n", self.merge_idle_secs));
        s.push_str(&format!("merge_min_segments={}\n", self.merge_min_segments));
        s.push_str(&format!("history_poll_secs={}\n", self.history_poll_secs));
        std::fs::write(Self::conf_path(data), s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repeated_root_lines() {
        let cfg = DaemonConfig::parse(
            "# 注释\n\
             root=C:/code\n\
             root=D:/notes\n\
             with_history=1\n\
             debounce_ms=500\n",
        );
        assert_eq!(cfg.engine.roots.len(), 2);
        assert_eq!(cfg.engine.roots[1], PathBuf::from("D:/notes"));
        assert!(cfg.engine.with_history);
        assert_eq!(cfg.debounce_ms, 500);
        // 没写的字段用默认值，不能变成 0
        assert_eq!(cfg.reconcile_secs, DEFAULT_RECONCILE_SECS);
    }

    #[test]
    fn zero_values_fall_back_to_defaults() {
        // 把 debounce 写成 0 会让主循环空转，必须兜住
        let cfg = DaemonConfig::parse("debounce_ms=0\nreconcile_secs=0\nmerge_secs=0\nmerge_min_segments=0\n");
        assert_eq!(cfg.debounce_ms, DEFAULT_DEBOUNCE_MS);
        assert_eq!(cfg.reconcile_secs, DEFAULT_RECONCILE_SECS);
        assert_eq!(cfg.merge_secs, DEFAULT_MERGE_SECS);
        assert_eq!(cfg.merge_min_segments, DEFAULT_MERGE_MIN_SEGMENTS);
        // merge_idle_secs 允许是 0：那是"随时可合并"，是合法配置而不是笔误
        assert_eq!(cfg.merge_idle_secs, DEFAULT_MERGE_IDLE_SECS);
    }

    #[test]
    fn merge_settings_survive_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = DaemonConfig::default();
        cfg.engine.roots = vec![PathBuf::from("C:/x")];
        cfg.merge_secs = 7200;
        cfg.merge_idle_secs = 0;
        cfg.merge_min_segments = 3;
        cfg.save(tmp.path()).unwrap();

        let back = DaemonConfig::load(tmp.path()).unwrap();
        assert_eq!(back.merge_secs, 7200);
        assert_eq!(back.merge_idle_secs, 0, "0 是合法值，不能被兜回默认");
        assert_eq!(back.merge_min_segments, 3);
    }

    #[test]
    fn roundtrip_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = DaemonConfig::default();
        cfg.engine.roots = vec![PathBuf::from("C:/a"), PathBuf::from("C:/b")];
        cfg.engine.with_history = true;
        cfg.debounce_ms = 1234;
        cfg.save(tmp.path()).unwrap();

        let back = DaemonConfig::load(tmp.path()).unwrap();
        assert_eq!(back.engine.roots, cfg.engine.roots);
        assert!(back.engine.with_history);
        assert_eq!(back.debounce_ms, 1234);
    }
}
