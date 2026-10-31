//! hub-core —— 本地离线多源个人检索中枢的核心语义层。
//!
//! 设计原则：本 crate **零外部依赖**（只用 std），因此 `cargo test -p hub-core` 在任何
//! 装了 Rust 的机器上都能直接跑通，不需要下载任何 crate。
//! 所有"查询语义"的正确性都由本 crate 的测试锁住；hub-store 只替换存储层，不改语义。

pub mod index;
pub mod model;
pub mod query;
pub mod source_hint;
pub mod time_expr;
pub mod tokenizer;

pub use index::{DECAY_DAYS, MemoryIndex, OFF_PENALTY, PHRASE_FLOOR, PHRASE_RATIO, phrase_bonus, time_decay};
pub use model::{Change, Doc, DocId, SourceId, contains_ci};
pub use query::{ParsedQuery, parse_query};
pub use source_hint::parse_source;
pub use time_expr::{TimeRange, parse_time, strip_time_expr};
pub use tokenizer::{Dict, bigram_tokens, fused_tokens, word_tokens};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 固定"现在"，让所有时间相关断言可复现
    const NOW: i64 = 1_700_000_000;
    const DAY: i64 = 86_400;

    fn days_ago(d: i64) -> i64 {
        NOW - d * DAY
    }

    /// 模拟真实分布的多源语料：代码 / 终端 / 聊天 / 截图 / 笔记
    /// id 分配：c01..c08 = 1..8，t01..t05 = 9..13，h01..h04 = 14..17，o01..o03 = 18..20，n01..n04 = 21..24
    fn corpus() -> Vec<(DocId, SourceId, &'static str, &'static str, i64)> {
        vec![
            // ---- 代码片段 ----
            (1, SourceId::Files, "rust 手动分配内存示例 alloc.rs",
             "use std::alloc::{alloc, Layout}; 手动在堆上分配一块内存 大小 1024 对齐 8 字节 然后 unsafe 解引用裸指针写入数据", 42),
            (2, SourceId::Files, "rust Box 堆分配 Vec 容量管理",
             "Box::new 在堆上分配 离开作用域自动释放 Vec 内部管理 capacity 与 len 扩容时重新分配内存块并拷贝", 40),
            (3, SourceId::Files, "实现一个简单的内存池",
             "自己写一个内存池 按固定大小切块 用 free list 串起来 分配时取表头 释放时挂回 避免频繁 malloc", 33),
            (4, SourceId::Files, "读进程内存 ReadProcessMemory 封装",
             "OpenProcess 拿到句柄 然后 ReadProcessMemory 读目标进程某地址的数据 WriteProcessMemory 写入新值", 95),
            (5, SourceId::Files, "mmap 映射大文件零拷贝读取",
             "用 mmap 把文件映射到地址空间 读的时候按需缺页 不把整个文件读进堆 适合大文件随机访问", 61),
            (6, SourceId::Files, "C 语言 malloc free 手动管理",
             "c 语言里 malloc 分配 free 释放 忘记释放就内存泄漏 悬垂指针 use after free 是经典问题", 120),
            (7, SourceId::Files, "终端历史解析模块",
             "解析 zsh_history 里的每一行 提取时间戳和命令 去掉重复 按时间排序后写入索引", 28),
            (8, SourceId::Files, "中文分词 bigram 实现",
             "对连续的中文字符做二元切分 生成相邻二元组 无词表 召回高但噪声多 适合兜底新词", 25),
            // ---- 终端历史 ----
            (9, SourceId::Terminal, "cargo build --release 带优化编译",
             "cargo build --release 编译 rust 项目 开启优化 输出体积更小 运行更快", 45),
            (10, SourceId::Terminal, "配置 ohos ndk 交叉编译 clang 路径",
             "export OHOS_NDK_HOME 指向鸿蒙 ndk 目录 然后 rustup target add 用 clang 链接", 37),
            (11, SourceId::Terminal, "杀掉占用端口的进程",
             "lsof 找到 pid 然后 kill 干掉 释放端口", 88),
            (12, SourceId::Terminal, "递归搜索文件内容",
             "grep 搜索当前目录下所有包含关键字的文件 显示行号", 52),
            (13, SourceId::Terminal, "git 回退到某次提交",
             "git log 找到提交的 hash 然后 git reset 回退 注意会丢掉工作区改动", 130),
            // ---- 聊天记录 ----
            (14, SourceId::Chat, "老张发的服务器方案链接",
             "老张在群里发了一个链接 说是新的服务器部署方案 让大家看看能不能用在我们的项目里", 76),
            (15, SourceId::Chat, "讨论内存占用太高的问题",
             "小李说后台进程内存占用太高了 一直挂着 问他能不能优化 老王说索引走 mmap 就不占常驻内存", 31),
            (16, SourceId::Chat, "约周五下午开会",
             "周会上说周五下午三点开个短会 同步一下进度 地点在三号会议室", 15),
            (17, SourceId::Chat, "推荐一个截图工具",
             "同事推荐了个截图工具 说能自动识别文字 找旧截图特别方便", 58),
            // ---- 截图 OCR ----
            (18, SourceId::Ocr, "报错截图 编译失败",
             "error could not compile 编译报错 提示找不到依赖 截图里红字一片", 48),
            (19, SourceId::Ocr, "任务管理器内存占用截图",
             "任务管理器里看到某个后台进程 占用内存 名字是 searcherd 截图记录当时状态", 30),
            (20, SourceId::Ocr, "终端窗口 cargo 输出",
             "终端里 cargo build 的输出 显示编译完成 用了 12 秒", 44),
            // ---- 笔记 ----
            (21, SourceId::Note, "个人数据中枢架构设想",
             "把散落各处的数据统一索引 文件 终端 聊天 截图 全部本地离线 不上传 隐私第一", 20),
            (22, SourceId::Note, "倒排索引与段合并笔记",
             "倒排索引 term 到 doc 列表 段不可变 删除先打标记 合并时才真正回收空间", 22),
            (23, SourceId::Note, "BM25 排序公式推导",
             "bm25 在 tfidf 基础上加了文档长度归一 参数控制词频饱和与长度惩罚", 65),
            (24, SourceId::Note, "sqlite fts5 使用注意",
             "sqlite 的 fts5 支持 bm25 但中文短查询要自己兜底", 55),
        ]
    }

    fn fixture() -> MemoryIndex {
        let mut ix = MemoryIndex::default();
        for (id, source, title, body, days) in corpus() {
            ix.upsert(Doc {
                id,
                source,
                uri: format!("{}/{}", source.as_str(), id),
                title: title.to_string(),
                body: body.to_string(),
                mtime: days_ago(days),
                content_hash: id * 7919,
                deleted: false,
            });
        }
        ix
    }

    fn search_src(ix: &MemoryIndex, q: &str, k: usize) -> Vec<SourceId> {
        let pq = parse_query(q, ix.dict());
        ix.search(&pq, k, NOW)
            .into_iter()
            .map(|(id, _)| ix.get(id).unwrap().source)
            .collect()
    }

    // ---------------------------------------------------------------
    // 端到端：自然语言查询必须落到正确的源
    // ---------------------------------------------------------------

    /// 核心断言：**不允许跨源泄漏**。
    /// 不写死"必须返回 3 条" —— 丢掉单字噪声后，"之前敲过的交叉编译命令"
    /// 真实匹配只有 2 条终端历史，要求 3 条是错的。
    fn assert_no_leak(ix: &MemoryIndex, q: &str, want: SourceId) {
        let got = search_src(ix, q, 3);
        assert!(!got.is_empty(), "查询 {:?} 返回空结果", q);
        assert!(
            got.iter().all(|s| *s == want),
            "查询 {:?} 出现跨源泄漏，期望全部 {:?}，实际 {:?}",
            q, want, got
        );
    }

    #[test]
    fn code_query_returns_code() {
        assert_no_leak(&fixture(), "上个月写的那个rust内存相关的代码片段", SourceId::Files);
    }

    #[test]
    fn terminal_query_returns_terminal() {
        assert_no_leak(&fixture(), "之前敲过的交叉编译命令", SourceId::Terminal);
    }

    #[test]
    fn chat_query_returns_chat() {
        assert_no_leak(&fixture(), "群里老张发的服务器方案链接", SourceId::Chat);
    }

    #[test]
    fn ocr_query_returns_ocr() {
        assert_no_leak(&fixture(), "三个月前那次编译报错截图", SourceId::Ocr);
    }

    #[test]
    fn time_constraint_penalizes_out_of_range() {
        let ix = fixture();
        let with_time = parse_query("上个月写的那个rust内存相关的代码片段", ix.dict());
        let without = parse_query("rust内存相关的代码片段", ix.dict());

        let a: HashMap<DocId, f32> = ix.search(&with_time, 20, NOW).into_iter().collect();
        let b: HashMap<DocId, f32> = ix.search(&without, 20, NOW).into_iter().collect();

        // doc4 是 95 天前，落在"上个月"(30~60天)之外，分数必须被明显压低
        let s_with = *a.get(&4).expect("doc4 应出现在结果中");
        let s_without = *b.get(&4).expect("doc4 应出现在结果中");
        assert!(
            s_with < s_without * 0.5,
            "时间约束未生效：doc4 带约束 {:.4}，不带约束 {:.4}",
            s_with,
            s_without
        );

        // 反过来，落在区间内的 doc2（40天前）不应被惩罚
        let in_with = *a.get(&2).expect("doc2 应出现在结果中");
        let in_without = *b.get(&2).expect("doc2 应出现在结果中");
        assert!(
            (in_with - in_without).abs() < 1e-6,
            "区间内文档不应被惩罚：{:.4} vs {:.4}",
            in_with,
            in_without
        );
    }

    // ---------------------------------------------------------------
    // 删除语义：tombstone 立即隐藏，merge 只回收磁盘
    // ---------------------------------------------------------------

    #[test]
    fn tombstone_hides_immediately() {
        let mut ix = fixture();
        let pq = parse_query("读进程内存 写入新值", ix.dict());
        let before: Vec<DocId> = ix.search(&pq, 20, NOW).into_iter().map(|(i, _)| i).collect();
        assert!(before.contains(&4), "删除前 c04 应可被搜到");

        ix.delete(4); // 只标记，不 merge

        let after: Vec<DocId> = ix.search(&pq, 20, NOW).into_iter().map(|(i, _)| i).collect();
        assert!(!after.contains(&4), "标记删除后 c04 必须立刻搜不到");
    }

    #[test]
    fn merge_reclaims_space() {
        let mut ix = fixture();
        let before_entries = ix.posting_entries();
        for id in [4, 11, 14, 18, 23] {
            ix.delete(id);
        }
        let still_occupied = ix.posting_entries();
        assert_eq!(
            still_occupied, before_entries,
            "merge 前磁盘占用不应变化（tombstone 只标记）"
        );

        let reclaimed = ix.merge();
        assert!(reclaimed > 0, "merge 必须实际回收条目");
        assert!(ix.posting_entries() < before_entries);
        assert_eq!(ix.len(), 19, "5 篇被删，应剩 19 篇");
    }

    // ---------------------------------------------------------------
    // 短语与排除
    // ---------------------------------------------------------------

    /// 关键语义：**引号短语要能"引入"文档**，而不只是给已有结果提权。
    /// doc5（mmap 零拷贝）正文里既没有"内存"也没有"管理"，它进不了 BM25 候选集；
    /// 如果短语加权只遍历已打分文档，这条查询永远找不到它。
    #[test]
    fn phrase_can_introduce_document() {
        let ix = fixture();
        let pq = parse_query("内存 管理 \"零拷贝\"", ix.dict());
        let top: Vec<DocId> = ix.search(&pq, 3, NOW).into_iter().map(|(i, _)| i).collect();

        assert!(top.contains(&5), "含精确短语'零拷贝'的 mmap 文档必须被检索到: {:?}", top);
        assert_eq!(top[0], 5, "命中精确短语的文档应排第一: {:?}", top);
    }

    #[test]
    fn exclude_removes_document() {
        let ix = fixture();
        let pq = parse_query("内存 分配 -malloc", ix.dict());
        let top: Vec<DocId> = ix.search(&pq, 10, NOW).into_iter().map(|(i, _)| i).collect();
        assert!(!top.contains(&6), "排除词 malloc 应剔除 c 语言文档: {:?}", top);
    }

    /// 排除词大小写不敏感：`parse_query` 会把 `-MALLOC` 小写成 `malloc`，
    /// 而正文里可能是 `malloc` / `MALLOC` 混着写。
    #[test]
    fn exclude_is_case_insensitive() {
        let ix = fixture();
        let pq = parse_query("内存 分配 -MALLOC", ix.dict());
        assert_eq!(pq.excludes, vec!["malloc".to_string()]);
        let top: Vec<DocId> = ix.search(&pq, 10, NOW).into_iter().map(|(i, _)| i).collect();
        assert!(!top.contains(&6), "大写排除词也必须生效: {:?}", top);
    }

    /// 不存在的短语不能凭空造出结果，只能在不命中时静默失效
    #[test]
    fn nonexistent_phrase_is_ignored() {
        let ix = fixture();
        let pq = parse_query("内存 \"绝对不存在的短语xyz\"", ix.dict());
        let top: Vec<DocId> = ix.search(&pq, 5, NOW).into_iter().map(|(i, _)| i).collect();
        assert!(!top.is_empty(), "无短语命中时应退回关键词结果: {:?}", top);
        assert!(top.iter().all(|d| ix.get(*d).unwrap().text().contains("内存")));
    }

    #[test]
    fn empty_result_is_safe() {
        let ix = fixture();
        let pq = parse_query("完全不存在的词zzz", ix.dict());
        assert!(ix.search(&pq, 10, NOW).is_empty());
    }
}
