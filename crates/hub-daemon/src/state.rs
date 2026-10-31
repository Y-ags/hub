//! 运行状态：pid、心跳、累计统计。
//!
//! ## 为什么用文件而不是 IPC
//!
//! `hub daemon status` 是个独立进程，需要知道守护进程还活着、干了多少活。
//! 正经做法是命名管道或 localhost socket —— 但那要再加一套协议、超时、
//! 权限处理，而这里要传的信息总共不到 200 字节，且**不需要实时**。
//!
//! 用文件的好处是：崩了不会留下死锁的 socket，用户可以直接 `cat` 看，
//! `hub daemon stop` 也不需要先建立连接才知道对方在不在。

use std::io;
use std::path::{Path, PathBuf};

use crate::config::HEARTBEAT_STALE_SECS;

/// 主循环每一轮刷新的状态。字段刻意保持扁平，方便 `key=value` 序列化。
#[derive(Debug, Clone, Default)]
pub struct State {
    pub pid: u32,
    pub started_at: i64,
    pub heartbeat_at: i64,
    /// 上次有文件变更的时间（不是心跳）
    pub last_event_at: i64,
    pub last_reconcile_at: i64,
    pub indexed_total: usize,
    pub deleted_total: usize,
    pub reconcile_runs: usize,
    /// 上次段合并的时间与次数
    pub last_merge_at: i64,
    pub merge_runs: usize,
    /// 最近一次观测到的索引段数（不合并也更新，用来判断"该不该合"）
    pub segments: usize,
    /// 主循环轮次，用来判断进程是"活着但卡住"还是"根本没在跑"
    pub ticks: u64,
    pub roots: Vec<String>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl State {
    pub fn path(data: &Path) -> PathBuf {
        data.join("daemon.state")
    }

    pub fn stop_flag(data: &Path) -> PathBuf {
        data.join("daemon.stop")
    }

    pub fn log_path(data: &Path) -> PathBuf {
        data.join("daemon.log")
    }

    /// 进程还在不在。
    pub fn pid_alive(&self) -> bool {
        crate::process::pid_running(self.pid)
    }

    /// 是否还在**正常干活**：进程活着，且心跳新鲜。
    ///
    /// 两个条件都要：只看心跳会把"刚死 30 秒"报成存活；只看 pid 会把
    /// "进程卡死在某个系统调用里"报成健康。
    pub fn is_alive(&self) -> bool {
        self.pid_alive() && self.heartbeat_at > 0 && now() - self.heartbeat_at <= HEARTBEAT_STALE_SECS
    }

    /// 进程活着但心跳停了 —— 卡住，或者正卡在冷启动的大目录扫描里
    pub fn is_stalled(&self) -> bool {
        self.pid_alive() && !self.is_alive()
    }

    pub fn heartbeat_age(&self) -> i64 {
        if self.heartbeat_at <= 0 {
            -1
        } else {
            now() - self.heartbeat_at
        }
    }

    pub fn serialize(&self) -> String {
        let roots = self
            .roots
            .iter()
            .map(|r| r.replace('\n', " "))
            .collect::<Vec<_>>()
            .join("\t");
        format!(
            "pid={}\nstarted_at={}\nheartbeat_at={}\nlast_event_at={}\n\
             last_reconcile_at={}\nindexed_total={}\ndeleted_total={}\n\
             reconcile_runs={}\nlast_merge_at={}\nmerge_runs={}\nsegments={}\n\
             ticks={}\nroots={}\n",
            self.pid,
            self.started_at,
            self.heartbeat_at,
            self.last_event_at,
            self.last_reconcile_at,
            self.indexed_total,
            self.deleted_total,
            self.reconcile_runs,
            self.last_merge_at,
            self.merge_runs,
            self.segments,
            self.ticks,
            roots
        )
    }

    pub fn parse(text: &str) -> Self {
        let mut s = State::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim() {
                "pid" => s.pid = v.trim().parse().unwrap_or(0),
                "started_at" => s.started_at = v.trim().parse().unwrap_or(0),
                "heartbeat_at" => s.heartbeat_at = v.trim().parse().unwrap_or(0),
                "last_event_at" => s.last_event_at = v.trim().parse().unwrap_or(0),
                "last_reconcile_at" => s.last_reconcile_at = v.trim().parse().unwrap_or(0),
                "indexed_total" => s.indexed_total = v.trim().parse().unwrap_or(0),
                "deleted_total" => s.deleted_total = v.trim().parse().unwrap_or(0),
                "reconcile_runs" => s.reconcile_runs = v.trim().parse().unwrap_or(0),
                "last_merge_at" => s.last_merge_at = v.trim().parse().unwrap_or(0),
                "merge_runs" => s.merge_runs = v.trim().parse().unwrap_or(0),
                "segments" => s.segments = v.trim().parse().unwrap_or(0),
                "ticks" => s.ticks = v.trim().parse().unwrap_or(0),
                "roots" => {
                    s.roots = v
                        .split('\t')
                        .filter(|x| !x.is_empty())
                        .map(str::to_string)
                        .collect()
                }
                _ => {}
            }
        }
        s
    }

