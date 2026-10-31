//! 文件系统 connector：递归扫描 + 跳过规则 + 二进制嗅探。

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// 内置跳过的目录名（小写比较）。
///
/// 这些目录的共同点：内容极多、价值极低，扫进去会把索引撑爆，
/// 而且每次全量 reconcile 都要重新遍历一遍。
const SKIP_DIRS: &[&str] = &[
    // VCS
    ".git",
    ".svn",
    ".hg",
    // 依赖 / 构建产物
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "vendor",
    ".venv",
    "venv",
    "site-packages",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".gradle",
    ".next",
    ".nuxt",
    ".cache",
    "registry",      // ~/.cargo/registry
    "packages",      // 各类包缓存
    // 编辑器 / IDE
    ".idea",
    ".vs",
    ".vscode",
    // 系统
    "appdata",
    "windows",
    "program files",
    "program files (x86)",
    "$recycle.bin",
    "system volume information",
    "library",       // macOS ~/Library
    ".trash",
    // 本产品自己的数据目录，别把自己索引进去
    ".hub",
];

/// 二进制扩展名（小写比较）。不靠内容嗅探也能挡掉一大半。
const SKIP_EXTS: &[&str] = &[
    "exe", "dll", "so", "dylib", "bin", "o", "obj", "lib", "a", "pdb", "ilk", "class", "jar",
    "pyc", "pyd", "wasm", "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "zst", "iso", "dmg",
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tiff", "psd", "mp3", "wav", "flac",
    "mp4", "mkv", "avi", "mov", "webm", "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
    "ttf", "otf", "woff", "woff2", "db", "sqlite", "sqlite3", "lock.tmp",
];

#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// 单个文件最大入索引字节数。超过直接跳过 —— 别让一个 2GB 日志把内存吃穿。
    pub max_file_bytes: u64,
    /// 递归深度上限，防软链环与畸形目录
    pub max_depth: usize,
    /// 是否跟随符号链接（默认否：绝大多数时候跟随只会制造环路）
    pub follow_symlinks: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: 2 * 1024 * 1024,
            max_depth: 32,
            follow_symlinks: false,
        }
    }
}

impl WalkOptions {
    fn skip_dir(&self, name: &str) -> bool {
        let l = name.to_ascii_lowercase();
        SKIP_DIRS.contains(&l.as_str())
    }

    fn skip_ext(&self, path: &Path) -> bool {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) => {
                let l = ext.to_ascii_lowercase();
                SKIP_EXTS.contains(&l.as_str())
            }
            None => false,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct WalkStats {
    pub dirs: usize,
    pub files: usize,
    pub skipped_dir: usize,
    pub skipped_ext: usize,
    pub skipped_size: usize,
    pub skipped_symlink: usize,
    /// 无权限 / 文件被占用 / 读到一半被删 —— 一律计数，不中断扫描
    pub errors: usize,
}

#[derive(Debug, Clone)]
pub struct FileHit {
    pub path: PathBuf,
    pub size: u64,
    /// unix 秒。0 表示取不到（极端情况），交给上层处理。
    pub mtime: i64,
}

/// 递归遍历 `root`，对每个候选文件回调一次 `on_file`。
///
/// 用显式栈而不是递归：深度 32 层 + 每层若干目录，递归写法在 Windows 上很容易爆栈。
pub fn walk<F>(root: &Path, opts: &WalkOptions, mut on_file: F) -> io::Result<WalkStats>
where
    F: FnMut(FileHit),
{
    let mut stats = WalkStats::default();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

    while let Some((dir, depth)) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        stats.dirs += 1;

        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            };
            let path = entry.path();
            // symlink_metadata：不跟随链接，否则 is_dir() 会对链接目标做判断，产生环路
            let md = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            };
            let ft = md.file_type();

            if ft.is_symlink() && !opts.follow_symlinks {
                stats.skipped_symlink += 1;
                continue;
            }
            if ft.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if opts.skip_dir(&name) || depth + 1 > opts.max_depth {
                    stats.skipped_dir += 1;
                    continue;
                }
                stack.push((path, depth + 1));
            } else if ft.is_file() {
                if md.len() == 0 || md.len() > opts.max_file_bytes {
                    stats.skipped_size += 1;
                    continue;
                }
                if opts.skip_ext(&path) {
                    stats.skipped_ext += 1;
                    continue;
                }
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                stats.files += 1;
                on_file(FileHit { path, size: md.len(), mtime });
            }
        }
    }

    Ok(stats)
}

