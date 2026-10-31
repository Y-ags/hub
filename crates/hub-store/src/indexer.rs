//! Indexer —— 把"水位表 + 倒排 + 写入器生命周期"粘起来的那一层。
//!
//! 第一版缺的就是它：`MetaStore` 和 `HubIndex` 各自写得很完整，但没有任何代码把它们串起来，
//! 于是"持久化"这件事在整条链路上并不存在（CLI 只用了内存索引）。
//!
//! 这里定了三条规则：
//! 1. **写一批 → commit → drop writer**。常驻 writer 会一直占着内存预算。
//! 2. **doc_id 由 SQLite 发放**。Tantivy 只是按这个 id 建倒排，索引可重建而 id 不变。
//! 3. **变更判定在写入前**。mtime 没变就直接跳过，连文件都不用读。

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use hub_core::{Doc, DocId, ParsedQuery, SourceId};

use crate::index::Hit;
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
    index: HubIndex,
    pending: Vec<Doc>,
    deletes: Vec<(SourceId, String)>,
    budget: usize,
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

impl Indexer {
    /// 打开（或创建）数据目录。目录结构：
    /// ```text
    /// <data>/meta.sqlite   水位表（事实源）
    /// <data>/index/        Tantivy 索引（可重建）
    /// ```
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let meta = MetaStore::open(dir.join("meta.sqlite"))?;
        let index = HubIndex::open_or_create(dir.join("index"))?;
        Ok(Self {
            meta,
            index,
            pending: Vec::new(),
            deletes: Vec::new(),
            budget: 15_000_000,
        })
    }

    /// 清空数据目录里的索引与水位表（`hub reset`）。
    ///
    /// 必须先于 `open` 调用：Windows 上被 mmap 的段文件删不掉。
    pub fn reset(dir: impl AsRef<Path>) -> Result<()> {
        let dir = dir.as_ref();
        let meta_path = dir.join("meta.sqlite");
        if meta_path.exists() {
            let meta = MetaStore::open(&meta_path)?;
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

    pub fn meta(&self) -> &MetaStore {
        &self.meta
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

    /// 把缓冲的变更写盘。返回本次提交的文档数。
    pub fn flush(&mut self) -> Result<usize> {
        if self.pending.is_empty() && self.deletes.is_empty() {
            return Ok(0);
        }
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

        let mut writer = self.index.writer(self.budget)?;

        // 先删后加：同一批里"删了又加回来"时，加的那条应该活下来
        for (source, uri) in self.deletes.drain(..) {
            self.index.delete_doc(&writer, &uri)?;
            self.meta.mark_deleted(source, &uri, now)?;
        }
        for d in &docs {
            self.index.add_doc(&writer, d)?;
            self.meta.upsert(d, now)?;
        }

        self.index.commit(&mut writer)?;
        drop(writer); // 释放内存预算与目录锁

        Ok(docs.len())
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
        let mut writer = self.index.writer(self.budget)?;
        self.index.merge_all(&mut writer)?;
        let tombs = self.meta.pending_tombstones(10_000)?;
        for (source, uri) in tombs {
            self.meta.purge(&source, &uri)?;
        }
        Ok(())
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