    /// 原子写：先写临时文件再改名。
    ///
    /// 不原子写的后果很具体：`hub daemon status` 恰好在写到一半时读到文件，
    /// 会看到空的 `heartbeat_at`，然后把一个正在干活的进程报成"已停止"。
    pub fn save(&self, data: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data)?;
        let path = Self::path(data);
        let tmp = data.join(format!("daemon.state.{}.tmp", self.pid));
        std::fs::write(&tmp, self.serialize())?;
        // Windows 上 rename 到已存在的目标会失败，先删
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::rename(&tmp, &path)
    }

    pub fn load(data: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(data)).ok()?;
        Some(Self::parse(&text))
    }

    pub fn clear(data: &Path) {
        let _ = std::fs::remove_file(Self::path(data));
    }
}

// ============================================================
// 停止信号
// ============================================================

/// 请求停止：写一个标记文件，主循环每轮检查。
///
/// 不用 `TerminateProcess` 是因为那条路会让 Tantivy 的段文件处于半写状态，
/// 下次打开需要恢复；也不用 Ctrl-C 广播，因为 daemon 是脱离开的，
/// 没有控制台可以接收。
pub fn request_stop(data: &Path) -> io::Result<()> {
    std::fs::create_dir_all(data)?;
    std::fs::write(State::stop_flag(data), b"stop")
}

pub fn stop_requested(data: &Path) -> bool {
    State::stop_flag(data).exists()
}

pub fn clear_stop(data: &Path) {
    let _ = std::fs::remove_file(State::stop_flag(data));
}

// ============================================================
// 时间格式化（不引 chrono）
// ============================================================

/// unix 秒 → "2026-10-31 21:40:28"。
///
/// 用 Howard Hinnant 的 `civil_from_days`：把纪元日数换算成 y/m/d，
/// 20 行搞定，比为了打一行日志引入 chrono 划算。
pub fn fmt_time(secs: i64) -> String {
    if secs <= 0 {
        return "-".into();
    }
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formatting_matches_known_dates() {
        // 0 / 负数表示"未设置"，日志里打成 `-` 比 1970 年更有意义
        assert_eq!(fmt_time(0), "-");
        assert_eq!(fmt_time(-1), "-");
        // 下面两个值用 `date -u -d @<秒>` 校验过
        assert_eq!(fmt_time(1_793_463_628), "2026-10-31 16:20:28");
        assert_eq!(fmt_time(1_700_000_000), "2023-11-14 22:13:20");
    }

    #[test]
    fn state_roundtrip() {
        let s = State {
            pid: 4242,
            started_at: 1_700_000_000,
            heartbeat_at: 1_700_000_010,
            indexed_total: 99,
            deleted_total: 3,
            reconcile_runs: 2,
            last_merge_at: 1_700_000_020,
            merge_runs: 1,
            segments: 6,
            ticks: 17,
            roots: vec!["C:/code".into(), "C:/notes".into()],
            ..Default::default()
        };
        let back = State::parse(&s.serialize());
        assert_eq!(back.pid, 4242);
        assert_eq!(back.indexed_total, 99);
        assert_eq!(back.reconcile_runs, 2);
        assert_eq!(back.last_merge_at, 1_700_000_020);
        assert_eq!(back.merge_runs, 1);
        assert_eq!(back.segments, 6);
        assert_eq!(back.roots, vec!["C:/code", "C:/notes"]);
    }

    #[test]
    fn dead_heartbeat_is_not_alive() {
        let mut s = State::default();
        assert!(!s.is_alive(), "pid = 0 不该被判定为存活");

        // 用一个真的存在且活着的 pid（就是测试进程自己）
        s.pid = std::process::id();
        s.heartbeat_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(s.is_alive(), "pid 活着 + 心跳新鲜，应判定存活");

        // 心跳停在 10 分钟前 → 进程还在但已经没在干活
        s.heartbeat_at -= 600;
        assert!(!s.is_alive());
        assert!(s.is_stalled(), "进程活着但心跳过期，应判定为卡住");
    }

    #[test]
    fn atomic_save_leaves_valid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = State::default();
        s.pid = 7;
        s.heartbeat_at = 1;
        s.save(tmp.path()).unwrap();
        let back = State::load(tmp.path()).unwrap();
        assert_eq!(back.pid, 7);
        // 临时文件不该残留
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
