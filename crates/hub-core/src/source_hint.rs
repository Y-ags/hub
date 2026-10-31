//! 查询中的"来源意图"识别。
//!
//! **第二个真实踩过的坑**：搜 "rust内存相关的代码片段"，BM25 把"我写的架构笔记"排到了第一，
//! 真正的代码排在后面。原因是 BM25 只算文本相关性，不理解 "代码片段" 是来源约束。
//! 没有这一层，"多源检索" 就只是"多源混杂"。

use crate::model::SourceId;

/// 关键词按长度降序排列，保证 "代码片段" 先于 "代码" 命中。
const HINTS: &[(&str, SourceId)] = &[
    ("代码片段", SourceId::Files),
    ("rust代码", SourceId::Files),
    ("源码", SourceId::Files),
    ("代码", SourceId::Files),
    ("函数", SourceId::Files),
    ("敲过的命令", SourceId::Terminal),
    ("终端", SourceId::Terminal),
    ("命令行", SourceId::Terminal),
    ("命令", SourceId::Terminal),
    ("指令", SourceId::Terminal),
    ("shell", SourceId::Terminal),
    ("聊天记录", SourceId::Chat),
    ("聊天", SourceId::Chat),
    ("群里", SourceId::Chat),
    ("微信", SourceId::Chat),
    ("消息", SourceId::Chat),
    ("报错截图", SourceId::Ocr),
    ("截图", SourceId::Ocr),
    ("截屏", SourceId::Ocr),
    ("笔记", SourceId::Note),
    ("文档", SourceId::Note),
];

pub fn parse_source(q: &str) -> Option<SourceId> {
    let lower = q.to_ascii_lowercase();
    for (kw, src) in HINTS {
        if lower.contains(kw) {
            return Some(*src);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_sources() {
        assert_eq!(parse_source("之前敲过的交叉编译命令"), Some(SourceId::Terminal));
        assert_eq!(parse_source("那个 rust 代码片段"), Some(SourceId::Files));
        assert_eq!(parse_source("群里发的服务器方案"), Some(SourceId::Chat));
        assert_eq!(parse_source("三个月前的报错截图"), Some(SourceId::Ocr));
        assert_eq!(parse_source("我的架构笔记"), Some(SourceId::Note));
    }

    #[test]
    fn longest_hint_wins() {
        // "代码片段" 必须优先于 "代码"，否则 Files 也可能被别的源抢走
        assert_eq!(parse_source("代码片段"), Some(SourceId::Files));
    }

    #[test]
    fn no_source_hint() {
        assert_eq!(parse_source("内存分配"), None);
    }
}
