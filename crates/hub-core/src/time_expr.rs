//! 查询中的时间表达式解析。
//!
//! **这是一个真实踩过的坑**：第一版只做了全局时间衰减，从未解析查询里的时间词，
//! 结果 "上个月写的那个 rust 内存代码片段" 返回了 22 天前的笔记，压过 40 天前真正的代码。
//! 第二版正则只写 `(\d+)个月前`，匹配不到"三**个月前"（中文数字），再次失效。
//!
//! 所以这里两个能力都必须有：中文数字转换 + 固定相对词。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub min_days: u32,
    pub max_days: u32,
}

impl TimeRange {
    pub fn contains(&self, days: u32) -> bool {
        days >= self.min_days && days <= self.max_days
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Day,
    Week,
    Month,
    Year,
}

/// 单个中文数字 -> 数值。注意 '两' 也可作数量（两个月前）。
fn cn_digit(c: char) -> Option<u32> {
    match c {
        '零' | '〇' => Some(0),
        '一' => Some(1),
        '二' | '两' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        _ => None,
    }
}

/// 支持 "3" / "三" / "十" / "十二" / "二十" / "二十一"
pub fn cn2int(s: &str) -> Option<u32> {
    if let Ok(n) = s.parse::<u32>() {
        return Some(n);
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return None;
    }
    match chars.iter().position(|&c| c == '十') {
        Some(pos) => {
            let hi = if pos == 0 {
                1
            } else {
                cn_digit(chars[pos - 1])?
            };
            let lo = if pos + 1 < chars.len() {
                cn_digit(chars[pos + 1])?
            } else {
                0
            };
            Some(hi * 10 + lo)
        }
        None => {
            if chars.len() == 1 {
                cn_digit(chars[0])
            } else {
                None
            }
        }
    }
}

const FIXED: &[(&str, u32, u32)] = &[
    ("前天", 2, 3),
    ("昨天", 1, 2),
    ("今天", 0, 1),
    ("上个星期", 7, 14),
    ("上周", 7, 14),
    ("本周", 0, 7),
    ("这周", 0, 7),
    ("上个月", 30, 60),
    ("上月", 30, 60),
    ("这个月", 0, 30),
    ("本月", 0, 30),
];

/// 解析 "N天前 / N周前 / N个月前 / N年前"，N 支持阿拉伯数字与中文数字。
/// 例："三个月前" -> (75,105)，"3 天前" -> (1,5)，"两个星期前" -> (10,18)
fn parse_n_before(q: &str) -> Option<TimeRange> {
    find_n_before(q).map(|(_, _, r)| r)
}

/// 同 [`parse_n_before`]，额外返回该表达式在字符序列中的下标区间 `[start, end]`（含两端），
/// 供 [`strip_time_expr`] 把这段时间词从查询里摘掉。
fn find_n_before(q: &str) -> Option<(usize, usize, TimeRange)> {
    let chars: Vec<char> = q.chars().collect();
    for i in 0..chars.len() {
        if chars[i] != '前' {
            continue;
        }
        // 往回跳过空白，定位单位字
        let mut j = i;
        while j > 0 && chars[j - 1].is_whitespace() {
            j -= 1;
        }
        if j == 0 {
            continue;
        }
        let unit = match chars[j - 1] {
            '年' => Unit::Year,
            '月' => Unit::Month,
            '周' => Unit::Week,
            '期' => Unit::Week, // "星期前"
            '天' | '日' => Unit::Day,
            _ => continue,
        };
        // 往回跳过修饰字（个 / 星）与空白
        let mut k = j - 1;
        while k > 0 && (chars[k - 1] == '个' || chars[k - 1] == '星' || chars[k - 1].is_whitespace())
        {
            k -= 1;
        }
        // 收集数字
        //
        // ⚠️ 必须把 '十' 也算作数字字符。`cn_digit('十')` 返回 None（它只是个位权），
        // 所以第一版写成 `cn_digit(c).is_some()` 时，收集循环会在"十"处直接断掉：
        //   "十二个月前" -> 只收到 "二"  -> 2 个月（错 10 个月）
        //   "二十一天前" -> 只收到 "一"  -> 1 天（错 20 天）
        //   "十个月前"   -> 收到空串    -> 整个时间约束丢失
        // cn2int() 本身是支持"十"的，问题只在这层收集。
        let mut num = String::new();
        while k > 0 {
            let c = chars[k - 1];
            if c.is_ascii_digit() || c == '十' || cn_digit(c).is_some() {
                num.insert(0, c);
                k -= 1;
            } else {
                break;
            }
        }
        if num.is_empty() {
            continue;
        }
        // 注意这里不能写 `cn2int(&num)?` —— 那会让一个解析失败的"前"提前终止整个函数，
        // 后面的合法表达式再也扫不到。
        let n = match cn2int(&num) {
            Some(n) => n,
            None => continue,
        };
        let center = match unit {
            Unit::Day => n,
            Unit::Week => n * 7,
            Unit::Month => n * 30,
            Unit::Year => n * 365,
        };
        let slack = match unit {
            Unit::Day => 2.min(n),
            Unit::Week => 4,
            Unit::Month => 15,
            Unit::Year => 60,
        };
        return Some((
            k,
            i,
            TimeRange {
                min_days: center.saturating_sub(slack),
                max_days: center + slack,
            },
        ));
    }
    None
}

/// 把查询里的时间表达式剔除，返回"裸意图"。
///
/// 为什么需要它：`parse_query` 原本把整串查询都丢进分词，于是"上个月写的那个rust…"会
/// 额外产出「上个 / 个月 / 写的 / 那个」这类噪声 token，既稀释 BM25，又让
/// `ParsedQuery::residue` 名不副实（文档说它是"去掉时间/来源/短语后的裸意图"）。
pub fn strip_time_expr(q: &str) -> String {
    let chars: Vec<char> = q.chars().collect();
    let mut drop = vec![false; chars.len()];

    if let Some((s, e, _)) = find_n_before(q) {
        for d in drop.iter_mut().take(e + 1).skip(s) {
            *d = true;
        }
    }
    for (kw, _, _) in FIXED {
        let k: Vec<char> = kw.chars().collect();
        let mut i = 0;
        while i + k.len() <= chars.len() {
            if chars[i..i + k.len()] == k[..] {
                for d in drop.iter_mut().skip(i).take(k.len()) {
                    *d = true;
                }
                i += k.len();
            } else {
                i += 1;
            }
        }
    }

    chars
        .iter()
        .enumerate()
        .filter(|(i, _)| !drop[*i])
        .map(|(_, c)| *c)
        .collect::<String>()
        .trim()
        .to_string()
}

/// 解析查询里的相对时间，返回天数区间。
pub fn parse_time(q: &str) -> Option<TimeRange> {
    if let Some(r) = parse_n_before(q) {
        return Some(r);
    }
    for (kw, lo, hi) in FIXED {
        if q.contains(kw) {
            return Some(TimeRange {
                min_days: *lo,
                max_days: *hi,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cn_number_conversion() {
        assert_eq!(cn2int("三"), Some(3));
        assert_eq!(cn2int("十"), Some(10));
        assert_eq!(cn2int("十二"), Some(12));
        assert_eq!(cn2int("二十"), Some(20));
        assert_eq!(cn2int("两"), Some(2));
        assert_eq!(cn2int("3"), Some(3));
    }

    // 这一条是第一版正则 `(\d+)个月前` 直接漏掉的
    #[test]
    fn chinese_numeral_month() {
        assert_eq!(
            parse_time("三个月前那次编译报错截图"),
            Some(TimeRange { min_days: 75, max_days: 105 })
        );
    }

    #[test]
    fn arabic_with_space() {
        assert_eq!(
            parse_time("3 个月前的记录"),
            Some(TimeRange { min_days: 75, max_days: 105 })
        );
        assert_eq!(
            parse_time("3天前"),
            Some(TimeRange { min_days: 1, max_days: 5 })
        );
    }

    #[test]
    fn week_and_year() {
        assert_eq!(
            parse_time("两个星期前"),
            Some(TimeRange { min_days: 10, max_days: 18 })
        );
        assert_eq!(
            parse_time("1年前"),
            Some(TimeRange { min_days: 305, max_days: 425 })
        );
    }

    #[test]
    fn fixed_words() {
        assert_eq!(
            parse_time("上个月写的那个代码"),
            Some(TimeRange { min_days: 30, max_days: 60 })
        );
        assert_eq!(
            parse_time("昨天看的方案"),
            Some(TimeRange { min_days: 1, max_days: 2 })
        );
    }

    #[test]
    fn no_time_expression() {
        assert_eq!(parse_time("rust 内存分配示例"), None);
    }

    // 下面四条是"十"收集缺陷的回归测试（第一版全部失败）
    #[test]
    fn teen_and_twenty_numeral() {
        assert_eq!(
            parse_time("十二个月前"),
            Some(TimeRange { min_days: 345, max_days: 375 })
        );
        assert_eq!(
            parse_time("十个月前"),
            Some(TimeRange { min_days: 285, max_days: 315 })
        );
        assert_eq!(
            parse_time("二十一天前"),
            Some(TimeRange { min_days: 19, max_days: 23 })
        );
        assert_eq!(
            parse_time("二十五天前"),
            Some(TimeRange { min_days: 23, max_days: 27 })
        );
    }

    #[test]
    fn thirty_days_and_years() {
        assert_eq!(
            parse_time("三十天前"),
            Some(TimeRange { min_days: 28, max_days: 32 })
        );
        assert_eq!(
            parse_time("两年前"),
            Some(TimeRange { min_days: 670, max_days: 790 })
        );
    }
}
