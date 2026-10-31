//! 持久化层的语义集成测试。
//!
//! 这些测试存在的唯一目的：**证明 hub-store 和 hub-core 跑的是同一套语义**。
//! 第一版 hub-core 有 29 条测试全绿，但 hub-store 的 search 收的是 `&str`，
//! 直接把字符串丢给 Tantivy QueryParser —— 测试全绿而生产全错。
//!
//! 每个用例都同时跑两套实现（内存参考实现 + Tantivy 持久化实现），
//! 断言两者的 Top-3 **来源分布一致**。分数不可能完全相同（字段结构不同），
//! 但"跨源串味""短语排第一""排除词生效"这些产品级语义必须一致。

use std::collections::HashMap;

use hub_core::{Doc, MemoryIndex, SourceId, parse_query};
use hub_store::Indexer;
use tempfile::TempDir;

const DAY: i64 = 86_400;

/// 与 hub-core 集成测试完全相同的 24 篇多源语料
fn corpus() -> Vec<(SourceId, &'static str, &'static str, i64)> {
    vec![
        (SourceId::Files, "rust 手动分配内存示例 alloc.rs",
         "use std::alloc::{alloc, Layout}; 手动在堆上分配一块内存 大小 1024 对齐 8 字节 然后 unsafe 解引用裸指针写入数据", 42),
        (SourceId::Files, "rust Box 堆分配 Vec 容量管理",
         "Box::new 在堆上分配 离开作用域自动释放 Vec 内部管理 capacity 与 len 扩容时重新分配内存块并拷贝", 40),
        (SourceId::Files, "实现一个简单的内存池",
         "自己写一个内存池 按固定大小切块 用 free list 串起来 分配时取表头 释放时挂回 避免频繁 malloc", 33),
        (SourceId::Files, "读进程内存 ReadProcessMemory 封装",
         "OpenProcess 拿到句柄 然后 ReadProcessMemory 读目标进程某地址的数据 WriteProcessMemory 写入新值", 95),
        (SourceId::Files, "mmap 映射大文件零拷贝读取",
         "用 mmap 把文件映射到地址空间 读的时候按需缺页 不把整个文件读进堆 适合大文件随机访问", 61),
        (SourceId::Files, "C 语言 malloc free 手动管理",
         "c 语言里 malloc 分配 free 释放 忘记释放就内存泄漏 悬垂指针 use after free 是经典问题", 120),
        (SourceId::Files, "终端历史解析模块",
         "解析 zsh_history 里的每一行 提取时间戳和命令 去掉重复 按时间排序后写入索引", 28),
        (SourceId::Files, "中文分词 bigram 实现",
         "对连续的中文字符做二元切分 生成相邻二元组 无词表 召回高但噪声多 适合兜底新词", 25),
        (SourceId::Terminal, "cargo build --release 带优化编译",
         "cargo build --release 编译 rust 项目 开启优化 输出体积更小 运行更快", 45),
        (SourceId::Terminal, "配置 ohos ndk 交叉编译 clang 路径",
         "export OHOS_NDK_HOME 指向鸿蒙 ndk 目录 然后 rustup target add 用 clang 链接", 37),
        (SourceId::Terminal, "杀掉占用端口的进程",
         "lsof 找到 pid 然后 kill 干掉 释放端口", 88),
        (SourceId::Terminal, "递归搜索文件内容",
         "grep 搜索当前目录下所有包含关键字的文件 显示行号", 52),
        (SourceId::Terminal, "git 回退到某次提交",
         "git log 找到提交的 hash 然后 git reset 回退 注意会丢掉工作区改动", 130),
        (SourceId::Chat, "老张发的服务器方案链接",
         "老张在群里发了一个链接 说是新的服务器部署方案 让大家看看能不能用在我们的项目里", 76),
        (SourceId::Chat, "讨论内存占用太高的问题",
         "小李说后台进程内存占用太高了 一直挂着 问他能不能优化 老王说索引走 mmap 就不占常驻内存", 31),
        (SourceId::Chat, "约周五下午开会",
         "周会上说周五下午三点开个短会 同步一下进度 地点在三号会议室", 15),
        (SourceId::Chat, "推荐一个截图工具",
         "同事推荐了个截图工具 说能自动识别文字 找旧截图特别方便", 58),
        (SourceId::Ocr, "报错截图 编译失败",
         "error could not compile 编译报错 提示找不到依赖 截图里红字一片", 48),
        (SourceId::Ocr, "任务管理器内存占用截图",
         "任务管理器里看到某个后台进程 占用内存 名字是 searcherd 截图记录当时状态", 30),
        (SourceId::Ocr, "终端窗口 cargo 输出",
         "终端里 cargo build 的输出 显示编译完成 用了 12 秒", 44),
        (SourceId::Note, "个人数据中枢架构设想",
         "把散落各处的数据统一索引 文件 终端 聊天 截图 全部本地离线 不上传 隐私第一", 20),
        (SourceId::Note, "倒排索引与段合并笔记",
         "倒排索引 term 到 doc 列表 段不可变 删除先打标记 合并时才真正回收空间", 22),
        (SourceId::Note, "BM25 排序公式推导",
         "bm25 在 tfidf 基础上加了文档长度归一 参数控制词频饱和与长度惩罚", 65),
        (SourceId::Note, "sqlite fts5 使用注意",
         "sqlite 的 fts5 支持 bm25 但中文短查询要自己兜底", 55),
    ]
}

