//! 统一数据模型。所有 connector 无论数据从哪来，都必须压成 `Doc` 这一种结构，
//! 这样上层索引与查询完全不关心数据来源。

pub type DocId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceId {
    Files,
    Terminal,
    Chat,
    Ocr,
    Note,
}

impl SourceId {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceId::Files => "files",
            SourceId::Terminal => "terminal",
            SourceId::Chat => "chat",
            SourceId::Ocr => "ocr",
            SourceId::Note => "note",
        }
    }

    /// 解析显式语法 `source:xxx` / `in:xxx`，同时接受常见别名。
    pub fn parse(s: &str) -> Option<SourceId> {
        match s.to_ascii_lowercase().as_str() {
            "files" | "file" | "code" | "代码" => Some(SourceId::Files),
            "terminal" | "shell" | "终端" => Some(SourceId::Terminal),
            "chat" | "wechat" | "聊天" => Some(SourceId::Chat),
            "ocr" | "screenshot" | "截图" => Some(SourceId::Ocr),
            "note" | "笔记" => Some(SourceId::Note),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Doc {
    pub id: DocId,
    pub source: SourceId,
    /// 唯一标识：文件路径、消息内部链接、截图路径等
    pub uri: String,
    pub title: String,
    pub body: String,
    /// unix 秒
    pub mtime: i64,
    /// 内容指纹，用于判变更 / 去重 / 增量同步
    pub content_hash: u64,
    pub deleted: bool,
}

impl Doc {
    pub fn days_ago(&self, now: i64) -> u32 {
        let d = (now - self.mtime) / 86_400;
        if d < 0 {
            0
        } else {
            d as u32
        }
    }

    /// 被索引与检索的正文
    pub fn text(&self) -> String {
        let mut s = String::with_capacity(self.title.len() + self.body.len() + 1);
        s.push_str(&self.title);
        s.push(' ');
        s.push_str(&self.body);
        s
    }
}

/// connector 产出的变更事件
#[derive(Debug, Clone)]
pub enum Change {
    Upsert(Doc),
    Delete { source: SourceId, uri: String },
}

/// 大小写不敏感的子串匹配（只对 ASCII 生效，中文本来没有大小写）。
///
/// 为什么需要：`parse_query` 会把 `-RSS` 统一小写成 `rss`，而正文里写的是 `RSS`。
/// 用 `str::contains` 直接比，排除词永远匹配不到 —— 用户写了 `-RSS`，结果照旧返回那条，
/// 而且**不报错**，只是"排除了但没完全排除"。
pub fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    if needle.is_ascii() {
        // 只在需要时才分配：绝大多数查询没有排除词
        haystack.to_ascii_lowercase().contains(needle)
    } else {
        haystack.contains(needle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_ci_matches_both_directions() {
        assert!(contains_ci("RSS 明显下降", "rss"));
        assert!(contains_ci("rss 明显下降", "rss"));
        assert!(contains_ci("常驻内存太高", "内存"));
        assert!(!contains_ci("常驻内存太高", "零拷贝"));
        assert!(!contains_ci("anything", ""));
    }
}
