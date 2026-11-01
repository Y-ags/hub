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
    // 必须用 fixture 建库时的那个时间戳。重新调 now_secs() 可能在跨秒时
    // 得到 now+1，于是 mtime 对不上、被判定成"内容变了" —— 测试就随机挂。
    let now = fx.now;
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

// ============================================================
// 段合并（daemon 的空闲维护任务，见 hub-daemon/src/lib.rs）
// ============================================================

/// 每批写入都是一个新段。段攒起来后合并应该把它们收成 1 个，
/// 且"没什么可合的时候不白干活"。
#[test]
fn merge_collapses_segments() {
    let dir = TempDir::new().unwrap();
    let mut ix = Indexer::open(dir.path()).unwrap();
    for i in 0..6 {
        assert!(ix
            .add(
                SourceId::Files,
                &format!("files/{i}"),
                format!("文档 {i}"),
                format!("正文内容 {i} 凑一点长度让倒排有东西可写"),
                1_700_000_000 + i,
            )
            .unwrap());
        ix.flush().unwrap();
    }
    assert!(ix.segment_count() > 1, "六次独立提交应产生多个段");

    assert!(ix.merge_if_needed(2).unwrap(), "段数超过阈值应该合并");
    assert_eq!(ix.segment_count(), 1, "合并后应只剩一个段");

    // 已经只有 1 个段、也没有墓碑 → 不该再重写一遍文件
    assert!(!ix.merge_if_needed(2).unwrap(), "无事可做时不该合并");
    // 数据没被合并弄丢
    assert_eq!(ix.meta().count_live().unwrap(), 6);
    assert_eq!(ix.index().num_docs(), 6);
}

/// 墓碑是"删了但空间还占着"。阈值没到也必须因为墓碑而合并，
/// 否则删掉的东西永远占磁盘，而用户不会知道要手动 hub merge。
#[test]
fn merge_purges_tombstones_even_below_segment_threshold() {
    let dir = TempDir::new().unwrap();
    let mut ix = Indexer::open(dir.path()).unwrap();
    for i in 0..3 {
        ix.add(SourceId::Files, &format!("files/{i}"), "标题".into(), format!("内容 {i}"), 1_700_000_000).unwrap();
    }
    ix.flush().unwrap();
    ix.delete(SourceId::Files, "files/0");
    ix.flush().unwrap();
    assert_eq!(ix.meta().count_tombstone().unwrap(), 1);

    // 阈值给到 100：段数远远不够，但墓碑必须触发合并
    assert!(ix.merge_if_needed(100).unwrap(), "有墓碑就该合并");
    assert_eq!(ix.meta().count_tombstone().unwrap(), 0, "墓碑应被清掉");
    let pq = ix.parse("内容 0");
    let hits = ix.search(&pq, 5).unwrap();
    assert!(!hits.iter().any(|h| h.uri == "files/0"), "删掉的文档不应再出现");
}

/// 反复修改同一个文件：倒排里必须替换，而不是每次多留一条僵尸。
///
/// 这个 bug 的表现极其隐蔽 —— 搜索结果按 doc_id 去重，用户看不出重复；
/// 只有 `stats` 里"倒排中"远大于"存活"才会露馅，而那时磁盘已经白占了很多。
#[test]
fn rewrites_replace_instead_of_appending() {
    let dir = TempDir::new().unwrap();
    let mut ix = Indexer::open(dir.path()).unwrap();
    for i in 0..5 {
        assert!(ix
            .add(
                SourceId::Files,
                "files/same",
                "同一篇".into(),
                format!("第 {i} 版正文 关键词 rewrite"),
                1_700_000_000 + i,
            )
            .unwrap());
        ix.flush().unwrap();
    }
    assert_eq!(ix.meta().count_live().unwrap(), 1);
    assert_eq!(ix.index().num_docs(), 1, "倒排里应只有一条，而不是五条僵尸");

    let pq = ix.parse("rewrite");
    let hits = ix.search(&pq, 10).unwrap();
    assert_eq!(hits.len(), 1, "同一文件不该出现多条命中");
    // 留下的必须是最后一版：mtime 被更新成了第 5 次写入时给的值
    assert_eq!(hits[0].mtime, 1_700_000_004, "应该是最后一版");
}

// ============================================================
// 共享只读句柄（daemon 的 IPC 查询线程靠它）
// ============================================================

/// `index_handle()` 拿到的必须**是同一个 reader**，不是"又打开了一份"。
///
/// 这条是 IPC 正确性的全部基础：daemon 的 IPC 线程用这个句柄回答查询，
/// 主循环用 `Indexer` 写入。如果句柄是另开的一份索引，IPC 就会一直
/// 看到旧数据 —— 而这种错在单测里永远暴露不出来，只有"改了文件但搜不到"
/// 才会被发现。
#[test]
fn shared_handle_sees_commits_from_the_writer() {
    const NOW: i64 = 1_700_000_200;
    let dir = TempDir::new().unwrap();
    let mut ix = Indexer::open(dir.path()).unwrap();
    let reader = ix.index_handle();
    assert_eq!(reader.num_docs(), 0);

    assert!(ix
        .add(
            SourceId::Files,
            "files/a",
            "第一篇".into(),
            "关键词 shardprobe 在这里".into(),
            1_700_000_000,
        )
        .unwrap());
    ix.flush().unwrap();

    // 没有 reload、没有重新 open，句柄就该看到 —— commit 内部已经 reload 过
    assert_eq!(reader.num_docs(), 1, "共享句柄应看到刚提交的文档");
    let pq = reader.parse("shardprobe");
    assert_eq!(reader.search(&pq, 5, NOW).unwrap().len(), 1);

    // 再写一篇，同一个句柄继续跟得上
    assert!(ix
        .add(
            SourceId::Terminal,
            "terminal://1",
            "命令".into(),
            "cargo build 关键词 shardprobe".into(),
            1_700_000_100,
        )
        .unwrap());
    ix.flush().unwrap();
    assert_eq!(reader.num_docs(), 2);
    assert_eq!(reader.search(&pq, 5, NOW).unwrap().len(), 2);

    // 删除也要同步可见
    ix.delete(SourceId::Files, "files/a");
    ix.flush().unwrap();
    let hits = reader.search(&pq, 5, NOW).unwrap();
    assert_eq!(hits.len(), 1, "删掉的文档不该再出现：{hits:?}");
    assert_eq!(hits[0].uri, "terminal://1");
}

/// 句柄是只读的：它不能凭空多出一个写入器，否则 daemon 会同时有两个写者。
/// 这里用"另开一个 Indexer 仍能写"来反证 —— 如果句柄抢了写锁，这里会失败。
#[test]
fn shared_handle_does_not_hold_the_writer_lock() {
    let dir = TempDir::new().unwrap();
    let ix = Indexer::open(dir.path()).unwrap();
    let _reader = ix.index_handle();

    // 同一进程里再开一个 Indexer 并写入（模拟 CLI 的 --local 路径）。
    // 两边的写入器是错开的，不该互相卡住。
    let mut other = Indexer::open(dir.path()).unwrap();
    assert!(other
        .add(SourceId::Files, "files/b", "另一路".into(), "关键词 lockprobe".into(), 1_700_000_000)
        .unwrap());
    other.flush().unwrap();

    // 写成功了就说明没被对方的锁卡住。这里断言 `other` 自己的视图 ——
    // `ix` 的 reader 要等 OnCommitWithDelay 那一拍才会看到别人写的段，
    // 那是另一个行为（跨 Indexer 的可见性延迟），不是这条测试要证的。
    assert_eq!(other.index().num_docs(), 1);
}