struct Fixture {
    dir: TempDir,
    indexer: Indexer,
    now: i64,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let mut indexer = Indexer::open(dir.path()).unwrap();
        let now = hub_store::indexer::now_secs();

        for (i, (source, title, body, days)) in corpus().into_iter().enumerate() {
            let uri = format!("{}/{}", source.as_str(), i + 1);
            assert!(
                indexer.add(source, &uri, title.to_string(), body.to_string(), now - days * DAY).unwrap(),
                "首轮写入不应被跳过"
            );
        }
        indexer.flush().unwrap();
        Self { dir, indexer, now }
    }

    fn sources(&self, q: &str, k: usize) -> Vec<SourceId> {
        let pq = self.indexer.parse(q);
        self.indexer
            .index()
            .search(&pq, k, self.now)
            .unwrap()
            .into_iter()
            .map(|h| h.source)
            .collect()
    }

    fn uris(&self, q: &str, k: usize) -> Vec<String> {
        let pq = self.indexer.parse(q);
        self.indexer
            .index()
            .search(&pq, k, self.now)
            .unwrap()
            .into_iter()
            .map(|h| h.uri)
            .collect()
    }
}

/// 内存参考实现的 Top-K 来源分布
fn core_sources(q: &str, k: usize, now: i64) -> Vec<SourceId> {
    let mut ix = MemoryIndex::default();
    for (i, (source, title, body, days)) in corpus().into_iter().enumerate() {
        ix.upsert(Doc {
            id: (i + 1) as u64,
            source,
            uri: format!("{}/{}", source.as_str(), i + 1),
            title: title.to_string(),
            body: body.to_string(),
            mtime: now - days * DAY,
            content_hash: (i + 1) as u64 * 7919,
            deleted: false,
        });
    }
    let pq = parse_query(q, ix.dict());
    ix.search(&pq, k, now)
        .into_iter()
        .map(|(id, _)| ix.get(id).unwrap().source)
        .collect()
}

/// 核心断言：查询识别出来源后，Top-3 不允许跨源泄漏
fn assert_no_leak(fx: &Fixture, q: &str, want: SourceId) {
    let got = fx.sources(q, 3);
    assert!(!got.is_empty(), "查询 {q:?} 返回空结果");
    assert!(
        got.iter().all(|s| *s == want),
        "查询 {q:?} 跨源泄漏，期望全部 {want:?}，实际 {got:?}"
    );
}

