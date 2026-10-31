//! Tantivy 倒排层 —— 可重建的索引缓存。
//!
//! 三个必须记住的约束：
//! 1. `search` 只接受 `&ParsedQuery`。把原始字符串交给 `QueryParser` 是错的：
//!    QueryParser 不懂「上个月」「代码片段」「-排除」，也不认中文引号。
//! 2. `Searcher` 临时借用、用完即 drop，绝不缓存 —— 否则旧段 mmap 释放不掉
//!    （Windows 上表现为段文件删不了）。
//! 3. `IndexWriter` 短命：写一批 → commit → drop。常驻 writer 会一直占着内存预算。

use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, RangeQuery, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions, Value, STORED,
    STRING,
};
use tantivy::tokenizer::{Token, TokenStream, Tokenizer};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term, doc};

use hub_core::{
    Doc, DocId, OFF_PENALTY, ParsedQuery, SourceId, TimeRange, bigram_tokens, contains_ci, parse_query,
    phrase_bonus, time_decay, word_tokens,
};
use hub_core::Dict;

use crate::{Result, StoreError};

/// 词级分词器名（必须是合法标识符，schema 里按名字引用）
pub const HUB_WORD: &str = "hub_word";
/// bigram 分词器名
pub const HUB_BIGRAM: &str = "hub_bigram";

/// bigram 通道是"兜底召回"，权重压在词级之下，避免一个词被两条通道重复计分
const BIGRAM_BOOST: f32 = 0.6;
/// 时间区间命中加权（不是硬过滤：区间外文档仍可被召回，只是排在后面）
const TIME_BOOST: f32 = 6.0;
/// 来源回退时的加权（硬过滤失败才用）
const SOURCE_BOOST: f32 = 6.0;
/// Tantivy 每个索引线程的内存预算下限（MEMORY_BUDGET_NUM_BYTES_MIN）
pub const MIN_WRITER_BUDGET: usize = 15_000_000;

// ============================================================
// 分词器：直接复用 hub-core 的算法，索引与查询共用一份词表
// ============================================================

#[derive(Clone, Copy)]
enum Mode {
    Word,
    Bigram,
}

/// 把 `hub_core::word_tokens` / `bigram_tokens` 包成 Tantivy 分词器。
///
/// 这一步是"语义一致"的物理基础。第一版索引侧用 cang-jie(jieba)、查询侧用 hub-core 词典，
/// 两套词表切出来的 term 对不上，`内存` 能建索引但查不到 —— 而且不报错，只是搜不出来。
#[derive(Clone)]
struct HubTokenizer {
    dict: Arc<Dict>,
    mode: Mode,
}

impl Tokenizer for HubTokenizer {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> VecTokenStream {
        let tokens = match self.mode {
            Mode::Word => word_tokens(text, &self.dict),
            Mode::Bigram => bigram_tokens(text),
        };
        VecTokenStream::new(tokens)
    }
}

/// 已经预先切好词的 token 流。offsets 不参与检索（不做高亮），保持 0 即可。
pub struct VecTokenStream {
    iter: std::vec::IntoIter<String>,
    token: Token,
}

impl VecTokenStream {
    fn new(tokens: Vec<String>) -> Self {
        Self {
            iter: tokens.into_iter(),
            token: Token::default(),
        }
    }
}

impl TokenStream for VecTokenStream {
    fn advance(&mut self) -> bool {
        match self.iter.next() {
            Some(t) => {
                self.token.text.clear();
                self.token.text.push_str(&t);
                // Token::default().position == usize::MAX，与官方分词器一致用 wrapping_add
                self.token.position = self.token.position.wrapping_add(1);
                self.token.position_length = 1;
                true
            }
            None => false,
        }
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

// ============================================================
// Schema
// ============================================================

pub struct Fields {
    pub id: Field,
    pub uri: Field,
    pub source: Field,
    pub title: Field,
    pub body: Field,
    /// 论文里的"bigram 通道"：整段文本的相邻二元组，负责兜住未登录词 / 型号 / OCR 碎片
    pub body_bg: Field,
    pub mtime: Field,
}

fn text_field(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::WithFreqs),
    )
}

fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();

    let id = b.add_u64_field(
        "id",
        NumericOptions::default().set_indexed().set_stored().set_fast(),
    );
    // uri 只用于删除（delete_term）与回显，必须原样存
    let uri = b.add_text_field("uri", STRING | STORED);
    // source 参与硬过滤，必须 indexed；用 raw tokenizer 保证 "files" 不被拆
    let source = b.add_text_field("source", STRING | STORED);

    let title = b.add_text_field("title", text_field(HUB_WORD).set_stored());
    let body = b.add_text_field("body", text_field(HUB_WORD).set_stored());
    let body_bg = b.add_text_field("body_bg", text_field(HUB_BIGRAM));

    let mtime = b.add_i64_field(
        "mtime",
        NumericOptions::default().set_indexed().set_stored().set_fast(),
    );

    let schema = b.build();
    (
        schema,
        Fields { id, uri, source, title, body, body_bg, mtime },
    )
}

// ============================================================
// 检索结果
// ============================================================

#[derive(Debug, Clone)]
pub struct Hit {
    pub doc_id: DocId,
    pub uri: String,
    pub source: SourceId,
    pub title: String,
    pub score: f32,
    /// unix 秒，供上层格式化"3 天前"
    pub mtime: i64,
}

// ============================================================
// HubIndex
// ============================================================

pub struct HubIndex {
    index: Index,
    fields: Fields,
    /// 长期持有的 reader；`Searcher` 必须临时借用、用完即 drop，不要缓存
    reader: IndexReader,
    dict: Arc<Dict>,
}

impl HubIndex {
    pub fn open_or_create(dir: impl AsRef<Path>) -> Result<Self> {
        Self::with_dict(dir, Dict::new())
    }

    pub fn with_dict(dir: impl AsRef<Path>, dict: Dict) -> Result<Self> {
        let dir = dir.as_ref();
        // ⚠️ MmapDirectory::open 要求目录已存在（源码里第一句就是 exists() 检查，
        // 不存在直接返回 OpenDirectoryError::DoesNotExist）。函数叫 open_or_create，
        // 但"create"只针对索引文件，不针对目录 —— 第一版漏了这一步，首次运行必挂。
        std::fs::create_dir_all(dir)?;

        let (schema, fields) = build_schema();
        let directory = MmapDirectory::open(dir)?;
        let index = Index::open_or_create(directory, schema).map_err(|e| match e {
            tantivy::TantivyError::SchemaError(msg) => StoreError::Message(format!(
                "索引 schema 与当前代码不一致（{msg}）。\n\
                 说明索引是旧版本建的。删掉索引目录重跑（或 `hub reset` 后重建），\n\
                 数据源那边有 SQLite 水位表兜底，最多重扫一遍。"
            )),
            other => StoreError::Tantivy(other),
        })?;

        let dict = Arc::new(dict);
        // 必须在任何写入/查询之前注册
        index.tokenizers().register(
            HUB_WORD,
            HubTokenizer { dict: dict.clone(), mode: Mode::Word },
        );
        index.tokenizers().register(
            HUB_BIGRAM,
            HubTokenizer { dict: dict.clone(), mode: Mode::Bigram },
        );

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        Ok(Self { index, fields, reader, dict })
    }

    pub fn fields(&self) -> &Fields {
        &self.fields
    }

    pub fn dict(&self) -> &Dict {
        &self.dict
    }

    /// 用本索引的词表解析查询。上层只应该有这一个入口，不该自己 new 一个 Dict。
    pub fn parse(&self, q: &str) -> ParsedQuery {
        parse_query(q, &self.dict)
    }

