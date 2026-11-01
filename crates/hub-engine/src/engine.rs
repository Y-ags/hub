//! 采集流水线实现。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use hub_connectors::{
    default_history_files, is_noise, parse_history, read_text, walk, WalkOptions, WalkStats,
};
use hub_core::{ParsedQuery, SourceId};
use hub_store::indexer::hash_bytes;
use hub_store::{Hit, Indexer};

use crate::config::EngineConfig;

/// 一次采集的结果。字段拆得比较细，是为了让 daemon 的日志能回答
/// "刚才到底扫了多少、为什么什么都没写进去"。
#[derive(Debug, Default, Clone)]
pub struct IngestReport {
    /// 遍历到的候选文件/条目总数
    pub scanned: usize,
    /// 真正落盘的文档数（认 flush 返回值，不认入队次数）
    pub indexed: usize,
    /// mtime/hash 没变被跳过的
    pub skipped: usize,
    /// 标记删除的
    pub deleted: usize,
    /// 目录 / 扩展名 / 超限 / 软链 各自的跳过数
    pub skipped_dir: usize,
    pub skipped_ext: usize,
    pub skipped_size: usize,
    pub skipped_symlink: usize,
    /// 读失败 / 权限不足 / 文件被占用
    pub errors: usize,
    pub elapsed_ms: u128,
}

impl IngestReport {
    fn absorb_walk(&mut self, ws: &WalkStats) {
        self.skipped_dir += ws.skipped_dir;
        self.skipped_ext += ws.skipped_ext;
        self.skipped_size += ws.skipped_size;
        self.skipped_symlink += ws.skipped_symlink;
        self.errors += ws.errors;
    }

    /// 有没有产生任何实际变更。daemon 用它决定要不要打日志、要不要通知。
    pub fn churned(&self) -> bool {
        self.indexed > 0 || self.deleted > 0
    }
}

/// 把 Path 规范成 uri：绝对化 + 统一 `/` 分隔 + 去掉 Windows 的 `\\?\` 前缀。
///
/// 这三步缺一不可：
/// - 不绝对化 → `hub index .` 和 `hub index C:/code` 会产生两组 uri
/// - 不统一分隔符 → Windows 上同一文件两种写法，水位表存两行
/// - 不去 `\\?\` → 遍历深层路径时 `read_dir` 可能返回 verbatim 前缀，
///   与用户传进来的普通路径对不上
pub fn to_uri(path: &Path) -> String {
    let p: PathBuf = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let s = p.to_string_lossy().replace('\\', "/");
    // 折叠 `.` 与 `..`，避免 `C:/a/../b` 与 `C:/b` 变成两条记录
    let s = normalize_uri(&s);
    s.strip_prefix("//?/").map(str::to_string).unwrap_or(s)
}

/// 纯字符串层面的 `.` / `..` 折叠，不碰文件系统（文件可能已被删除）。
fn normalize_uri(s: &str) -> String {
    let (prefix, rest) = if let Some(r) = s.strip_prefix("//?/") {
        ("//?/", r)
    } else if s.len() >= 2 && s.as_bytes()[1] == b':' {
        (&s[..2], &s[2..])
    } else {
        ("", s)
    };
    let mut out: Vec<&str> = Vec::new();
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    if prefix.is_empty() {
        joined
    } else if prefix.ends_with('/') {
        // `//?/` 已经带分隔符了，再拼一个会变成 `//?//C:/...`
        format!("{prefix}{joined}")
    } else {
        format!("{prefix}/{joined}")
    }
}

/// uri 是否位于某个 root 之下。用于 reconcile 时限定删除范围：
/// 用户把 `~/code` 从配置里去掉后，那些记录不该被当成"文件被删了"。
fn uri_under(uri: &str, root_uri: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").to_ascii_lowercase();
    let (u, r) = (norm(uri), norm(root_uri));
    if u == r {
        return true;
    }
    let mut rp = r;
    if !rp.ends_with('/') {
        rp.push('/');
    }
    u.starts_with(&rp)
}

