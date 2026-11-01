//! 日志。写文件，可选同时回显到终端。
//!
//! 守护进程是脱离控制台运行的，`println!` 在那种环境下会直接丢（或让写入失败），
//! 所以日志必须落在文件里。前台模式（`hub daemon run`）额外回显一份，
//! 免得上线前调试还得 `tail -f`。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::state::fmt_time;

/// 超过这个大小就滚动一次。daemon 一天也就写几十 KB，5MB 够用好几个月。
const MAX_BYTES: u64 = 5 * 1024 * 1024;

pub struct Log {
    path: PathBuf,
    file: Option<File>,
    /// 前台运行时为 true
    echo: bool,
}

impl Log {
    pub fn open(path: PathBuf, echo: bool) -> Self {
        let file = Self::open_file(&path);
        Self { path, file, echo }
    }

    /// 不落盘的日志。测试里起 IPC 服务端要用它 —— 否则每跑一次测试
    /// 就往真实 daemon.log 里写一堆东西。
    pub fn silent() -> Self {
        Self {
            path: PathBuf::new(),
            file: None,
            echo: false,
        }
    }

    fn open_file(path: &Path) -> Option<File> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // 滚动：保留了旧日志就没法 append，先改名再新建
        if let Ok(md) = std::fs::metadata(path) {
            if md.len() > MAX_BYTES {
                let _ = std::fs::rename(path, path.with_extension("log.1"));
            }
        }
        OpenOptions::new().create(true).append(true).open(path).ok()
    }

    fn write(&mut self, level: &str, msg: &str) {
        let line = format!("{} [{}] {}\n", fmt_time(now()), level, msg);
        if self.echo {
            // 前台模式下 stderr 用于日志，stdout 留给可能的结构化输出
            eprint!("{line}");
        }

        let mut ok = false;
        if let Some(f) = self.file.as_mut() {
            ok = f.write_all(line.as_bytes()).is_ok() && f.flush().is_ok();
        }
        if !ok {
            // silent() 没有路径，不必每写一次就试着开一次文件
            if self.path.as_os_str().is_empty() {
                return;
            }
            // 磁盘满 / 文件被占用：重新开一次，仍失败就静默丢弃 ——
            // 日志不该把主循环搞崩
            let path = self.path.clone();
            self.file = Self::open_file(&path);
            if let Some(f) = self.file.as_mut() {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
        }
    }

    pub fn info(&mut self, msg: impl AsRef<str>) {
        self.write("INFO", msg.as_ref());
    }

    pub fn warn(&mut self, msg: impl AsRef<str>) {
        self.write("WARN", msg.as_ref());
    }

    pub fn error(&mut self, msg: impl AsRef<str>) {
        self.write("ERROR", msg.as_ref());
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