    /// 打开一个短命写入器。用完必须 drop —— 这是控制常驻内存的关键。
    ///
    /// budget 是所有索引线程共享的总预算（字节）。Tantivy 要求每线程至少 15MB，
    /// 传更小的值会直接返回 InvalidArgument，所以这里夹一下并保证单线程可用。
    pub fn writer(&self, memory_budget: usize) -> Result<IndexWriter> {
        let budget = memory_budget.max(MIN_WRITER_BUDGET);
        Ok(self.index.writer(budget)?)
    }

    pub fn add_doc(&self, writer: &IndexWriter, doc: &Doc) -> Result<()> {
        let f = &self.fields;
        let text = doc.text();
        writer.add_document(doc!(
            f.id => doc.id,
            f.uri => doc.uri.as_str(),
            f.source => doc.source.as_str(),
            f.title => doc.title.as_str(),
            f.body => doc.body.as_str(),
            f.body_bg => text,
            f.mtime => doc.mtime,
        ))?;
        Ok(())
    }

    /// 逻辑删除：commit 后立刻查询不到；磁盘空间等段合并时回收。
    pub fn delete_doc(&self, writer: &IndexWriter, uri: &str) -> Result<()> {
        writer.delete_term(Term::from_field_text(self.fields.uri, uri));
        Ok(())
    }

    /// 提交并让 reader 看到新数据。**没有这一步，写进去的文档永远不会落盘。**
    pub fn commit(&self, writer: &mut IndexWriter) -> Result<u64> {
        let opstamp = writer.commit()?;
        self.reader.reload()?;
        Ok(opstamp)
    }

    pub fn reload(&self) -> Result<()> {
        self.reader.reload()?;
        Ok(())
    }

    /// 已提交的存活文档数（含尚未合并掉的删除）。
    pub fn num_docs(&self) -> usize {
        self.reader.searcher().num_docs() as usize
    }

    /// 当前可检索的段数量。
    ///
    /// daemon 用它决定"要不要合并"：每批写入都会产生一个新段，
    /// 段数涨到几十上百时，每次查询都要逐个段打分，延迟成倍上升。
    /// 已经只有一个段时再合并纯属白烧 IO。
    pub fn segment_count(&self) -> usize {
        self.reader.searcher().segment_readers().len()
    }

    /// 手动触发段合并以回收已删文档的空间。合并是后台任务，这里等它结束。
    pub fn merge_all(&self, writer: &mut IndexWriter) -> Result<()> {
        let ids = self.index.searchable_segment_ids()?;
        if !ids.is_empty() {
            writer.merge(&ids).wait()?;
            writer.commit()?;
            self.reader.reload()?;
        }
        Ok(())
    }

    // ------------------------------------------------------------
    // 检索
    // ------------------------------------------------------------

    /// 文本查询（推荐入口）：内部走 `parse_query`，语义不会漏。
    pub fn search_text(&self, q: &str, k: usize, now: i64) -> Result<Vec<Hit>> {
        let pq = self.parse(q);
        self.search(&pq, k, now)
    }

    /// 结构化查询。
    ///
    /// 来源采用**硬过滤 + 空结果回退**：识别到来源时只返回该来源；
    /// 若硬过滤后为空（可能来源识别错了），退回软惩罚，宁可宽松也不返回空。
    pub fn search(&self, pq: &ParsedQuery, k: usize, now: i64) -> Result<Vec<Hit>> {
        let hits = self.search_once(pq, k, now, true)?;
        if hits.is_empty() && pq.source.is_some() {
            return self.search_once(pq, k, now, false);
        }
        Ok(hits)
    }

