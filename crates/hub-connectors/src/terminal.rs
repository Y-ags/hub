//! 终端历史 connector：`.zsh_history` / `.bash_history` / PowerShell 历史。
//!
//! 三种格式：
//! - zsh 扩展格式：`: 1700000000:0;cargo build --release`
//! - bash + HISTTIMEFORMAT：`#1700000000` 单独一行，下一行才是命令
//! - 纯文本：没有时间戳，只能退化用文件 mtime

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub cmd: String,
    /// unix 秒；0 表示历史文件里没有时间戳
    pub ts: i64,
    /// 在历史文件里的行序号（1 起），用于生成稳定的 uri
    pub index: usize,
}

/// 解析历史文件文本，自动识别格式。
///
/// `fallback_ts`：无时间戳时统一用的时间（一般传文件 mtime）。
pub fn parse_history(text: &str, fallback_ts: i64) -> Vec<HistoryEntry> {
    let mut out: Vec<HistoryEntry> = Vec::new();
    let mut cur_ts = fallback_ts;
    let mut idx = 0usize;
    let mut pending_multiline: Option<usize> = None;

    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');

        // zsh 扩展格式
        if let Some(rest) = line.strip_prefix(": ") {
            if let Some((meta, cmd)) = rest.split_once(';') {
                let ts = meta
                    .split(':')
                    .next()
                    .and_then(|s| s.trim().parse::<i64>().ok());
                if let Some(ts) = ts {
                    cur_ts = ts;
                    idx += 1;
                    pending_multiline = None;
                    out.push(HistoryEntry { cmd: cmd.to_string(), ts: cur_ts, index: idx });
                    continue;
                }
            }
        }

        // bash 时间戳行
        if let Some(ts) = line.strip_prefix('#').and_then(|s| s.trim().parse::<i64>().ok()) {
            cur_ts = ts;
            continue;
        }

        if line.trim().is_empty() {
            continue;
        }

        // zsh 的多行命令以反斜杠续行，续到上一条上
        if let Some(i) = pending_multiline {
            out[i].cmd.push('\n');
            out[i].cmd.push_str(line);
            if !line.ends_with('\\') {
                pending_multiline = None;
            }
            continue;
        }

        idx += 1;
        out.push(HistoryEntry { cmd: line.to_string(), ts: cur_ts, index: idx });
        if line.ends_with('\\') {
            pending_multiline = Some(out.len() - 1);
        }
    }

    out
}

/// 噪声命令：占历史 60% 以上，索引进来只会稀释排序。
pub fn is_noise(cmd: &str) -> bool {
    let c = cmd.trim();
    if c.len() < 2 {
        return true;
    }
    let first = c.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "ls" | "ll" | "la" | "cd" | "pwd" | "clear" | "cls" | "exit" | "quit" | "q" | "history"
            | "top" | "htop" | "man" | "which" | "echo"
    ) || c.starts_with("cd ")
}

/// 找出本机存在的历史文件。找不到就返回空 —— 调用方不该假设一定有。
pub fn default_history_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);

    if let Some(h) = &home {
        for name in [".zsh_history", ".bash_history", ".sh_history"] {
            let p = h.join(name);
            if p.is_file() {
                out.push(p);
            }
        }
    }

    // PowerShell 的 PSReadLine 历史（Windows 上最有用的一份）
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let p = Path::new(&appdata)
            .join("Microsoft")
            .join("Windows")
            .join("PowerShell")
            .join("PSReadLine")
            .join("ConsoleHost_history.txt");
        if p.is_file() {
            out.push(p);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_zsh_extended_format() {
        let text = ": 1700000000:0;cargo build --release\n\
                    : 1700000100:0;export OHOS_NDK_HOME=/opt/ndk\n\
                    : 1700000200:0;rustup target add aarch64\n";
        let v = parse_history(text, 0);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].cmd, "cargo build --release");
        assert_eq!(v[0].ts, 1_700_000_000);
        assert_eq!(v[2].index, 3);
    }

    #[test]
    fn parses_bash_with_timestamps() {
        let text = "#1700000000\ngit reset --hard HEAD~1\n#1700000300\ngrep -rn bm25 .\n";
        let v = parse_history(text, 0);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].ts, 1_700_000_000);
        assert_eq!(v[1].cmd, "grep -rn bm25 .");
        assert_eq!(v[1].ts, 1_700_000_300);
    }

    #[test]
    fn plain_text_falls_back_to_file_mtime() {
        let v = parse_history("ls -al\ncargo test\n", 1_690_000_000);
        assert_eq!(v.len(), 2);
        assert!(v.iter().all(|e| e.ts == 1_690_000_000));
    }

    #[test]
    fn multiline_command_is_joined() {
        let v = parse_history("git commit \\\n  -m 'fix bug'\n", 0);
        assert_eq!(v.len(), 1);
        assert!(v[0].cmd.contains("fix bug"));
    }

    #[test]
    fn noise_filter() {
        assert!(is_noise("ls"));
        assert!(is_noise("cd .."));
        assert!(is_noise("c"));
        assert!(!is_noise("cargo build --release"));
        assert!(!is_noise("git reset --hard HEAD~1"));
    }
}
