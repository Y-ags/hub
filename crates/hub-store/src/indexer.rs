//! Indexer —— 把"水位表 + 倒排 + 写入器生命周期"粘起来的那一层。
//!
//! 第一版缺的就是它：`MetaStore` 和 `HubIndex` 各自写得很完整，但没有任何代码把它们串起来，
//! 于是"持久化"这件事在整条链路上并不存在（CLI 只用了内存索引）。
//!
//! 这里定了四条规则：
//! 1. **写一批 → commit → drop writer**。常驻 writer 会一直占着内存预算。
//! 2. **doc_id 由 SQLite 发放**。Tantivy 只是按这个 id 建倒排，索引可重建而 id 不变。
//! 3. **变更判定在写入前**。mtime 没变就直接跳过，连文件都不用读。
//! 4. **写之前先拿写租约**（`lease` 模块）。同一份数据目录上可能同时有 daemon
//!    和 `hub index` 两个写者，Tantivy 的写入器锁又是硬互斥的（撞上就报错，
//!    不排队）—— 所以互斥这件事必须由我们自己**排队**做完。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hub_core::{Doc, DocId, ParsedQuery, SourceId};

use crate::index::Hit;
use crate::lease::{self, WriteLease};
use crate::meta::MetaStore;
use crate::{HubIndex, Result};

/// 一批索引任务的统计
#[derive(Debug, Default, Clone, Copy)]
pub struct IndexerStats {
    /// 扫描到的候选条目
    pub scanned: usize,
    /// 真正入队/写盘的
    pub indexed: usize,
    /// 内容没变被跳过的
    pub skipped: usize,
    /// 标记删除的
    pub deleted: usize,
}

impl IndexerStats {
    pub fn merge(&mut self, other: &IndexerStats) {
        self.scanned += other.scanned;
        self.indexed += other.indexed;
        self.skipped += other.skipped;
        self.deleted += other.deleted;
    }
}

pub struct Indexer {
    meta: MetaStore,
    /// 用 `Arc` 而不是直接持有，是为了能把同一个 reader 交给别的线程（IPC 查询）。
    ///
    /// 这很关键：IPC 线程如果自己再 `open` 一次索引，就会有两套 reader，
    /// 一套随 commit 刷新、一套等 `OnCommitWithDelay`，两边看到的"最新"不同。
    /// 共用一个 `HubIndex` 才有唯一的事实。
    index: Arc<HubIndex>,
    pending: Vec<Doc>,
    deletes: Vec<(SourceId, String)>,
    budget: usize,
    /// 数据目录。写租约要落在这儿的 `writer.lock` 上，所以得留着。
    dir: PathBuf,
    /// 等写租约的耐心。daemon 用 0（试一下就走，撞上了下一轮再补），
    /// 用户命令用默认的 30 秒（他明确说了要写，等是对的行为）。
    lease_wait: Duration,
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// FNV-1a 64 位。不用引入 blake3/sha2：这里只需要"内容变没变"的指纹，
/// 不做安全校验，64 位非加密哈希足够，还省一个依赖。
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 水位表的文件名。
///
/// 抽成常量是因为**已经不止 `Indexer` 一个人用它**：daemon 的 IPC 线程要用
/// 只读连接打开同一个库来回答 `stats`。两处各写一遍字面量，改名的那个下午
/// 就会变成"stats 说元数据库不存在"。
pub const META_FILE: &str = "meta.sqlite";

/// `<data>/meta.sqlite`
pub fn meta_path(dir: impl AsRef<Path>) -> std::path::PathBuf {
    dir.as_ref().join(META_FILE)
}

impl Indexer {
    /// 打开（或创建）数据目录。目录结构：
    /// ```text
    /// <data>/meta.sqlite   水位表（事实源）
    /// <data>/index/        Tantivy 索引（可重建）
    /// ```
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let meta = MetaStore::open(meta_path(dir))?;
        let index = Arc::new(HubIndex::open_or_create(dir.join("index"))?);
        Ok(Self {
            meta,
            index,
            pending: Vec::new(),
            deletes: Vec::new(),
            budget: 15_000_000,
            dir: dir.to_path_buf(),
            // 默认值走环境变量：CLI 想调耐心不用改代码，daemon 会显式覆盖成 0
            lease_wait: lease::default_wait(),
        })
    }

    /// 等写租约的时长。见字段注释：daemon 传 0。
    pub fn set_lease_wait(&mut self, wait: Duration) {
        self.lease_wait = wait;
    }

    pub fn lease_wait(&self) -> Duration {
        self.lease_wait
    }

    /// 数据目录（`meta.sqlite` 与 `index/` 的父目录）。
    pub fn data_dir(&self) -> &Path {
        &self.dir
    }