fn mtime_of(path: &Path) -> i64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct Engine {
    indexer: Indexer,
    cfg: EngineConfig,
    opts: WalkOptions,
    /// 每个历史文件上次见到的 (字节数, mtime)。一样就完全不用解析 ——
    /// 用户不敲命令时，轮询的代价只是几次 stat。
    history_stamp: HashMap<PathBuf, (u64, i64)>,
    /// 已经写进索引的历史条目 uri。终端历史动辄几千条，每轮为每条查一次
    /// SQLite 是纯浪费；这个集合让重复条目在内存里就被挡掉。
    history_seen: HashSet<String>,
}

impl Engine {
    pub fn open(data: impl AsRef<Path>, cfg: EngineConfig) -> hub_store::Result<Self> {
        let indexer = Indexer::open(data)?;
        let opts = WalkOptions {
            max_file_bytes: cfg.max_bytes(),
            ..Default::default()
        };
        Ok(Self {
            indexer,
            cfg,
            opts,
            history_stamp: HashMap::new(),
            history_seen: HashSet::new(),
        })
    }

    pub fn cfg(&self) -> &EngineConfig {
        &self.cfg
    }

    pub fn cfg_mut(&mut self) -> &mut EngineConfig {
        &mut self.cfg
    }

    pub fn indexer(&self) -> &Indexer {
        &self.indexer
    }

    pub fn indexer_mut(&mut self) -> &mut Indexer {
        &mut self.indexer
    }

    pub fn search_text(&self, q: &str, k: usize) -> hub_store::Result<Vec<Hit>> {
        self.indexer.search_text(q, k)
    }

    pub fn search(&self, pq: &ParsedQuery, k: usize) -> hub_store::Result<Vec<Hit>> {
        self.indexer.search(pq, k)
    }

    pub fn parse(&self, q: &str) -> ParsedQuery {
        self.indexer.parse(q)
    }

    /// 段合并 + 墓碑清理。低频操作，只在 daemon 空闲或用户手动 `hub merge` 时调。
    pub fn merge(&mut self) -> hub_store::Result<()> {
        self.indexer.merge()
    }

    /// 按需合并：没有可回收的东西就不动手。返回是否真的合并了。
    pub fn merge_if_needed(&mut self, min_segments: usize) -> hub_store::Result<bool> {
        self.indexer.merge_if_needed(min_segments)
    }

    pub fn segment_count(&self) -> usize {
        self.indexer.segment_count()
    }

    /// 全量遍历所有 roots，只写有变化的。
    ///
    /// 这就是 `hub index` 的实现，也是 daemon 首次启动时的冷启动路径。
    /// **不删除任何东西** —— 删除检测是 [`Engine::reconcile`] 的职责，
    /// 两者分开是因为"用户手动跑了一次 index"和"后台持续对账"的语义不同：
    /// 前者不该静默把记录删掉（可能只是挂载盘没插）。
    pub fn scan_all(&mut self) -> hub_store::Result<IngestReport> {
        let started = Instant::now();
        let mut rep = IngestReport::default();
        let roots = self.cfg.roots.clone();
        // 先把不可变借用的东西复制出来：闭包里要用 `&mut self` 调 ingest_one，
        // 直接读 `self.opts` / `self.cfg.batch` 会和可变借用打架。
        let opts = self.opts.clone();
        let batch = self.cfg.batch.max(1);

        for root in &roots {
            if !root.exists() {
                continue;
            }
            let mut pending = 0usize;
            let mut first_err: Option<hub_store::StoreError> = None;

            let ws = walk(root, &opts, |hit| {
                if first_err.is_some() {
                    return;
                }
                rep.scanned += 1;
                let uri = to_uri(&hit.path);
                match self.ingest_one(&hit.path, &uri, hit.mtime) {
                    Ok(true) => pending += 1,
                    Ok(false) => rep.skipped += 1,
                    Err(e) => {
                        first_err = Some(e);
                        return;
                    }
                }
                if pending >= batch {
                    match self.indexer.flush() {
                        Ok(n) => rep.indexed += n,
                        Err(e) => first_err = Some(e),
                    }
                    pending = 0;
                }
            })
            .map_err(hub_store::StoreError::Io)?;

            rep.absorb_walk(&ws);
            if let Some(e) = first_err {
                return Err(e);
            }
        }

        if self.cfg.with_history {
            let h = self.ingest_history()?;
            rep.scanned += h.scanned;
            rep.skipped += h.skipped;
            rep.indexed += h.indexed;
        }

        rep.indexed += self.indexer.flush()?;
        rep.elapsed_ms = started.elapsed().as_millis();
        Ok(rep)
    }

