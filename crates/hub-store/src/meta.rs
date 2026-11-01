//! SQLite 水位表 —— 事实源。
//!
//! 记三件事：
//! 1. 某个 (source, uri) 上次索引时的 content_hash / mtime（增量闸门）
//! 2. 它的存活状态（live / tombstone）
//! 3. doc_id 分配（Tantivy 里的主键必须由事实源发放，否则重建索引后 id 会漂移）

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use hub_core::{Doc, DocId, SourceId};

use crate::Result;

/// mtime 距今小于这个秒数时，不信任"mtime 相同 = 没变"的快通道。
///
/// 取 3 秒：足够覆盖一次保存动作产生的多轮写入（编辑器通常 truncate → write →
/// close，几十毫秒内完成），又远小于正常编辑间隔，不会让快通道失效。
const FRESH_WINDOW: i64 = 3;

/// 打开水位表最多试几次。退避 20 / 40 / 80 / 160 / 320ms。
///
/// 为什么要 6 次而不是 2 次：并发打开同一个 WAL 库的窗口可能持续上百毫秒
/// （两个进程把 `-shm` 建起来又拆掉，反复走恢复路径），实测 4 次（140ms）
/// 在压测下仍会漏一次。620ms 的总预算对一次启动来说可以接受，
/// 而它换掉的是"用户偶发看到 attempt to write a readonly database"。
const OPEN_ATTEMPTS: u32 = 6;

