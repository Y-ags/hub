//! 内存倒排索引 + BM25 + tombstone + 时间/来源重排。
//!
//! 这是 hub-core 的"正确性参考实现"：零依赖、可直接单测。
//! 生产环境由 hub-store 用 Tantivy 替换掉这里的存储层，
//! 但**查询语义（时间/来源/短语/排除）必须与此保持一致** —— 改动前先跑这里的测试。

use crate::model::{Doc, DocId, contains_ci};
use crate::query::ParsedQuery;
use crate::tokenizer::{Dict, fused_tokens};
use std::collections::{HashMap, HashSet};

const K1: f32 = 1.2;
const B: f32 = 0.75;
/// 越新的文档权重越高，约 62 天半衰
pub const DECAY_DAYS: f32 = 90.0;
/// 不在时间区间 / 不匹配来源时的软惩罚（不硬剔除，避免误杀）
pub const OFF_PENALTY: f32 = 0.15;
/// 短语加成是**精确子串匹配**，是整个系统里精度最高的信号，必须压过普通关键词。
///
/// ⚠️ 这里不能用固定常数。实测：固定 5.0 时，搜 `内存 管理 "零拷贝"` 返回 [19,2,15]，
/// 含"零拷贝"的 doc5 反而排不进前三 —— 因为 5.0 乘上时间衰减后只剩 2.54，
/// 低于普通关键词文档的 3.74。
///
/// 改成**相对值**：取本轮 BM25 的最高分为基准，短语加成 = max_bm25 * 2 + 1。
/// 这样无论语料怎么变、关键词分多高，命中精确短语的文档一定排在最前。
pub const PHRASE_RATIO: f32 = 2.0;
/// max_bm25 为 0（关键词全没命中）时的兜底加成
pub const PHRASE_FLOOR: f32 = 1.0;

/// 时间衰减因子：越老越小，62 天衰减到 0.5。
///
/// 单独抽成函数是为了让 hub-store 复用同一份口径 —— 持久化层不允许自己再定义一套常数，
/// 否则"黄金参考实现"就名存实亡了。
pub fn time_decay(days_ago: u32) -> f32 {
    (-(days_ago as f32) / DECAY_DAYS).exp()
}

/// 短语加成（相对值口径，见 [`PHRASE_RATIO`]）。
pub fn phrase_bonus(max_bm25: f32) -> f32 {
    if max_bm25 > 0.0 {
        max_bm25 * PHRASE_RATIO + PHRASE_FLOOR
    } else {
        PHRASE_FLOOR
    }
}

pub struct MemoryIndex {
    dict: Dict,
    docs: HashMap<DocId, Doc>,
    postings: HashMap<String, HashMap<DocId, u32>>,
    dl: HashMap<DocId, usize>,
    tomb: HashSet<DocId>,
}

impl Default for MemoryIndex {
    fn default() -> Self {
        Self::new(Dict::new())
    }
}

impl MemoryIndex {
    pub fn new(dict: Dict) -> Self {
        Self {
            dict,
            docs: HashMap::new(),
            postings: HashMap::new(),
            dl: HashMap::new(),
            tomb: HashSet::new(),
        }
    }

