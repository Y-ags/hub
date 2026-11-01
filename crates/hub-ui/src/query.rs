//! UI 的数据源与展示格式化。
//!
//! 这一层刻意**不碰任何 Win32 调用**：把"查什么、显示什么"和"怎么画"分开，
//! 前者能写单测，后者只能靠人眼。
//!
//! 查询顺序与 CLI 一致：先问 daemon（它持有会随 commit 刷新的 reader，
//! 数据最新），连不上再退回本地打开索引。UI 是长时间运行的进程，
//! 本地路径必须每轮 `reload()` —— 否则 daemon 后来启动、写了很多新东西，
//! 窗口里永远看不到。

use std::path::{Path, PathBuf};

use hub_store::index::Hit;
use hub_store::indexer::Indexer;

/// 结果从哪来。显示在状态栏，用户能一眼看出"这是实时数据还是本地快照"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Daemon,
    Local,
}

impl Via {
    pub fn label(self) -> &'static str {
        match self {
            Via::Daemon => "实时",
            Via::Local => "本地",
        }
    }
}

/// 列表里的一行。
pub struct Row {
    pub title: String,
    /// 第二行的展示文本（路径 + 时间），由 GDI 的 DT_PATH_ELLIPSIS 自己截断
    pub subtitle: String,
    /// 真的能双击打开的文件路径。终端历史为 `None`
    pub path: Option<PathBuf>,
    /// 原始 uri，用于"复制路径"
    pub uri: String,
    pub source: String,
}

pub struct Reply {
    pub rows: Vec<Row>,
    pub num_docs: usize,
    pub elapsed_ms: f32,
    pub via: Via,
}

pub struct Engine {
    data: PathBuf,
    local: Option<Indexer>,
}

impl Engine {
    pub fn new(data: impl AsRef<Path>) -> Self {
        Self {
            data: data.as_ref().to_path_buf(),
            local: None,
        }
    }

    /// 查一次。失败不抛给界面 —— 界面上的报错位置只有一行状态栏，
    /// 与其弹框不如把原因显示在那里。
    pub fn search(&mut self, q: &str, k: usize) -> Result<Reply, String> {
        if q.trim().is_empty() {
            // 空查询也要知道索引有多大：界面靠它区分"还没建索引"和
            // "建了但这个词没命中"。之前这里只查本地句柄（还是 None），
            // 于是有 5 篇文档时也显示"索引是空的"。
            let (num_docs, via) = self.probe_docs();
            return Ok(Reply {
                rows: Vec::new(),
                num_docs,
                elapsed_ms: 0.0,
                via,
            });
        }

        let started = std::time::Instant::now();
        if let Some(r) = self.ask_daemon(q, k) {
            return Ok(Reply {
                elapsed_ms: started.elapsed().as_secs_f32() * 1000.0,
                ..r
            });
        }

        let idx = self.local_indexer()?;
        // daemon 可能在我们打开之后才写入。Tantivy 的 reload 先比版本号，
        // 没变化时几乎不花钱，所以每轮都调是划算的。
        idx.index().reload().map_err(|e| e.to_string())?;

        let hits = idx
            .search_text(q, k)
            .map_err(|e| e.to_string())?;
        let num_docs = idx.index().num_docs();
        let now = hub_store::indexer::now_secs();

        Ok(Reply {
            rows: hits.iter().map(|h| row_of(h, now)).collect(),
            num_docs,
            elapsed_ms: started.elapsed().as_secs_f32() * 1000.0,
            via: Via::Local,
        })
    }

    fn ask_daemon(&self, q: &str, k: usize) -> Option<Reply> {
        use hub_daemon::ipc;

        let mut c = ipc::Client::connect(&self.data).ok()?;
        // 版本对不上就当它不在：升级后旧 daemon 仍在跑是常态，
        // 拿它返回的结构解析出歪结果比退回本地糟得多
        if c.ping().ok()?.proto != ipc::PROTO {
            return None;
        }
        let resp = c.call(&ipc::encode_search(k, q)).ok()?;
        let parsed = ipc::parse_search_reply(&resp).ok()?;
        let now = hub_store::indexer::now_secs();
        Some(Reply {
            rows: parsed.hits.iter().map(|h| row_of(h, now)).collect(),
            num_docs: parsed.num_docs,
            elapsed_ms: 0.0,
            via: Via::Daemon,
        })
    }

    fn local_indexer(&mut self) -> Result<&mut Indexer, String> {
        if self.local.is_none() {
            let ix = Indexer::open(&self.data)
                .map_err(|e| format!("打开数据目录失败：{e}"))?;
            self.local = Some(ix);
        }
        Ok(self.local.as_mut().unwrap())
    }

