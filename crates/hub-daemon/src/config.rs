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

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub engine: EngineConfig,
    pub debounce_ms: u64,
    pub reconcile_secs: u64,
    pub heartbeat_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            engine: EngineConfig::default(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            reconcile_secs: DEFAULT_RECONCILE_SECS,
            heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
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
        let cfg = DaemonConfig::parse("debounce_ms=0\nreconcile_secs=0\n");
        assert_eq!(cfg.debounce_ms, DEFAULT_DEBOUNCE_MS);
        assert_eq!(cfg.reconcile_secs, DEFAULT_RECONCILE_SECS);
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