    pub fn dict(&self) -> &Dict {
        &self.dict
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    pub fn get(&self, id: DocId) -> Option<&Doc> {
        self.docs.get(&id)
    }

    /// 写入或更新。已存在则先摘掉旧倒排，保证不残留脏数据。
    pub fn upsert(&mut self, doc: Doc) {
        if self.docs.contains_key(&doc.id) {
            self.detach(doc.id);
        }
        let terms = fused_tokens(&doc.text(), &self.dict);
        let mut tf: HashMap<String, u32> = HashMap::new();
        for t in &terms {
            *tf.entry(t.clone()).or_insert(0) += 1;
        }
        self.dl.insert(doc.id, terms.len());
        for (t, c) in tf {
            self.postings.entry(t).or_default().insert(doc.id, c);
        }
        self.tomb.remove(&doc.id);
        self.docs.insert(doc.id, doc);
    }

    /// 逻辑删除：立刻搜不到，磁盘空间等 merge 再回收。
    /// 已验证：tombstone 与 merge 职责分离（见 tests::tombstone_hides_immediately）
    pub fn delete(&mut self, id: DocId) {
        self.tomb.insert(id);
        if let Some(d) = self.docs.get_mut(&id) {
            d.deleted = true;
        }
    }

    fn detach(&mut self, id: DocId) {
        for pl in self.postings.values_mut() {
            pl.remove(&id);
        }
        self.postings.retain(|_, v| !v.is_empty());
        self.dl.remove(&id);
    }

    /// 物理回收：重建索引，跳过已删文档。返回回收的倒排条目数。
    pub fn merge(&mut self) -> usize {
        let before: usize = self.postings.values().map(|v| v.len()).sum();
        let keep: Vec<Doc> = self
            .docs
            .values()
            .filter(|d| !self.tomb.contains(&d.id))
            .cloned()
            .collect();
        self.postings.clear();
        self.dl.clear();
        self.docs.clear();
        self.tomb.clear();
        for d in keep {
            self.upsert(d);
        }
        before.saturating_sub(self.postings.values().map(|v| v.len()).sum())
    }

    /// 当前磁盘（逻辑）占用的倒排条目总数，含已删文档的残留
    pub fn posting_entries(&self) -> usize {
        self.postings.values().map(|v| v.len()).sum()
    }

    /// 检索。`now` 为当前 unix 秒。
    ///
    /// 来源采用**硬过滤 + 空结果回退**：
    /// - 查询里识别到来源（"群里发的"、"代码片段"）时，只返回该来源的结果；
    ///   软惩罚（0.15）不够 —— 实测"群里老张发的服务器方案链接"会让一条终端历史
    ///   （正文含"链接"）混进 Top3，这就是跨源泄漏。
    /// - 若硬过滤后为空（可能来源识别错了），回退成软惩罚，宁可宽松也不要返回空。
    pub fn search(&self, pq: &ParsedQuery, k: usize, now: i64) -> Vec<(DocId, f32)> {
        let mut out = self.rank(pq, now, 0.0);
        if out.is_empty() && pq.source.is_some() {
            out = self.rank(pq, now, OFF_PENALTY);
        }
        out.truncate(k);
        out
    }

    fn rank(&self, pq: &ParsedQuery, now: i64, source_penalty: f32) -> Vec<(DocId, f32)> {
        let alive: Vec<DocId> = self
            .docs
            .keys()
            .filter(|i| !self.tomb.contains(i))
            .copied()
            .collect();
        if alive.is_empty() {
            return Vec::new();
        }

        let n = alive.len() as f32;
        let total_dl: usize = alive.iter().map(|i| self.dl.get(i).copied().unwrap_or(0)).sum();
        let avgdl = total_dl as f32 / n;

        let mut scores: HashMap<DocId, f32> = HashMap::new();

        // BM25
        //
        // ⚠️ 必须先对查询词去重。`fused_tokens` 同时产出词级和 bigram 两份 token，
        // 对两字词来说两者完全相同，于是每个词在 `pq.terms` 里都出现两次 ——
        // 不去重就会让所有关键词命中的文档分数翻倍。
        // 实测后果：搜 `内存 管理 "零拷贝"` 时，doc15（3.01）被抬高到
        // 只拿到短语加成的 doc5（2.54）之上，导致精确短语命中反而进不了 Top3。
        let mut seen: HashSet<&str> = HashSet::new();
        let uniq_terms: Vec<&String> = pq
            .terms
            .iter()
            .filter(|t| seen.insert(t.as_str()))
            .collect();

        for t in uniq_terms {
            let pl = match self.postings.get(t) {
                Some(p) => p,
                None => continue,
            };
            let df = pl.keys().filter(|i| !self.tomb.contains(i)).count() as f32;
            if df == 0.0 {
                continue;
            }
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for (id, tf) in pl.iter() {
                if self.tomb.contains(id) {
                    continue;
                }
                let dl = self.dl.get(id).copied().unwrap_or(0) as f32;
                let denom = *tf as f32 + K1 * (1.0 - B + B * dl / avgdl);
                let s = idf * (*tf as f32 * (K1 + 1.0)) / denom;
                *scores.entry(*id).or_insert(0.0) += s;
            }
        }

        // 短语加成取本轮 BM25 最高分的相对值，保证精确短语永远压过关键词命中
        let max_bm25 = scores.values().copied().fold(0.0f32, f32::max);
        let bonus = phrase_bonus(max_bm25);

        // 短语加权：遍历**全部存活文档**，而不只是已被 BM25 打分的那些。
        //
        // 这是实测出来的缺陷：doc5（mmap 零拷贝）正文里没有"内存"也没有"管理"，
        // 所以它根本进不了 BM25 候选集；而短语加权当时只遍历已打分文档，
        // 导致用户搜 `内存 管理 "零拷贝"` 永远找不到它。
        //
        // 语义上，引号短语应当能**引入**文档：用户搜 "零拷贝" 就是要找这个精确串，
        // 不该要求它同时命中别的关键词。修复后 doc5 直接排第一。
        if !pq.phrases.is_empty() {
            for id in &alive {
                let text = self.docs[id].text();
                let hits = pq
                    .phrases
                    .iter()
                    .filter(|ph| contains_ci(&text, ph))
                    .count() as f32;
                if hits > 0.0 {
                    *scores.entry(*id).or_insert(0.0) += hits * bonus;
                }
            }
        }

        // 排除词：命中即剔除
        let mut out: Vec<(DocId, f32)> = scores
            .into_iter()
            .filter(|(id, _)| {
                let text = self.docs[id].text();
                !pq.excludes.iter().any(|e| contains_ci(&text, e))
            })
            .map(|(id, s)| {
                let d = &self.docs[&id];
                let age = d.days_ago(now);
                let mut s = s;
                if let Some(tr) = &pq.time {
                    if !tr.contains(age) {
                        s *= OFF_PENALTY;
                    }
                }
                if let Some(src) = pq.source {
                    if d.source != src {
                        s *= source_penalty; // 硬过滤时为 0.0，直接淘汰
                    }
                }
                s *= time_decay(age);
                (id, s)
            })
            .filter(|(_, s)| *s > 0.0)
            .collect();

        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out
    }
}