    /// 处理 watcher 报上来的具体文件。
    ///
    /// 与 `scan_all` 的区别是**没有遍历过程**，所以跳过规则只能靠
    /// `WalkOptions::should_index(&path)` 自己判 —— 这正是那条规则必须公开的原因。
    pub fn index_paths(&mut self, paths: &[PathBuf]) -> hub_store::Result<IngestReport> {
        let started = Instant::now();
        let mut rep = IngestReport::default();

        for p in paths {
            rep.scanned += 1;
            // 路径可能刚被删掉
            let md = match fs::metadata(p) {
                Ok(m) => m,
                Err(_) => {
                    rep.skipped += 1;
                    continue;
                }
            };
            if !md.is_file() {
                rep.skipped += 1;
                continue;
            }
            if !self.indexable(p) || md.len() == 0 || md.len() > self.opts.max_file_bytes {
                rep.skipped += 1;
                continue;
            }
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let uri = to_uri(p);
            match self.ingest_one(p, &uri, mtime) {
                Ok(true) => {}
                Ok(false) => rep.skipped += 1,
                // 单个文件失败（被占用 / 权限）不该打断整批
                Err(_) => rep.errors += 1,
            }
        }

        rep.indexed = self.indexer.flush()?;
        rep.elapsed_ms = started.elapsed().as_millis();
        Ok(rep)
    }

    /// 文件被删除 / 重命名后的墓碑。
    ///
    /// 对每个路径做**子树删除**：watcher 只会报一条 `Remove(目录)`，
    /// 但索引里可能有该目录下的几百条记录。只按精确 uri 删会把它们全部漏掉，
    /// 留下永远搜得到的幽灵文档。
    pub fn remove_paths(&mut self, paths: &[PathBuf]) -> hub_store::Result<IngestReport> {
        let started = Instant::now();
        let mut removed = 0usize;
        for p in paths {
            let uri = to_uri(p);
            match self.indexer.delete_under(SourceId::Files, &uri) {
                Ok(n) => removed += n,
                Err(e) => return Err(e),
            }
        }
        self.indexer.flush()?;
        Ok(IngestReport {
            scanned: paths.len(),
            deleted: removed,
            indexed: 0,
            elapsed_ms: started.elapsed().as_millis(),
            ..Default::default()
        })
    }

    /// 供 daemon / 调试判断某个路径是否会被跳过规则挡住
    pub fn path_indexable(&self, path: &Path) -> bool {
        self.indexable(path)
    }