    fn search_once(
        &self,
        pq: &ParsedQuery,
        k: usize,
        now: i64,
        hard_source: bool,
    ) -> Result<Vec<Hit>> {
        let query = match self.build_query(pq, now, hard_source) {
            Some(q) => q,
            None => {
                // 没有检索词：纯时间查询（"上个月"）按最近顺序列出来
                match pq.time {
                    Some(tr) => match self.time_range(tr, now, 1.0) {
                        Some(q) => q,
                        None => return Ok(Vec::new()),
                    },
                    None => return Ok(Vec::new()),
                }
            }
        };

        // 重排窗口：时间/来源惩罚是在拿到 BM25 之后做的，窗口必须比 k 大得多，
        // 否则"区间外的高分文档"会把区间内的挤出去。
        let window = k.saturating_mul(20).clamp(100, 2000);

        let searcher = self.reader.searcher();
        let top = searcher.search(&*query, &TopDocs::with_limit(window).order_by_score())?;

        let f = &self.fields;
        let mut cands: Vec<Hit> = Vec::with_capacity(top.len());
        let mut bodies: Vec<String> = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let d: TantivyDocument = searcher.doc(addr)?;
            let uri = d
                .get_first(f.uri)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let source = d
                .get_first(f.source)
                .and_then(|v| v.as_str())
                .and_then(SourceId::parse)
                .unwrap_or(SourceId::Files);
            let title = d
                .get_first(f.title)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let body = d
                .get_first(f.body)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mtime = d.get_first(f.mtime).and_then(|v| v.as_i64()).unwrap_or(0);
            let doc_id = d.get_first(f.id).and_then(|v| v.as_u64()).unwrap_or(0);

            if !pq.excludes.is_empty() || !pq.phrases.is_empty() {
                bodies.push(format!("{title} {body}"));
            }
            cands.push(Hit { doc_id, uri, source, title, score, mtime });
        }

        // ---- 与 hub-core::index::rank 完全同口径的重排 ----
        //
        // 常数全部来自 hub-core（OFF_PENALTY / time_decay / phrase_bonus），
        // 这里不允许再定义一套 —— 两套常数就是两套语义。
        let max_bm25 = cands.iter().map(|c| c.score).fold(0.0f32, f32::max);
        let bonus = phrase_bonus(max_bm25);

        let mut out: Vec<Hit> = Vec::with_capacity(cands.len());
        for (i, mut c) in cands.into_iter().enumerate() {
            let text: Option<&String> = if bodies.is_empty() { None } else { bodies.get(i) };
            if let Some(t) = text {
                if pq.excludes.iter().any(|e| contains_ci(t, e)) {
                    continue;
                }
                let mut phrase_hits = 0usize;
                for p in &pq.phrases {
                    if contains_ci(t, p) {
                        phrase_hits += 1;
                    }
                }
                if phrase_hits > 0 {
                    c.score += phrase_hits as f32 * bonus;
                }
            }

            let days = if now > c.mtime {
                ((now - c.mtime) / 86_400) as u32
            } else {
                0
            };
            if let Some(tr) = &pq.time {
                if !tr.contains(days) {
                    c.score *= OFF_PENALTY;
                }
            }
            if let Some(src) = pq.source {
                if c.source != src {
                    // 硬过滤模式下这类文档已经被查询条件挡掉了，走不到这里；
                    // 只有回退到软惩罚时才会被执行
                    c.score *= OFF_PENALTY;
                }
            }
            c.score *= time_decay(days);
            out.push(c);
        }

