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
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

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
    pub fn likely_unchanged(&self, source: SourceId, uri: &str, mtime: i64) -> Result<bool> {
        match self.lookup(source, uri)? {
            None => Ok(false),
            Some(row) => Ok(row.live && row.mtime == mtime),
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