    /// 对账：一次遍历同时完成**补齐 / 更新 / 删除**三种判定。
    ///
    /// 为什么必须有这一步：watcher 会漏事件。典型场景 ——
    /// 笔记本合盖睡眠期间用户改了文件、Windows 的 `ReadDirectoryChangesW`
    /// 缓冲区溢出（大量文件同时变化）、进程没在跑的那几秒。
    /// 光靠事件流，索引会永远留着一批幽灵文档，也会永远漏掉一批新文件。
    ///
    /// 为什么它比 [`Engine::scan_all`] 快：`scan_all` 对每个文件都要查一次
    /// SQLite 水位表，这里是**先把水位表整体装进内存**（一次 SQL），
    /// 再拿 `HashMap` 比对。5 万文件量级下差距是一个数量级的查询次数。
    ///
    /// 也因此，daemon 冷启动直接用它 —— 一次对账就把"磁盘上有什么"
    /// 和"索引里有什么"拉平了，不需要额外跑一遍全量扫描。
    pub fn reconcile(&mut self) -> hub_store::Result<IngestReport> {
        let started = Instant::now();
        let mut rep = IngestReport::default();
        let roots = self.cfg.roots.clone();
        if roots.is_empty() {
            return Ok(rep);
        }

        // 1) 水位表整体进内存：uri -> mtime
        let mut watermark: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        self.indexer
            .meta()
            .for_each_live_meta(SourceId::Files, |uri, mtime| {
                watermark.insert(uri.to_string(), mtime);
            })?;

        // 2) 遍历磁盘：收集实际存在的集合，顺便挑出要索引的
        let mut present: HashSet<String> = HashSet::new();
        let mut changed: Vec<(PathBuf, i64)> = Vec::new();
        let opts = self.opts.clone();
        let mut walk_ok_roots = 0usize;

        for root in &roots {
            if !root.exists() {
                // 根目录整体不在了（U 盘拔了 / 网络盘断开）—— 不做删除判定，
                // 否则一次挂载失败会把整棵树的记录清空
                continue;
            }
            walk_ok_roots += 1;
            match walk(root, &opts, |hit| {
                let uri = to_uri(&hit.path);
                present.insert(uri.clone());
                rep.scanned += 1;
                match watermark.get(&uri) {
                    // mtime 一致：认为没变，不读文件
                    Some(&mt) if mt == hit.mtime => {}
                    _ => changed.push((hit.path, hit.mtime)),
                }
            }) {
                Ok(ws) => rep.absorb_walk(&ws),
                Err(_) => rep.errors += 1,
            }
        }

        // 一个 root 都没扫成 → 说明路径配置全错或全被卸载，此时做删除判定等于清库
        if walk_ok_roots == 0 || present.is_empty() {
            rep.elapsed_ms = started.elapsed().as_millis();
            return Ok(rep);
        }

        // 3) 索引新增与修改
        //
        // 先按跳过规则过滤再进循环：`indexable` 借的是 `&self`，
        // 而 `ingest_one` 要 `&mut self`，两者不能同时存在。
        let changed: Vec<(PathBuf, i64)> = changed
            .into_iter()
            .filter(|(p, _)| self.indexable(p))
            .collect();

        let batch = self.cfg.batch.max(1);
        let mut pending = 0usize;
        for (path, mtime) in changed {
            let uri = to_uri(&path);
            match self.ingest_one(&path, &uri, mtime) {
                Ok(true) => {
                    pending += 1;
                    if pending >= batch {
                        rep.indexed += self.indexer.flush()?;
                        pending = 0;
                    }
                }
                Ok(false) => rep.skipped += 1,
                Err(_) => rep.errors += 1,
            }
        }
        rep.indexed += self.indexer.flush()?;

        // 4) 删除检测：水位表里有、磁盘上没有，且属于当前 roots
        let root_uris: Vec<String> = roots.iter().map(|r| to_uri(r)).collect();
        let mut stale: Vec<String> = Vec::new();
        for uri in watermark.keys() {
            if !root_uris.iter().any(|r| uri_under(uri, r)) {
                continue;
            }
            if !present.contains(uri) {
                stale.push(uri.clone());
            }
        }

        if !stale.is_empty() {
            rep.deleted = stale.len();
            self.indexer.delete_many(SourceId::Files, stale);
            self.indexer.flush()?;
        }

        rep.elapsed_ms = started.elapsed().as_millis();
        Ok(rep)
    }