    /// 进写临界区：拿租约，拿不到就等（最多 `lease_wait`）。
    ///
    /// **只在这一个地方拿**。`flush` / `merge` / `reset` 都是"拿锁 → 写 → 放锁"
    /// 的短临界区，彼此不嵌套，所以不需要可重入 —— 简单到这个程度是有意的，
    /// 可重入锁一旦有了，下一个人就会在不该拿的地方拿。
    fn lease_for(&self, role: &'static str) -> Result<WriteLease> {
        WriteLease::acquire_within(&self.dir, role, self.lease_wait)
    }

    /// 清空数据目录里的索引与水位表（`hub reset`）。
    ///
    /// 必须先于 `open` 调用：Windows 上被 mmap 的段文件删不掉。
    /// 会先拿写租约 —— 删索引目录同样是一次"写"，不能和 daemon 的提交重叠。
    pub fn reset(dir: impl AsRef<Path>) -> Result<()> {
        Self::reset_within(dir, lease::default_wait())
    }

    /// [`Indexer::reset`] 的可控等待版本。测试要它，好断言"有人在写时不会硬干"。
    pub fn reset_within(dir: impl AsRef<Path>, wait: Duration) -> Result<()> {
        let dir = dir.as_ref();
        // 变量名别叫 meta_path —— 会和上面那个函数同名，靠"初始化表达式里
        // 还看得见外层"这条规则才能编过，读的人得停下来想一秒
        let mp = meta_path(dir);
        // 租约文件要落在目录里，目录不存在就先建（acquire 内部也会建，这里显式一下
        // 是为了让"空目录"这种情形也走同一条路）
        std::fs::create_dir_all(dir)?;
        let _lease = WriteLease::acquire_within(dir, "reset", wait)?;
        if mp.exists() {
            let meta = MetaStore::open(&mp)?;
            meta.clear()?;
            drop(meta);
        }
        let idx = dir.join("index");
        if idx.exists() {
            std::fs::remove_dir_all(&idx)?;
        }
        Ok(())
    }

    pub fn index(&self) -> &HubIndex {
        &self.index
    }

    /// 拿一份共享只读句柄（IPC 查询线程用）。
    ///
    /// 注意的是：拿到的只是**索引**，不含水位表。查询路径本来也不需要
    /// 水位表 —— 命中结果里的 uri/title/mtime 都在倒排的 stored fields 里。
    pub fn index_handle(&self) -> Arc<HubIndex> {
        Arc::clone(&self.index)
    }

    pub fn meta(&self) -> &MetaStore {
        &self.meta
    }

    /// 缓冲区里还有多少条待写。daemon / connector 用它决定何时主动 flush，
    /// 不必猜"这一批大概多大"。
    pub fn pending_len(&self) -> usize {
        self.pending.len() + self.deletes.len()
    }

    /// connector 在读文件之前调用：mtime 没变就直接跳过，**不读文件**。
    pub fn needs_read(&self, source: SourceId, uri: &str, mtime: i64) -> Result<bool> {
        Ok(!self.meta.likely_unchanged(source, uri, mtime)?)
    }

    /// 入队一篇文档。返回 `false` 表示内容没变、已跳过。
    pub fn add(
        &mut self,
        source: SourceId,
        uri: &str,
        title: String,
        body: String,
        mtime: i64,
    ) -> Result<bool> {
        let content_hash = hash_bytes(body.as_bytes());
        if !self.meta.needs_reindex(source, uri, content_hash, mtime)? {
            return Ok(false);
        }
        let id: DocId = match self.meta.lookup(source, uri)? {
            Some(row) => row.doc_id,
            None => self.meta.alloc_doc_id()?,
        };
        self.pending.push(Doc {
            id,
            source,
            uri: uri.to_string(),
            title,
            body,
            mtime,
            content_hash,
            deleted: false,
        });
        Ok(true)
    }

    pub fn delete(&mut self, source: SourceId, uri: &str) {
        self.deletes.push((source, uri.to_string()));
    }

    /// 批量删除（reconcile 用）。一次 flush 里只发一次 SQL 事务。
    pub fn delete_many<I>(&mut self, source: SourceId, uris: I)
    where
        I: IntoIterator<Item = String>,
    {
        for u in uris {
            self.deletes.push((source, u));
        }
    }

    /// 删除某个路径及其子树下的所有记录，返回命中条数。
    ///
    /// 这是"整个目录被删掉"的正确处理：watcher 只报一条 `Remove(目录)`，
    /// 但索引里可能有该目录下的几百条记录。daemon 拿不到这些 uri
    /// （它不知道索引里有什么），只能让水位表自己去查。
    pub fn delete_under(&mut self, source: SourceId, prefix: &str) -> Result<usize> {
        let uris = self.meta.live_under(source, prefix)?;
        let n = uris.len();
        self.delete_many(source, uris);
        Ok(n)
    }