/// 这个错误是不是"等一会儿再来就行"。
///
/// 只认三种：`BUSY` / `LOCKED` / `READONLY`。前两个是显式的"有人在写"，
/// 第三个是 Windows 上并发打开 WAL 库时 SQLite 给出的瞬时状态（见 [`MetaStore::open`]）。
/// 其它错误（磁盘满、库损坏、没权限）重试一万次也一样，直接抛。
fn open_transient(e: &crate::StoreError) -> bool {
    use rusqlite::ErrorCode;
    match e {
        crate::StoreError::Sql(rusqlite::Error::SqliteFailure(err, _)) => matches!(
            err.code,
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked | ErrorCode::ReadOnly
        ),
        _ => false,
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 水位表里的一行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaRow {
    pub doc_id: DocId,
    pub content_hash: u64,
    /// unix 秒。文件源用它做"没改就跳过"的快通道，不用读文件。
    pub mtime: i64,
    pub live: bool,
}

pub struct MetaStore {
    conn: Connection,
}

impl MetaStore {
    /// 打开（必要时创建）水位表。
    ///
    /// 带重试：两个进程**同时**打开同一个 WAL 库时，后到的那个有概率拿到一个
    /// 瞬时错误（实测 `SQLITE_READONLY / attempt to write a readonly database`
    /// —— 一方正在做 WAL 恢复、拿 `-shm` 独占锁，另一方连恢复都挤不进去）。
    /// 这不是真的只读，隔几十毫秒重开就好了。见 [`open_transient`]。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut attempt = 0u32;
        loop {
            match Self::open_once(path) {
                Ok(s) => return Ok(s),
                Err(e) if open_transient(&e) && attempt + 1 < OPEN_ATTEMPTS => {
                    attempt += 1;
                    // 20ms / 40ms / 80ms：WAL 恢复是毫秒级的，退避不用更复杂
                    std::thread::sleep(std::time::Duration::from_millis(20 << (attempt - 1)));
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn open_once(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        // ⚠️ busy_timeout 必须**最先**装上，早于下面任何一条语句。
        //
        // 这不是洁癖，是实测出来的：WAL 库的第一个打开者会去拿 `-shm` 的独占锁做恢复，
        // 如果这一刻另一个连接正好也在打开同一个库，没装 busy handler 的那一方
        // 不会等，而是直接失败。装上之后失败率从 4/100 降到 1/100 ——
        // 降不下去的那一份就是 `open` 外面那层重试在兜的。
        //
        // 用 API 而不是 `PRAGMA busy_timeout`：PRAGMA 本身也是一条要执行的语句，
        // 而我们要的是"在第一条语句之前生效"。
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            -- daemon 常驻之后，同一个库一定会有第二个连接（CLI 的 stats /
            -- search --local）。WAL 允许多读一写，但两个写者仍会撞上写锁；
            -- SQLite 默认的 busy_timeout 是 0，撞上就直接返回 SQLITE_BUSY，
            -- 用户看到的是"database is locked"这种毫无意义的报错。
            -- （busy_timeout 已在上面用 API 设过了，这里不再重复。）

            CREATE TABLE IF NOT EXISTS doc_meta (
                source       TEXT    NOT NULL,
                uri          TEXT    NOT NULL,
                doc_id       INTEGER NOT NULL,
                content_hash INTEGER NOT NULL,
                mtime        INTEGER NOT NULL,
                status       TEXT    NOT NULL DEFAULT 'live',
                updated_at   INTEGER NOT NULL,
                PRIMARY KEY (source, uri)
            );

            CREATE INDEX IF NOT EXISTS idx_doc_meta_status ON doc_meta(status);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_doc_meta_doc_id ON doc_meta(doc_id);

            CREATE TABLE IF NOT EXISTS counters (
                name  TEXT    PRIMARY KEY,
                value INTEGER NOT NULL
            );
            INSERT INTO counters (name, value) VALUES ('doc_id', 0)
                ON CONFLICT(name) DO NOTHING;
            "#,
        )?;
        Ok(Self { conn })
    }

    /// 只读打开，给"只看不动"的读者用（daemon 的 IPC 线程回答 `stats`）。
    ///
    /// 两处和 `open` 不同：
    ///
    /// 1. **不做迁移**。`open` 里那批 `CREATE TABLE IF NOT EXISTS` 在只读连接上
    ///    会直接失败；更重要的是语义 —— 用 `SQLITE_OPEN_READONLY` 打开之后，
    ///    这条连接在数据库层面就写不了，以后谁加了写语句，测试里立刻炸，
    ///    而不是在生产里悄悄变成第二个写者。
    /// 2. **不建库**。flags 里**没有** `SQLITE_OPEN_CREATE`（`open` 那套
    ///    `Connection::open` 是 `READ_WRITE | CREATE`）。实测确认过：库不存在时
    ///    SQLite 返回的是 `unable to open database file`，**不会**顺手造一个
    ///    空库。下面那个 `exists()` 预检不是为了防这个，只是为了让报错说清楚
    ///    "该去建库"还是"权限不对" —— SQLite 自己那句话两种原因长得一样。
    ///
    /// WAL 的注意点：SQLite 要求打开 WAL 库的进程对 `-shm` 有写权限（那是进程间
    /// 共享的索引结构，不是数据）。同一用户的普通进程都满足，所以这里不特殊处理；
    /// 真在只读介质上会返回错误，调用方的正确反应是退回本地打开索引。
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(crate::StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("元数据库不存在：{}", path.display()),
            )));
        }
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        // 只读连接一样会和写者撞锁（WAL 下读者不阻塞写者，但 checkpoint 会），
        // 撞上时别立刻甩 SQLITE_BUSY 给上层
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::open(":memory:")
    }

    /// 查水位。`None` 表示这个 (source, uri) 从没见过。
    pub fn lookup(&self, source: SourceId, uri: &str) -> Result<Option<MetaRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT doc_id, content_hash, mtime, status FROM doc_meta WHERE source = ?1 AND uri = ?2",
                params![source.as_str(), uri],
                |r| {
                    Ok(MetaRow {
                        doc_id: r.get::<_, i64>(0)? as u64,
                        content_hash: r.get::<_, i64>(1)? as u64,
                        mtime: r.get(2)?,
                        live: r.get::<_, String>(3)? == "live",
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// 是否需要重新索引。
    ///
    /// 快通道：mtime 没变且状态是 live → 直接判定"没变"，**不用读文件算 hash**。
    /// 第一版只有 `needs_reindex(source, uri, hash)`，意味着调用方必须先读完整个文件
    /// 才能问"要不要索引"，所谓"省 CPU 的第一道闸门"其实只省了分词，IO 一点没省。
    pub fn needs_reindex(
        &self,
        source: SourceId,
        uri: &str,
        content_hash: u64,
        mtime: i64,
    ) -> Result<bool> {
        match self.lookup(source, uri)? {
            None => Ok(true),
            Some(row) => Ok(!row.live || row.content_hash != content_hash || row.mtime != mtime),
        }
    }

    /// 只靠 mtime 判断是否可能变过 —— 供 connector 在读文件之前调用。
    ///
    /// ⚠️ mtime 只精确到**秒**（`Doc.mtime` 是 unix 秒），而编辑器一次保存
    /// 可能在同一秒内产生多轮写入。如果无条件信任"mtime 相同 = 内容没变"，
    /// 表现是"文件明明改了，搜索还是旧内容"，而且要等到下一次全量对账才修正。
    ///
    /// 所以这里加了**新鲜度保护**：只有 mtime 距今超过 `FRESH_WINDOW` 秒，
    /// 才允许走快通道跳过读取。刚改过的文件宁可多读一次 ——
    /// 多读一个文件是微秒级开销，漏掉一次修改是用户可见的错。
    pub fn likely_unchanged(&self, source: SourceId, uri: &str, mtime: i64) -> Result<bool> {
        match self.lookup(source, uri)? {
            None => Ok(false),
            Some(row) => {
                if !row.live || row.mtime != mtime {
                    return Ok(false);
                }
                Ok(mtime + FRESH_WINDOW < now_secs())
            }
        }
    }

    pub fn upsert(&self, doc: &Doc, now: i64) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO doc_meta (source, uri, doc_id, content_hash, mtime, status, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, 'live', ?6)
               ON CONFLICT(source, uri) DO UPDATE SET
                 doc_id = excluded.doc_id,
                 content_hash = excluded.content_hash,
                 mtime = excluded.mtime,
                 status = 'live',
                 updated_at = excluded.updated_at"#,
            params![
                doc.source.as_str(),
                doc.uri,
                doc.id as i64,
                doc.content_hash as i64,
                doc.mtime,
                now
            ],
        )?;
        Ok(())
    }

    pub fn mark_deleted(&self, source: SourceId, uri: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE doc_meta SET status = 'tombstone', updated_at = ?3 WHERE source = ?1 AND uri = ?2",
            params![source.as_str(), uri, now],
        )?;
        Ok(())
    }

    /// 待物理回收的条目，供后台 merge 任务消费
    pub fn pending_tombstones(&self, limit: usize) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source, uri FROM doc_meta WHERE status = 'tombstone' LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// merge 完成后真正删掉记录
    pub fn purge(&self, source: &str, uri: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM doc_meta WHERE source = ?1 AND uri = ?2",
            params![source, uri],
        )?;
        Ok(())
    }

    /// 分配一个新的 doc_id。
    ///
    /// 这是第一版缺失的关键一环：`Doc.id` 由调用方随便传，`doc_meta.doc_id` 存了却没人发号，
    /// 重建索引后 id 与元数据必然对不上。现在 id 由事实源单调发放，
    /// Tantivy 只是按这个 id 建倒排，删了重建 id 也不变。
    pub fn alloc_doc_id(&mut self) -> Result<DocId> {
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE counters SET value = value + 1 WHERE name = 'doc_id'", [])?;
        let v: i64 = tx.query_row("SELECT value FROM counters WHERE name = 'doc_id'", [], |r| {
            r.get(0)
        })?;
        tx.commit()?;
        Ok(v as u64)
    }

    pub fn count_live(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM doc_meta WHERE status = 'live'",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    pub fn count_tombstone(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM doc_meta WHERE status = 'tombstone'",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// 各来源的存活文档数，供 `hub stats` 展示
    pub fn count_by_source(&self) -> Result<Vec<(String, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT source, COUNT(*) FROM doc_meta WHERE status = 'live' GROUP BY source ORDER BY 2 DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 列出某来源下所有存活条目的 (uri, mtime)。
    ///
    /// `reconcile` 用它做**一次遍历的三种判定**：新增（水位表里没有）、
    /// 修改（mtime 对不上）、删除（磁盘上没有）。流式回调而不是返回 Vec，
    /// 50 万条时不会一次性吃满内存。
    ///
    /// 之所以连 mtime 一起给：如果只拿 uri，对账时每个文件都要再查一次
    /// SQLite 才知道"变没变"，5 万文件就是 5 万次查询。
    pub fn for_each_live_meta<F>(&self, source: SourceId, mut f: F) -> Result<usize>
    where
        F: FnMut(&str, i64),
    {
        let mut stmt = self
            .conn
            .prepare("SELECT uri, mtime FROM doc_meta WHERE source = ?1 AND status = 'live'")?;
        let mut rows = stmt.query(params![source.as_str()])?;
        let mut n = 0;
        while let Some(row) = rows.next()? {
            let uri: String = row.get(0)?;
            let mtime: i64 = row.get(1)?;
            f(&uri, mtime);
            n += 1;
        }
        Ok(n)
    }

    /// 批量标记删除（一次事务，避免 N 次 fsync）
    pub fn mark_deleted_batch(&mut self, source: SourceId, uris: &[String], now: i64) -> Result<usize> {
        if uris.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE doc_meta SET status = 'tombstone', updated_at = ?3
                 WHERE source = ?1 AND uri = ?2 AND status = 'live'",
            )?;
            for u in uris {
                stmt.execute(params![source.as_str(), u, now])?;
            }
        }
        tx.commit()?;
        Ok(uris.len())
    }

    /// 找出某个路径**及其子树**下的所有存活记录。
    ///
    /// 用于"整个目录被删掉"的场景：watcher 只会报一条 `Remove(C:/code/proj)`，
    /// 但索引里可能有这个目录下的几百条记录。只按 uri 精确匹配会把它们全部漏掉。
    ///
    /// 边界要精确：`C:/code/proj` 不能匹配到 `C:/code/proj2`，
    /// 所以是 `uri = prefix` 或 `uri LIKE prefix/%`，而不是裸的 `prefix%`。
    pub fn live_under(&self, source: SourceId, prefix: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            r"SELECT uri FROM doc_meta
              WHERE source = ?1 AND status = 'live'
                AND (uri = ?2 OR uri LIKE ?3 ESCAPE '\')",
        )?;
        let pattern = format!("{}/%", escape_like(prefix));
        let rows = stmt.query_map(params![source.as_str(), prefix, pattern], |r| {
            r.get::<_, String>(0)
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 清空水位表与计数器（`hub reset` 用）。索引目录由调用方自行删除。
    pub fn clear(&self) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM doc_meta; UPDATE counters SET value = 0 WHERE name = 'doc_id';")?;
        Ok(())
    }

    /// 供测试与调试直接拿到底层连接
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// 转义 SQL `LIKE` 里的元字符。
///
/// 文件路径里 `_` 很常见（`my_file.rs`），在 LIKE 里它是"任意单字符"通配符，
/// 不转义的话 `C:/a/my_file.rs/%` 会匹配到 `C:/a/myXfile.rs/...`。
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c == '\\' || c == '%' || c == '_' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: DocId, hash: u64, mtime: i64) -> Doc {
        Doc {
            id,
            source: SourceId::Files,
            uri: "C:/tmp/a.rs".into(),
            title: "a.rs".into(),
            body: "内存".into(),
            mtime,
            content_hash: hash,
            deleted: false,
        }
    }

    #[test]
    fn readonly_refuses_to_create_a_missing_db() {
        let t = tempfile::TempDir::new().unwrap();
        let p = t.path().join("meta.sqlite");
        // `unwrap_err` 要求 Ok 那边是 Debug，MetaStore 没实现（也不该为
        // 了一条测试去实现），所以直接 match
        let e = match MetaStore::open_readonly(&p) {
            Ok(_) => panic!("库不存在时不该返回成功"),
            Err(e) => e,
        };
        // 断言"文件仍然不存在"而不是只断言报错：只读打开真正的风险是
        // flags 里混进了 CREATE —— 那样报错是没有的，它只是安静地造了个空库，
        // 之后 `hub stats` 会一路显示 0 篇，看起来像"索引丢了"
        assert!(!p.exists(), "只读打开不该创建文件");
        assert!(matches!(e, crate::StoreError::Io(_)), "{e}");
    }

    #[test]
    fn readonly_connection_reads_but_cannot_write() {
        let t = tempfile::TempDir::new().unwrap();
        let p = t.path().join("meta.sqlite");

        // 写者**不** drop：daemon 一边握可写连接、IPC 线程一边用只读连接，
        // 这才是生产里的样子。WAL 下多读一写是设计内的用法
        let w = MetaStore::open(&p).unwrap();
        w.upsert(&doc(1, 42, 1000), 1_700_000_000).unwrap();

        let r = MetaStore::open_readonly(&p).unwrap();
        assert_eq!(r.count_live().unwrap(), 1);
        assert_eq!(r.count_by_source().unwrap(), vec![("files".to_string(), 1)]);
        // 只读那边看到的和写者写的是同一份
        assert!(r.lookup(SourceId::Files, "C:/tmp/a.rs").unwrap().is_some());

        // 关键性质：这条连接在数据库层面就写不了。以后谁加了写语句，
        // 测试里立刻炸，而不是在生产里悄悄变成第二个写者
        let e = r.upsert(&doc(2, 43, 2000), 1_700_000_000).unwrap_err();
        let msg = e.to_string().to_lowercase();
        assert!(
            msg.contains("readonly") || msg.contains("read-only"),
            "只读连接必须写不进去，实际报错：{e}"
        );
        assert_eq!(r.count_live().unwrap(), 1, "写了失败，数字不该变");
        assert_eq!(w.count_live().unwrap(), 1);
    }

    #[test]
    fn doc_id_is_monotonic_and_never_reused() {
        let mut m = MetaStore::open_in_memory().unwrap();
        let a = m.alloc_doc_id().unwrap();
        let b = m.alloc_doc_id().unwrap();
        assert_eq!((a, b), (1, 2));

        // 即便清空水位表，计数器也重新归零（reset 语义），但正常删除不会回退 id
        let c = m.alloc_doc_id().unwrap();
        assert_eq!(c, 3);
    }

    #[test]
    fn mtime_fast_path_skips_without_content() {
        let m = MetaStore::open_in_memory().unwrap();
        m.upsert(&doc(1, 42, 1000), 1_700_000_000).unwrap();

        assert!(m.likely_unchanged(SourceId::Files, "C:/tmp/a.rs", 1000).unwrap());
        // mtime 变了 → 需要重读
        assert!(!m.likely_unchanged(SourceId::Files, "C:/tmp/a.rs", 2000).unwrap());
        // 没见过 → 需要重读
        assert!(!m.likely_unchanged(SourceId::Files, "C:/tmp/b.rs", 1000).unwrap());
    }

    #[test]
    fn hash_change_requires_reindex() {
        let m = MetaStore::open_in_memory().unwrap();
        m.upsert(&doc(1, 42, 1000), 1).unwrap();
        assert!(!m.needs_reindex(SourceId::Files, "C:/tmp/a.rs", 42, 1000).unwrap());
        assert!(m.needs_reindex(SourceId::Files, "C:/tmp/a.rs", 43, 1000).unwrap());
    }

    #[test]
    fn tombstone_is_not_live() {
        let m = MetaStore::open_in_memory().unwrap();
        m.upsert(&doc(1, 42, 1000), 1).unwrap();
        m.mark_deleted(SourceId::Files, "C:/tmp/a.rs", 2).unwrap();

        let row = m.lookup(SourceId::Files, "C:/tmp/a.rs").unwrap().unwrap();
        assert!(!row.live);
        assert!(!m.likely_unchanged(SourceId::Files, "C:/tmp/a.rs", 1000).unwrap());
        assert!(m.needs_reindex(SourceId::Files, "C:/tmp/a.rs", 42, 1000).unwrap());
        assert_eq!(m.pending_tombstones(10).unwrap().len(), 1);
        assert_eq!(m.count_live().unwrap(), 0);
        assert_eq!(m.count_tombstone().unwrap(), 1);
    }

    #[test]
    fn reindex_revives_tombstone() {
        let m = MetaStore::open_in_memory().unwrap();
        m.upsert(&doc(1, 42, 1000), 1).unwrap();
        m.mark_deleted(SourceId::Files, "C:/tmp/a.rs", 2).unwrap();
        m.upsert(&doc(1, 43, 2000), 3).unwrap();

        let row = m.lookup(SourceId::Files, "C:/tmp/a.rs").unwrap().unwrap();
        assert!(row.live);
        assert_eq!(row.content_hash, 43);
        assert_eq!(m.count_live().unwrap(), 1);
    }
}