    /// 终端历史。幂等：uri 只由内容和时间决定，历史文件追加内容时旧条目不变。
    ///
    /// daemon 会**周期性**调它（用户敲完命令几秒内就该能搜到），所以两条快路径
    /// 是必须的：
    /// 1. 文件大小 + mtime 都没变 → 连读取和解析都跳过，只花一次 stat
    /// 2. 条目已经在 `history_seen` 里 → 不查库、不入队
    ///
    /// 没有第 2 条的话，每轮都要为几千条历史各做一次 SQLite 查询。
    pub fn ingest_history(&mut self) -> hub_store::Result<IngestReport> {
        let started = Instant::now();
        let mut rep = IngestReport::default();

        // 配置里指定了就用指定的，否则用平台默认位置（zsh / bash / PSReadLine）
        let files = if self.cfg.history_files.is_empty() {
            default_history_files()
        } else {
            self.cfg.history_files.clone()
        };

        for f in files {
            let stamp = match fs::metadata(&f) {
                Ok(m) => (m.len(), mtime_of(&f)),
                Err(_) => continue,
            };
            if self.history_stamp.get(&f) == Some(&stamp) {
                continue;
            }

            let text = match read_text(&f, 32 * 1024 * 1024) {
                Ok(Some(t)) => t,
                _ => continue,
            };
            let file_mtime = stamp.1;
            let file_uri = to_uri(&f);

            for e in parse_history(&text, file_mtime) {
                if is_noise(&e.cmd) {
                    continue;
                }
                rep.scanned += 1;

                // 有真实时间戳（zsh / bash）→ 条目由 (时间, 命令) 唯一标识。
                // 纯文本历史（PSReadLine）没有时间戳 → **只按命令去重**。
                //
                // 后者不是偷懒：拿 mtime 兜底再参与 uri，文件每追加一条命令
                // mtime 就变，整份历史的 uri 全部改变，于是每轮都把几千条
                // 重写一遍。副作用（同一命令只留一条）恰好也是我们想要的。
                let uri = if e.has_ts {
                    let key = format!("{}:{}", e.ts, e.cmd);
                    format!("terminal://{file_uri}/{:016x}", hash_bytes(key.as_bytes()))
                } else {
                    format!("terminal://{file_uri}/{:016x}", hash_bytes(e.cmd.as_bytes()))
                };

                if self.history_seen.contains(&uri) {
                    rep.skipped += 1;
                    continue;
                }

                let known = if e.has_ts {
                    // 冷启动时内存集合是空的，靠水位表挡：mtime 相同就别读了
                    !self.indexer.needs_read(SourceId::Terminal, &uri, e.ts)?
                } else {
                    // 纯文本条目不能比 mtime —— 它的 mtime 就是会变的文件 mtime。
                    // 存在即视为无需更新，时间保留第一次见到的那次。
                    self.indexer
                        .meta()
                        .lookup(SourceId::Terminal, &uri)?
                        .map(|r| r.live)
                        .unwrap_or(false)
                };
                if known {
                    self.history_seen.insert(uri);
                    rep.skipped += 1;
                    continue;
                }

                let title: String = e.cmd.lines().next().unwrap_or("").chars().take(80).collect();
                if self
                    .indexer
                    .add(SourceId::Terminal, &uri, title, e.cmd.clone(), e.ts)?
                {
                    self.history_seen.insert(uri);
                    // 历史条目量可能上万，边加边提交
                    if self.indexer.pending_len() >= self.cfg.batch {
                        rep.indexed += self.indexer.flush()?;
                    }
                } else {
                    self.history_seen.insert(uri);
                    rep.skipped += 1;
                }
            }
            // 只有整份文件解析完才记下 stamp：中途出错下次还要重来
            self.history_stamp.insert(f, stamp);
        }

        rep.indexed += self.indexer.flush()?;
        rep.elapsed_ms = started.elapsed().as_millis();
        Ok(rep)
    }

