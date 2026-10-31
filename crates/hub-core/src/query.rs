//! 查询解析器 —— 自然语言搜索能否成立，全靠这一层。
//!
//! 支持：
//! - 时间："上个月" / "三个月前" / "3天前"
//! - 来源：自然语言（"代码片段"）与显式语法（source:code / in:terminal）
//! - 短语："精确短语"
//! - 排除：-关键词

use crate::model::SourceId;
use crate::source_hint::parse_source;
use crate::time_expr::{TimeRange, parse_time, strip_time_expr};
use crate::tokenizer::{Dict, fused_tokens};

#[derive(Debug, Clone, Default)]
pub struct ParsedQuery {
    /// 分词后的检索词（词级 + bigram 融合）
    pub terms: Vec<String>,
    /// 引号内的精确短语，命中后加固定权重
    pub phrases: Vec<String>,
    /// `-xxx` 排除词
    pub excludes: Vec<String>,
    pub time: Option<TimeRange>,
    pub source: Option<SourceId>,
    /// 去掉时间/来源/短语后剩下的"裸意图"，用于调试与展示
    pub residue: String,
}

pub fn parse_query(q: &str, dict: &Dict) -> ParsedQuery {
    let mut pq = ParsedQuery::default();

    // 1) 时间与来源用原始查询解析（不能先剔除，否则"上个月"会被吃掉）
    pq.time = parse_time(q);

    // 2) 提取引号短语，其余字符进入 rest
    let mut rest = String::new();
    let mut buf = String::new();
    let mut in_quote = false;
    for c in q.chars() {
        match c {
            '"' | '\u{201c}' | '\u{201d}' => {
                if in_quote {
                    let p = std::mem::take(&mut buf);
                    if !p.trim().is_empty() {
                        pq.phrases.push(p);
                    }
                }
                in_quote = !in_quote;
            }
            _ => {
                if in_quote {
                    buf.push(c);
                } else {
                    rest.push(c);
                }
            }
        }
    }
    // 未闭合引号里的残余也要收进来，避免丢词
    if !buf.trim().is_empty() {
        pq.phrases.push(buf);
    }

    // 3) 时间词是元信息，不是检索词：先从裸文本里摘掉，再处理显式语法
    //
    //    不摘的后果（第一版就是这样）："上个月写的那个rust内存相关的代码片段" 会把
    //    「上个 / 个月 / 写的 / 那个」一起丢进分词，噪声 token 稀释 BM25，
    //    同时让 residue 名不副实（文档说它是"去掉时间后的裸意图"）。
    let stripped = strip_time_expr(&rest);

    // 4) 逐词处理显式语法
    let mut words: Vec<&str> = Vec::new();
    for tok in stripped.split_whitespace() {
        let lowered = tok.to_ascii_lowercase();
        if let Some(v) = lowered
            .strip_prefix("source:")
            .or_else(|| lowered.strip_prefix("in:"))
        {
            pq.source = SourceId::parse(v);
        } else if let Some(v) = lowered.strip_prefix('-') {
            if !v.is_empty() {
                pq.excludes.push(v.to_string());
            }
        } else {
            words.push(tok);
        }
    }

    // 5) 显式语法没给出来源时，退回自然语言识别
    if pq.source.is_none() {
        pq.source = parse_source(q);
    }

    // 6) 剩余文本做融合分词
    let residue = words.join(" ");
    pq.terms = fused_tokens(&residue, dict);
    pq.residue = residue;
    pq
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_source_syntax() {
        let d = Dict::new();
        let q = parse_query("source:terminal 交叉编译", &d);
        assert_eq!(q.source, Some(SourceId::Terminal));
        assert!(q.terms.iter().any(|t| t.contains("交叉")));
    }

    #[test]
    fn phrase_and_exclude() {
        let d = Dict::new();
        let q = parse_query("内存 管理 \"零拷贝\" -测试", &d);
        assert_eq!(q.phrases, vec!["零拷贝".to_string()]);
        assert_eq!(q.excludes, vec!["测试".to_string()]);
    }

    #[test]
    fn natural_language_full() {
        let d = Dict::new();
        let q = parse_query("上个月写的那个rust内存相关的代码片段", &d);
        assert_eq!(q.time, Some(TimeRange { min_days: 30, max_days: 60 }));
        assert_eq!(q.source, Some(SourceId::Files));
        assert!(q.terms.iter().any(|t| t == "内存"));
    }

    /// 时间词不能被当成检索词，否则「上个 / 个月 / 写的 / 那个」会稀释 BM25
    #[test]
    fn time_words_do_not_leak_into_terms() {
        let d = Dict::new();
        let q = parse_query("上个月写的那个rust内存相关的代码片段", &d);
        assert!(!q.residue.contains("上个月"), "residue 仍残留时间词: {:?}", q.residue);
        assert!(!q.terms.iter().any(|t| t == "上个" || t == "个月"));
    }

    /// 纯时间查询没有检索词，由存储层走"按时间列最近文档"的分支
    #[test]
    fn time_only_query_has_no_terms() {
        let d = Dict::new();
        let q = parse_query("三个月前", &d);
        assert_eq!(q.time, Some(TimeRange { min_days: 75, max_days: 105 }));
        assert!(q.terms.is_empty(), "意外产出检索词: {:?}", q.terms);
    }
}