        out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(k);
        Ok(out)
    }

    /// 把 ParsedQuery 翻译成 Tantivy 查询。返回 `None` 表示没有任何正向条件。
    ///
    /// ⚠️ 结构上有一处必须注意：**正向条件要包成一个只有 Should 的子查询，再整体 Must**。
    /// 如果直接把 source 过滤写成外层的 `Must`，Lucene 语义会认为"Should 变可选"，
    /// 于是同来源的所有文档都会成为候选，得 0 分也照样返回。
    /// 实测在 1.2 万条真实终端历史里，搜"交叉编译 命令"会带出 5 条毫不相关的 `systeminfo`、
    /// `pushd "%~dp0"` —— 这就是它。
    fn build_query(
        &self,
        pq: &ParsedQuery,
        now: i64,
        hard_source: bool,
    ) -> Option<Box<dyn Query>> {
        let f = &self.fields;
        let mut positive: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        // 词级通道：精确排序
        for t in self.word_terms(pq) {
            if t.is_empty() {
                continue;
            }
            positive.push((Occur::Should, Box::new(self.term(f.title, &t))));
            positive.push((Occur::Should, Box::new(self.term(f.body, &t))));
        }
        // bigram 通道：兜底召回（权重压低，避免与词级重复计分）
        for t in self.bigram_terms(pq) {
            if t.is_empty() {
                continue;
            }
            positive.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(self.term(f.body_bg, &t)),
                    BIGRAM_BOOST,
                )),
            ));
        }
        // 短语：必须能"引入"文档，而不是只给已有结果提权
        for ph in &pq.phrases {
            if let Some(q) = self.phrase_query(ph) {
                positive.push((Occur::Should, q));
            }
        }

        if positive.is_empty() {
            return None;
        }

        let mut outer: Vec<(Occur, Box<dyn Query>)> = vec![(
            Occur::Must,
            Box::new(BooleanQuery::new(positive)) as Box<dyn Query>,
        )];

        if let Some(src) = pq.source {
            let q: Box<dyn Query> = Box::new(TermQuery::new(
                Term::from_field_text(f.source, src.as_str()),
                IndexRecordOption::Basic,
            ));
            if hard_source {
                outer.push((Occur::Must, q));
            } else {
                outer.push((Occur::Should, Box::new(BoostQuery::new(q, SOURCE_BOOST))));
            }
        }

        if let Some(tr) = pq.time {
            if let Some(q) = self.time_range(tr, now, TIME_BOOST) {
                outer.push((Occur::Should, q));
            }
        }

        Some(Box::new(BooleanQuery::new(outer)))
    }

    fn term(&self, field: Field, text: &str) -> TermQuery {
        TermQuery::new(
            Term::from_field_text(field, text),
            IndexRecordOption::WithFreqs,
        )
    }

    /// 词级检索词。优先用 residue 重新切（`ParsedQuery::terms` 是词级+bigram 混在一起的），
    /// 只有在 residue 为空但 terms 非空（手工构造 AST）时才退回 terms。
    fn word_terms(&self, pq: &ParsedQuery) -> Vec<String> {
        if pq.residue.trim().is_empty() {
            return pq.terms.clone();
        }
        word_tokens(&pq.residue, &self.dict)
    }

    fn bigram_terms(&self, pq: &ParsedQuery) -> Vec<String> {
        if pq.residue.trim().is_empty() {
            return Vec::new();
        }
        bigram_tokens(&pq.residue)
    }

    /// 短语 → 用 bigram 做 Must 组合。
    ///
    /// 为什么不用 PhraseQuery：中文短语经词级分词后位置不连续（词级与 bigram 是两套坐标），
    /// 而 bigram 一定连续覆盖任意子串，用它做 AND 召回最稳。
    /// 精确性由重排阶段的 `text.contains(phrase)` 保证，这里的召回只求不漏。
    fn phrase_query(&self, phrase: &str) -> Option<Box<dyn Query>> {
        let toks = bigram_tokens(phrase);
        if toks.is_empty() {
            return None;
        }
        if toks.len() == 1 {
            return Some(Box::new(self.term(self.fields.body_bg, &toks[0])));
        }
        let must: Vec<(Occur, Box<dyn Query>)> = toks
            .iter()
            .map(|t| {
                (
                    Occur::Must,
                    Box::new(self.term(self.fields.body_bg, t)) as Box<dyn Query>,
                )
            })
            .collect();
        Some(Box::new(BooleanQuery::new(must)))
    }

    fn time_range(&self, tr: TimeRange, now: i64, boost: f32) -> Option<Box<dyn Query>> {
        let lo = now - (tr.max_days as i64) * 86_400;
        let hi = now - (tr.min_days as i64) * 86_400;
        if lo > hi {
            return None;
        }
        let q = RangeQuery::new(
            Bound::Included(Term::from_field_i64(self.fields.mtime, lo)),
            Bound::Included(Term::from_field_i64(self.fields.mtime, hi)),
        );
        if boost == 1.0 {
            Some(Box::new(q))
        } else {
            Some(Box::new(BoostQuery::new(Box::new(q), boost)))
        }
    }
}