    /// 判断某个路径是否该进索引。
    ///
    /// 关键在**先把 root 剥掉再判**：跳过规则描述的是"根目录以下哪些子目录是噪声"，
    /// 不是"绝对路径里出现过哪些词"。拿绝对路径判，`root` 落在 `%TEMP%` /
    /// `%APPDATA%` / `~/Library` 下时整棵树都会被跳过 —— 而且静默无提示。
    fn indexable(&self, path: &Path) -> bool {
        match self.cfg.roots.iter().find_map(|r| path.strip_prefix(r).ok()) {
            Some(rel) => self.opts.should_index_rel(rel),
            // watcher 理论上不会报 root 之外的路径，但真报了也别崩
            None => self.opts.should_index(path),
        }
    }

    /// 读一个文件并尝试入队。返回 `Ok(false)` 表示内容没变。
    fn ingest_one(
        &mut self,
        path: &Path,
        uri: &str,
        mtime: i64,
    ) -> hub_store::Result<bool> {
        // 第一道闸门：mtime 没变就不读文件
        if !self.indexer.needs_read(SourceId::Files, uri, mtime)? {
            return Ok(false);
        }
        let text = match read_text(path, self.opts.max_file_bytes) {
            Ok(Some(t)) => t,
            // 二进制 / 非 UTF-8 / 超限都算跳过，不是错误
            _ => return Ok(false),
        };
        let title: String = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        self.indexer.add(SourceId::Files, uri, title, text, mtime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_is_normalized() {
        assert_eq!(normalize_uri("C:/a/../b"), "C:/b");
        assert_eq!(normalize_uri("C:/a/./b/"), "C:/a/b");
        assert_eq!(normalize_uri("//?/C:/deep/x"), "//?/C:/deep/x");
        assert_eq!(to_uri(Path::new("C:\\a\\b.rs")), "C:/a/b.rs");
        // verbatim 前缀必须去掉，否则与用户传入的普通路径对不上
        assert_eq!(
            to_uri(Path::new("//?/C:/very/long/path/a.rs")),
            "C:/very/long/path/a.rs"
        );
    }

    #[test]
    fn root_prefix_match_respects_boundary() {
        assert!(uri_under("C:/code/a.rs", "C:/code"));
        assert!(uri_under("C:/code/sub/a.rs", "C:/code"));
        // 不能把 C:/code2 当成 C:/code 的子路径
        assert!(!uri_under("C:/code2/a.rs", "C:/code"));
        assert!(!uri_under("C:/other/a.rs", "C:/code"));
        // Windows 大小写不敏感
        assert!(uri_under("c:/CODE/a.rs", "C:/code"));
    }

    #[test]
    fn should_index_matches_walk_rules() {
        let o = WalkOptions::default();
        assert!(o.should_index(Path::new("C:/code/src/main.rs")));
        assert!(!o.should_index(Path::new("C:/code/node_modules/x/index.js")));
        assert!(!o.should_index(Path::new("C:/code/logo.png")));
        assert!(!o.should_index(Path::new("C:/proj/.git/HEAD")));
    }

    #[test]
    fn skip_rules_only_look_below_root() {
        let o = WalkOptions::default();
        // root 落在 Temp/AppData 下时，绝对路径判定会把整棵树判成噪声 ——
        // 这正是 daemon 第一版一条都索引不进去的原因
        assert!(!o.should_index(Path::new(
            "C:/Users/x/AppData/Local/Temp/proj/src/a.rs"
        )));
        // 剥掉 root 之后只判 `src/a.rs`，就该通过
        assert!(o.should_index_rel(Path::new("src/a.rs")));
        // 根下的噪声目录仍然要被挡住
        assert!(!o.should_index_rel(Path::new("node_modules/pkg/index.js")));
    }
}