    /// 把缓冲的变更写盘。返回本次提交的文档数。
    ///
    /// **整个函数体都在写租约里**：从拿写入器之前，到 commit 之后、水位表写完，
    /// 中间不允许另一个进程插进来。这是"两个 commit 交错"唯一可能造成真实损坏的地方，
    /// 所以临界区就划在这儿 —— 不多不少。
    ///
    /// ## 顺序：先提交倒排，再写水位表
    ///
    /// 反过来的话（先写水位表再提交），中间任何一次提交失败都会留下
    /// **永久**不一致：水位表说"这篇已经按这个 hash 入过库了"，而倒排里根本没有，
    /// 之后每轮对账都会被增量闸门挡住（`needs_reindex` 认为没变），
    /// 于是这篇文件永远搜不到 —— 一个不会自愈的错。
    ///
    /// 现在的顺序最坏情况是"倒排有、水位表没有"：下一轮对账发现水位表没这一行，
    /// 重新读一遍文件、按 uri 先删后加。重复但正确，会自愈。
    ///
    /// ## 为什么提交要重试
    ///
    /// Windows 上刚提交完的一刹那，另一个进程里 `OnCommitWithDelay` 的 reader
    /// 可能正在 reload（重新扫 meta.json、mmap 段文件）。如果它恰好挡住了
    /// 我们的重命名，Tantivy 返回的是 `An IO error occurred: 拒绝访问。`——
    /// 一个和"并发"看起来毫无关系的错误。等几十毫秒再来一次就好。
    pub fn flush(&mut self) -> Result<usize> {
        if self.pending.is_empty() && self.deletes.is_empty() {
            return Ok(0);
        }
        // `_lease` 不能写成 `_`：那样它会立刻被 drop，锁在临界区开始前就放掉了，
        // 而且编译器不会有任何提示 —— 一个下划线就能让整套机制静默失效。
        let _lease = self.lease_for("flush")?;
        let now = now_secs();

        // 同一批次里同一 (source, uri) 可能被写入多次，后写的赢
        let mut seen: HashMap<(SourceId, String), usize> = HashMap::new();
        let mut docs: Vec<Doc> = Vec::with_capacity(self.pending.len());
        for d in self.pending.drain(..) {
            let key = (d.source, d.uri.clone());
            match seen.get(&key) {
                Some(&i) => docs[i] = d,
                None => {
                    seen.insert(key, docs.len());
                    docs.push(d);
                }
            }
        }

        // 删除清单里如果又有新增，说明"删了又加回来"，新增那条应该活下来
        let deletes: Vec<(SourceId, String)> = self.deletes.drain(..).collect();

        // ---- 1. 写倒排并提交（可能重试）----
        let mut attempt = 0u32;
        loop {
            match self.apply_to_index(&docs, &deletes) {
                Ok(()) => break,
                Err(e) if commit_retryable(&e) && attempt + 1 < COMMIT_ATTEMPTS => {
                    attempt += 1;
                    // 15 / 30ms：对方那次 reload 是毫秒级的，不值得退避更久
                    std::thread::sleep(Duration::from_millis(15 * u64::from(attempt)));
                }
                Err(e) => {
                    // 一条都没提交成功：把这批原样塞回缓冲，下一次 flush 再试。
                    // 之所以能这么干，是因为水位表**还没动过** —— 重放是幂等的。
                    self.pending.extend(docs);
                    self.deletes.extend(deletes);
                    return Err(e);
                }
            }
        }

        // ---- 2. 提交成功之后才动水位表 ----
        if !deletes.is_empty() {
            // 按来源分组，避免每个 uri 一次事务
            let mut by_src: HashMap<&'static str, (SourceId, Vec<String>)> = HashMap::new();
            for (s, uri) in &deletes {
                by_src
                    .entry(s.as_str())
                    .or_insert_with(|| (*s, Vec::new()))
                    .1
                    .push(uri.clone());
            }
            for (_, (src, uris)) in by_src {
                self.meta.mark_deleted_batch(src, &uris, now)?;
            }
        }
        for d in &docs {
            self.meta.upsert(d, now)?;
        }

        Ok(docs.len())
    }