/// 读文本。返回 `None` 表示"这不是一段值得索引的文本"：
/// 太大 / 空 / 含 NUL 字节（二进制）/ 不是合法 UTF-8。
///
/// 非 UTF-8 直接放弃而不是 lossy 转换：GBK 内容转成 replacement char 后
/// 全是 `�`，既搜不到又占倒排 —— 宁可漏，不要脏。GBK 识别留待后续。
pub fn read_text(path: &Path, max_bytes: u64) -> io::Result<Option<String>> {
    let md = fs::metadata(path)?;
    if md.len() == 0 || md.len() > max_bytes {
        return Ok(None);
    }
    let mut buf = Vec::with_capacity(md.len() as usize);
    File::open(path)?.take(max_bytes).read_to_end(&mut buf)?;

    // 只看前 8KB：真正的二进制文件在开头就有 NUL
    if buf.iter().take(8192).any(|b| *b == 0) {
        return Ok(None);
    }
    match String::from_utf8(buf) {
        Ok(s) => Ok(Some(s)),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn touch(dir: &Path, rel: &str, content: &str) -> PathBuf {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn walks_and_skips_noise_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "a.txt", "内存管理");
        touch(root, "sub/b.rs", "fn main() {}");
        touch(root, "node_modules/pkg/index.js", "console.log(1)");
        touch(root, "target/debug/x.rs", "wrong");
        touch(root, "logo.png", "not really a png");

        let mut seen: Vec<String> = Vec::new();
        let stats = walk(root, &WalkOptions::default(), |h| {
            seen.push(h.path.file_name().unwrap().to_string_lossy().to_string());
        })
        .unwrap();

        seen.sort();
        assert_eq!(seen, vec!["a.txt", "b.rs"]);
        assert!(stats.skipped_dir >= 2, "node_modules / target 应被跳过");
        assert!(stats.skipped_ext >= 1, "png 应被跳过");
    }

    #[test]
    fn rejects_binary_and_oversize() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = touch(tmp.path(), "bin.dat", "");
        fs::write(&bin, [0u8, 1, 2, 3, 4]).unwrap();

        assert!(read_text(&bin, 1024).unwrap().is_none(), "含 NUL 应判为二进制");

        let big = tmp.path().join("big.txt");
        fs::write(&big, "x".repeat(4096)).unwrap();
        assert!(read_text(&big, 1024).unwrap().is_none(), "超限应跳过");

        let ok = touch(tmp.path(), "ok.txt", "零拷贝 mmap");
        assert_eq!(read_text(&ok, 1024).unwrap().as_deref(), Some("零拷贝 mmap"));
    }

    #[test]
    fn rejects_non_utf8() {
        let tmp = tempfile::tempdir().unwrap();
        let gbk = tmp.path().join("gbk.txt");
        // 0xD6 0xD0 是 GBK 的"中"，不是合法 UTF-8
        fs::write(&gbk, [0xD6u8, 0xD0, 0xCE, 0xC4]).unwrap();
        assert!(read_text(&gbk, 1024).unwrap().is_none());
    }

    #[test]
    fn depth_limit_holds() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "d1/d2/d3/deep.txt", "deep");
        let opts = WalkOptions { max_depth: 1, ..Default::default() };

        let mut count = 0;
        walk(tmp.path(), &opts, |_| count += 1).unwrap();
        assert_eq!(count, 0, "超过深度上限的文件不应被访问");
    }
}