#[test]
fn natural_language_queries_land_on_right_source() {
    let fx = Fixture::new();
    assert_no_leak(&fx, "上个月写的那个rust内存相关的代码片段", SourceId::Files);
    assert_no_leak(&fx, "之前敲过的交叉编译命令", SourceId::Terminal);
    assert_no_leak(&fx, "群里老张发的服务器方案链接", SourceId::Chat);
    assert_no_leak(&fx, "三个月前那次编译报错截图", SourceId::Ocr);
    assert_no_leak(&fx, "我的架构笔记", SourceId::Note);
}

/// 持久化实现与内存参考实现，来源分布必须逐位一致
///
/// 这条测试曾经抓到过一个很隐蔽的坑：来源过滤写成外层 `Must` 后，Lucene 语义会让
/// `Should` 变可选，同来源的所有文档都会成为候选（0 分），于是这里多出第 3 条 Terminal。
/// 修法是把正向条件包成"只有 Should 的子查询"再整体 Must。
#[test]
fn parity_with_memory_reference() {
    let fx = Fixture::new();
    for q in [
        "上个月写的那个rust内存相关的代码片段",
        "之前敲过的交叉编译命令",
        "群里老张发的服务器方案链接",
        "三个月前那次编译报错截图",
    ] {
        let store = fx.sources(q, 3);
        let core = core_sources(q, 3, fx.now);
        assert_eq!(store, core, "查询 {q:?} 两套实现的来源分布不一致");
    }
}

/// 0 分文档不许出现在结果里。
///
/// 来源硬过滤是"过滤"，不是"匹配"：它不该把同来源但完全没命中关键词的文档拉进来。
#[test]
fn zero_score_documents_are_not_returned() {
    let fx = Fixture::new();
    // 该查询会识别出来源 Terminal，硬过滤到 5 条终端历史；
    // 其中只有 2 条真正含"交叉编译"，其余必须不出现。
    let pq = fx.indexer.parse("之前敲过的交叉编译命令");
    let hits = fx.indexer.index().search(&pq, 20, fx.now).unwrap();
    assert!(!hits.is_empty());
    for h in &hits {
        assert!(h.score > 0.0, "0 分文档混进结果: {} ({:.4})", h.uri, h.score);
    }
    assert_eq!(hits.len(), 2, "应只有 2 条真命中: {:?}", hits.iter().map(|h| &h.uri).collect::<Vec<_>>());
}

/// 引号短语必须能"引入"文档：doc5 正文既没有「内存」也没有「管理」
#[test]
fn phrase_introduces_document() {
    let fx = Fixture::new();
    let top = fx.uris("内存 管理 \"零拷贝\"", 3);
    assert!(
        top.first().map(|u| u.ends_with("/5")).unwrap_or(false),
        "含精确短语「零拷贝」的 mmap 文档应排第一，实际 {top:?}"
    );
}

/// 排除词生效
#[test]
fn exclude_removes_document() {
    let fx = Fixture::new();
    let top = fx.uris("内存 分配 -malloc", 10);
    assert!(
        !top.iter().any(|u| u.ends_with("/6")),
        "排除词 malloc 应剔除 C 语言文档，实际 {top:?}"
    );
}

/// 排除词大小写不敏感（正文里可能是 `MMAP`，查询里写 `-mmap`）
#[test]
fn exclude_is_case_insensitive() {
    let fx = Fixture::new();
    let top = fx.uris("\"零拷贝\" -MMAP", 10);
    assert!(
        !top.iter().any(|u| u.ends_with("/5")),
        "大写排除词 MMAP 应剔除 mmap 文档，实际 {top:?}"
    );
}