    /// 索引里有多少文档，以及这个数字是从哪拿到的。
    /// 先问 daemon（不必打开索引），连不上再自己开。
    fn probe_docs(&mut self) -> (usize, Via) {
        if let Some(n) = self.daemon_docs() {
            return (n, Via::Daemon);
        }
        match self.local_indexer() {
            Ok(ix) => (ix.index().num_docs(), Via::Local),
            Err(_) => (0, Via::Local),
        }
    }

    fn daemon_docs(&self) -> Option<usize> {
        use hub_daemon::ipc;
        let mut c = ipc::Client::connect(&self.data).ok()?;
        let p = c.ping().ok()?;
        if p.proto != ipc::PROTO {
            return None;
        }
        Some(p.num_docs)
    }
}

fn row_of(h: &Hit, now: i64) -> Row {
    let path = openable_path(&h.uri, h.source.as_str());
    let mut subtitle = match &path {
        Some(p) => p.to_string_lossy().replace('/', "\\"),
        // 终端历史没有文件可打开。以前这里直接显示原始 uri，
        // 而它末尾挂着一串由命令内容算出来的哈希 —— 对用户毫无意义，
        // 却把整行塞满，把"什么时候敲的"挤到看不见的地方。
        None => "终端历史".to_string(),
    };
    subtitle.push_str("    ");
    subtitle.push_str(&human_ago(h.mtime, now));

    Row {
        title: if h.title.trim().is_empty() {
            h.uri.clone()
        } else {
            h.title.clone()
        },
        subtitle,
        path,
        uri: h.uri.clone(),
        source: h.source.as_str().to_string(),
    }
}

/// uri 能不能落到一个真实文件上。
///
/// `files` 来源的 uri 是规范化后的绝对路径（`C:/a/b.rs`），换成反斜杠即可
/// 交给 ShellExecute。`terminal` 来源是 `terminal://.../哈希`，没有文件。
pub fn openable_path(uri: &str, source: &str) -> Option<PathBuf> {
    if source != "files" {
        return None;
    }
    if uri.contains("://") {
        return None;
    }
    Some(PathBuf::from(uri.replace('/', "\\")))
}

/// "3 天前" 这类相对时间。超过 30 天就显示日期 —— "47 天前"没人换算得动。
pub fn human_ago(mtime: i64, now: i64) -> String {
    if mtime <= 0 {
        return String::new();
    }
    let d = now - mtime;
    if d < 0 {
        // 未来时间：时钟漂移或文件来自别的时间设置，别显示"负 3 天前"
        return "刚刚".to_string();
    }
    match d {
        0..=59 => "刚刚".to_string(),
        60..=3599 => format!("{} 分钟前", d / 60),
        3600..=86_399 => format!("{} 小时前", d / 3600),
        86_400..=2_591_999 => format!("{} 天前", d / 86_400),
        _ => ymd(mtime),
    }
}

/// unix 秒 → `2026-08-14`。不引 chrono：只需要年月日，
/// 用公历换算的常用算法就够，省一个依赖。
pub fn ymd(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    // 1970-01-01 起的天数 → 公历年月日（Howard Hinnant 的 civil_from_days）
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_buckets() {
        let now = 1_700_000_000;
        assert_eq!(human_ago(now, now), "刚刚");
        assert_eq!(human_ago(now - 30, now), "刚刚");
        assert_eq!(human_ago(now - 90, now), "1 分钟前");
        assert_eq!(human_ago(now - 7200, now), "2 小时前");
        assert_eq!(human_ago(now - 86_400 * 3, now), "3 天前");
        // 超过 30 天退回日期
        let old = human_ago(now - 86_400 * 47, now);
        assert!(old.starts_with("20"), "{old}");
        assert_eq!(old.len(), 10, "应是 YYYY-MM-DD：{old}");
        // 未来时间不能出现负数
        assert_eq!(human_ago(now + 500, now), "刚刚");
        // 没有时间戳
        assert_eq!(human_ago(0, now), "");
    }

    #[test]
    fn ymd_matches_known_dates() {
        assert_eq!(ymd(0), "1970-01-01");
        // 2023-11-14T22:13:20Z
        assert_eq!(ymd(1_700_000_000), "2023-11-14");
        // 闰日
        assert_eq!(ymd(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn only_files_uris_are_openable() {
        let p = openable_path("C:/edge/hub/src/main.rs", "files").unwrap();
        assert_eq!(p.to_string_lossy(), "C:\\edge\\hub\\src\\main.rs");
        // 终端历史：没有文件可开
        assert!(openable_path("terminal://C:/x/history/0a1b2c", "terminal").is_none());
        // 来源对但 uri 是自定义协议，仍然不算文件
        assert!(openable_path("files://whatever", "files").is_none());
    }
}
