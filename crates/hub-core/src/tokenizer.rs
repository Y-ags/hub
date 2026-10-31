//! 中文分词。零依赖实现。
//!
//! 两条路线，必须同时用：
//! - `word_tokens`：词级（最大正向匹配 + 词典），长查询排序精确
//! - `bigram_tokens`：二元切分，无词表，兜住新词 / 型号 / OCR 碎片
//!
//! 验证结论（见 verify_core.py 实验1）：融合方案 MRR 0.958，优于任一单路。

use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct Dict {
    words: HashSet<String>,
}

impl Default for Dict {
    fn default() -> Self {
        Self::new()
    }
}

impl Dict {
    pub fn new() -> Self {
        Self::from_words(&DEFAULT_DICT)
    }

    pub fn from_words(words: &[&str]) -> Self {
        Self {
            words: words.iter().map(|w| w.to_string()).collect(),
        }
    }

    pub fn contains(&self, w: &str) -> bool {
        self.words.contains(w)
    }

    pub fn insert(&mut self, w: &str) {
        self.words.insert(w.to_string());
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

/// 内置基础词表。生产环境应由 jieba-rs / cang-jie 提供，
/// 这里只为让 hub-core 零依赖可测试。
pub const DEFAULT_DICT: &[&str] = &[
    "内存", "内存池", "管理", "分配", "分配器", "释放", "堆", "栈", "指针", "裸指针", "悬垂指针",
    "泄漏", "手动管理", "容量", "扩容", "映射", "缺页", "零拷贝", "大文件", "随机访问", "进程",
    "句柄", "地址", "写入", "读取", "分词", "二元切分", "二元组", "词表", "召回", "噪声", "新词",
    "中文", "索引", "倒排", "倒排索引", "段", "合并", "标记", "回收", "空间", "排序", "公式",
    "文档长度", "词频", "链接", "服务器", "部署", "方案", "项目", "截图", "工具", "识别", "文字",
    "编译", "优化", "输出", "交叉", "目录", "目标", "端口", "提交", "回退", "搜索", "关键字",
    "行号", "后台", "常驻", "占用", "状态", "报错", "依赖", "本地", "离线", "上传", "隐私",
    "数据", "文件", "终端", "聊天", "聊天记录", "历史", "记录", "笔记", "架构",
];

/// 把文本切成片段：`(片段, 是否 ASCII 字母数字下划线)`。
/// 其他字符（标点、空白、表情）作为分隔符丢弃。
pub fn segments(text: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    let mut cur_ascii = false;

    for c in text.chars() {
        let class = if c.is_ascii_alphanumeric() || c == '_' {
            Some(true)
        } else if ('\u{4e00}'..='\u{9fff}').contains(&c) {
            Some(false)
        } else {
            None
        };

        match class {
            None => {
                if !cur.is_empty() {
                    out.push((std::mem::take(&mut cur), cur_ascii));
                }
            }
            Some(a) => {
                if cur.is_empty() {
                    cur_ascii = a;
                }
                if a == cur_ascii {
                    cur.push(c);
                } else {
                    out.push((std::mem::take(&mut cur), cur_ascii));
                    cur_ascii = a;
                    cur.push(c);
                }
            }
        }
    }
    if !cur.is_empty() {
        out.push((cur, cur_ascii));
    }
    out
}

/// 词级分词：中文用最大正向匹配查词典，未命中落单字；英文数字按边界切并转小写。
pub fn word_tokens(text: &str, dict: &Dict) -> Vec<String> {
    let mut out = Vec::new();
    for (seg, is_ascii) in segments(text) {
        if is_ascii {
            out.push(seg.to_ascii_lowercase());
            continue;
        }
        let chars: Vec<char> = seg.chars().collect();
        let n = chars.len();
        let mut i = 0;
        while i < n {
            let max_len = std::cmp::min(4, n - i);
            let mut hit: Option<(String, usize)> = None;
            for l in (1..=max_len).rev() {
                let w: String = chars[i..i + l].iter().collect();
                if dict.contains(&w) {
                    hit = Some((w, l));
                    break;
                }
            }
            match hit {
                Some((w, l)) => {
                    out.push(w);
                    i += l;
                }
                None => {
                    // 未登录字直接跳过，**不产出单字 token**。
                    //
                    // 这是实测出来的坑：单字几乎无区分度。查询"完全不存在的词zzz"
                    // 会拆出 完 / 全 / 存 / 词，其中「完」命中"编译完成"，「全」命中"全部"，
                    // 导致一个毫无意义的查询返回了结果。
                    //
                    // 召回不会因此丢失 —— 未登录序列由 `bigram_tokens` 兜底。
                    // 职责划分：词级 = 精确排序，bigram = 兜底召回。
                    i += 1;
                }
            }
        }
    }
    out
}

/// bigram：中文连续段切相邻二元组，单字保留原样；英文按边界切原词。
pub fn bigram_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (seg, is_ascii) in segments(text) {
        if is_ascii {
            out.push(seg.to_ascii_lowercase());
            continue;
        }
        let chars: Vec<char> = seg.chars().collect();
        if chars.len() == 1 {
            out.push(chars[0].to_string());
            continue;
        }
        for w in chars.windows(2) {
            out.push(w.iter().collect::<String>());
        }
    }
    out
}

/// 融合：词级（精确排序）+ bigram（兜底召回）。
pub fn fused_tokens(text: &str, dict: &Dict) -> Vec<String> {
    let mut out = word_tokens(text, dict);
    out.extend(bigram_tokens(text));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_split_ascii_and_cjk() {
        let s = segments("rust内存 mmap_64 管理");
        assert_eq!(
            s,
            vec![
                ("rust".to_string(), true),
                ("内存".to_string(), false),
                ("mmap_64".to_string(), true),
                ("管理".to_string(), false),
            ]
        );
    }

    #[test]
    fn word_tokens_max_match() {
        let d = Dict::new();
        assert_eq!(word_tokens("内存管理", &d), vec!["内存", "管理"]);
    }

    #[test]
    fn unmatched_chars_produce_no_token() {
        let d = Dict::new();
        // 未登录字不产出单字（噪声抑制）
        assert_eq!(word_tokens("龘龘", &d), Vec::<String>::new());
        // 但 bigram 必须接住，保证召回不丢
        assert_eq!(bigram_tokens("龘龘"), vec!["龘龘"]);
    }

    #[test]
    fn noisy_single_chars_are_suppressed() {
        let d = Dict::new();
        let t = word_tokens("完全不存在的词", &d);
        assert!(!t.iter().any(|x| x == "完"), "「完」会命中「编译完成」: {:?}", t);
        assert!(!t.iter().any(|x| x == "全"), "「全」会命中「全部」: {:?}", t);
    }

    #[test]
    fn bigram_produces_pairs() {
        assert_eq!(bigram_tokens("内存"), vec!["内存"]);
        assert_eq!(bigram_tokens("内存管理"), vec!["内存", "存管", "管理"]);
        // 英文不切二元组
        assert_eq!(bigram_tokens("rust"), vec!["rust"]);
    }

    #[test]
    fn fused_contains_both() {
        let d = Dict::new();
        let t = fused_tokens("内存管理", &d);
        assert!(t.contains(&"内存".to_string()));
        assert!(t.contains(&"存管".to_string()));
    }
}