    /// 把一批变更写进倒排并提交、等合并线程收尾。失败时目录状态由 Tantivy 保证
    /// 是"要么整批没提交、要么整批提交了"，所以调用方可以整体重放。
    fn apply_to_index(&self, docs: &[Doc], deletes: &[(SourceId, String)]) -> Result<()> {
        let writer = self.index.writer(self.budget)?;

        // 先删后加
        for (_, uri) in deletes {
            self.index.delete_doc(&writer, uri)?;
        }
        for d in docs {
            // 更新同一个 uri 必须先删掉旧文档。Tantivy 没有 upsert 语义：
            // 直接 add 会往段里再塞一条，旧的那条从此变成搜得到、但水位表里
            // 已经不存在的僵尸 —— 搜索结果按 doc_id 去重会把它藏起来，
            // 于是磁盘上白占一份、查询多扫一段，没人看得出来。
            // delete_term 只作用于本次 add 之前的文档，所以"先删后加"是对的。
            self.index.delete_doc(&writer, &d.uri)?;
            self.index.add_doc(&writer, d)?;
        }

        // 提交 + 等后台合并线程停手，然后写入器自己把内存预算和目录锁还回来。
        // 注意这里传的是所有权：`commit_and_finish` 里要 `wait_merging_threads`，
        // 那个函数会消费写入器 —— 也就是说"放锁"这件事不可能早于"合并线程结束"。
        self.index.commit_and_finish(writer)?;
        Ok(())
    }

    /// 文本检索（走 parse_query，语义完整）
    pub fn search_text(&self, q: &str, k: usize) -> Result<Vec<Hit>> {
        self.index.search_text(q, k, now_secs())
    }

    pub fn search(&self, pq: &ParsedQuery, k: usize) -> Result<Vec<Hit>> {
        self.index.search(pq, k, now_secs())
    }

    pub fn parse(&self, q: &str) -> ParsedQuery {
        self.index.parse(q)
    }

    /// (存活, 墓碑, 倒排里的存活文档数, 各来源分布)
    pub fn stats(&self) -> Result<(usize, usize, usize, Vec<(String, usize)>)> {
        Ok((
            self.meta.count_live()?,
            self.meta.count_tombstone()?,
            self.index.num_docs(),
            self.meta.count_by_source()?,
        ))
    }

    /// 回收墓碑对应的倒排空间（低频调用，别放进热路径）
    pub fn merge(&mut self) -> Result<()> {
        // 合并要重写段文件，是比普通 flush 重得多的写操作 —— 更要独占
        let _lease = self.lease_for("merge")?;
        let mut writer = self.index.writer(self.budget)?;
        self.index.merge_all(&mut writer)?;
        // merge_all 里的 `.wait()` 只等我们自己发起的那个合并任务；
        // 提交之后合并策略还会自己排活。一样要等干净，否则放锁之后
        // 它还在重写段文件（见 `HubIndex::commit_and_finish` 的注释）。
        self.index.quiesce(writer)?;
        let tombs = self.meta.pending_tombstones(10_000)?;
        for (source, uri) in tombs {
            self.meta.purge(&source, &uri)?;
        }
        Ok(())
    }

    pub fn segment_count(&self) -> usize {
        self.index.segment_count()
    }

    /// 空闲维护用的"按需合并"：没有可回收的东西就什么都不做。
    ///
    /// `merge()` 无条件建写入器 —— 那会占用内存预算、拿写租约、并短暂锁住索引目录。
    /// daemon 每轮都调它显然不行，所以这里先判断两个真正触发合并的条件：
    /// 段数已经积起来了，或者有墓碑占着倒排里的空间。
    /// 返回 `true` 表示真的合并了。
    pub fn merge_if_needed(&mut self, min_segments: usize) -> Result<bool> {
        let segs = self.index.segment_count();
        let tombs = self.meta.count_tombstone()?;
        if segs < min_segments.max(2) && tombs == 0 {
            return Ok(false);
        }
        self.merge()?;
        Ok(true)
    }
}

/// 一次 flush 里最多提交几次。3 次 × 15/30ms 退避，总代价不到 50ms。
const COMMIT_ATTEMPTS: u32 = 3;

/// 提交失败值不值得再来一次。
///
/// 只认三种"环境性的、马上就好的"错误：
/// - `PermissionDenied`：Windows 上另一个进程正 mmap 着要被替换的文件（最常见）
/// - `WouldBlock`：文件被占用
/// - `LockFailure`：理论上租约在手就不会发生；真发生了说明有个不认租约的
///   旧版本进程在写，等它一下比重启整个 daemon 划算
///
/// 其它错误（磁盘满、索引损坏）重试是没有意义的，直接交出去。
fn commit_retryable(e: &crate::StoreError) -> bool {
    use crate::StoreError;
    match e {
        StoreError::Tantivy(tantivy::TantivyError::IoError(io)) => matches!(
            io.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
        ),
        StoreError::Tantivy(tantivy::TantivyError::LockFailure(..)) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_sensitive() {
        assert_eq!(hash_bytes(b"hello"), hash_bytes(b"hello"));
        assert_ne!(hash_bytes(b"hello"), hash_bytes(b"hellp"));
        assert_ne!(hash_bytes(b""), hash_bytes(b"a"));
    }
}