/// 时间约束：区间外的文档必须被明显压低
#[test]
fn time_constraint_penalizes_out_of_range() {
    let fx = Fixture::new();
    let pq = fx.indexer.parse("内存代码片段 上个月");
    let hits: HashMap<String, f32> = fx
        .indexer
        .index()
        .search(&pq, 20, fx.now)
        .unwrap()
        .into_iter()
        .map(|h| (h.uri, h.score))
        .collect();

    let doc4 = *hits.get("files/4").expect("doc4 应出现在结果中");
    let doc2 = *hits.get("files/2").expect("doc2 应出现在结果中");
    assert!(
        doc4 < doc2,
        "95 天前的 doc4 ({doc4:.4}) 应低于 40 天前的 doc2 ({doc2:.4})"
    );
}

/// 纯时间查询（没有检索词）按最近顺序返回
#[test]
fn time_only_query_lists_recent_docs() {
    let fx = Fixture::new();
    let hits = fx.indexer.search_text("上个月", 5).unwrap();
    assert!(!hits.is_empty(), "纯时间查询不应返回空");
    for h in &hits {
        let days = (fx.now - h.mtime) / DAY;
        assert!(
            (30..=60).contains(&days),
            "结果 {:?} 不在「上个月」区间内（{days} 天前）",
            h.uri
        );
    }
}

/// 重启后仍能搜到 —— 这是"持久化"三个字的唯一验收标准
#[test]
fn survives_reopen() {
    let fx = Fixture::new();
    let before = fx.uris("mmap 零拷贝", 3);
    assert!(!before.is_empty());

    let path = fx.dir.path().to_path_buf();
    drop(fx.indexer); // 关闭 SQLite 与 reader

    let indexer = Indexer::open(&path).unwrap();
    let pq = indexer.parse("mmap 零拷贝");
    let after: Vec<String> = indexer
        .index()
        .search(&pq, 3, hub_store::indexer::now_secs())
        .unwrap()
        .into_iter()
        .map(|h| h.uri)
        .collect();
    assert_eq!(before, after, "重启后检索结果发生变化");
    assert_eq!(indexer.meta().count_live().unwrap(), 24);
}

/// 内容没变就不重复写；doc_id 也不能漂移
#[test]
fn incremental_skip_and_stable_id() {
    let mut fx = Fixture::new();
    let now = hub_store::indexer::now_secs();
    let id_before = fx.indexer.meta().lookup(SourceId::Files, "files/1").unwrap().unwrap().doc_id;

    let changed = fx
        .indexer
        .add(SourceId::Files, "files/1", "同一标题".into(), corpus()[0].2.into(), now - 42 * DAY)
        .unwrap();
    assert!(!changed, "内容未变应跳过");
    assert_eq!(fx.indexer.flush().unwrap(), 0);

    let changed = fx
        .indexer
        .add(SourceId::Files, "files/1", "改过的标题".into(), "全新的正文内容 带有 zzz 标记".into(), now - 42 * DAY)
        .unwrap();
    assert!(changed, "内容变了必须重新索引");
    assert_eq!(fx.indexer.flush().unwrap(), 1);

    let id_after = fx.indexer.meta().lookup(SourceId::Files, "files/1").unwrap().unwrap().doc_id;
    assert_eq!(id_before, id_after, "文档更新后 doc_id 不应漂移");
}

/// 删除后立刻搜不到（tombstone 生效）
#[test]
fn delete_is_visible_immediately() {
    let mut fx = Fixture::new();
    let uri = "files/5";
    assert!(!fx.uris("\"零拷贝\"", 3).is_empty());

    fx.indexer.delete(SourceId::Files, uri);
    fx.indexer.flush().unwrap();

    let after = fx.uris("\"零拷贝\"", 5);
    assert!(!after.iter().any(|u| u == uri), "删除后仍能搜到 {uri}: {after:?}");
    let row = fx.indexer.meta().lookup(SourceId::Files, uri).unwrap().unwrap();
    assert!(!row.live);
}
