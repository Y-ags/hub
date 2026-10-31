//! 文件系统监听。
//!
//! ## 为什么不用 notify crate
//!
//! 这个项目在**完全离线**的环境里编译（`cargo --offline`），而 crates.io 缓存里
//! 没有 `notify`。更实际的理由是：`notify` 在 Windows 上就是对
//! `ReadDirectoryChangesW` 的一层薄封装，真正要处理的麻烦事（缓冲区溢出、
//! 重命名拆成两条、同步阻塞调用没法取消）它一样要你自己面对。
//! 直接调这个 API 的代码量和引一个依赖差不多，但少了 20 个传递依赖。
//!
//! ## 事件模型的简化
//!
//! **只有三种事件**，而且都只携带路径，不携带"这是文件还是目录"：
//!
//! | 事件 | 何时 |
//! |---|---|
//! | [`FsEvent::Upsert`] | 新增 / 内容修改 / 重命名到 |
//! | [`FsEvent::Remove`] | 删除 / 重命名离开 |
//! | [`FsEvent::Overflow`] | 缓冲区溢出，**必须全量对账** |
//!
//! 不做"文件/目录"的判断是有意的：Windows 的 `FILE_ACTION_REMOVED` 事件里
//! 根本没有这个信息，硬判断就要在事件线程里 `stat`（额外 IO，且文件此时
//! 已经不存在了）。反正 daemon 处理事件时本来就要 `stat` —— 到那时判断更准确。
//!
//! ## 非 Windows
//!
//! 用目录快照轮询兜底。不是为了性能，是为了让 `cargo test` 在 Linux CI 上能跑 ——
//! 这个项目的目标平台是 Windows，但代码不该因此变得不可测。

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// 监听到的变更。只带路径，不带类型 —— 见模块文档。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEvent {
    /// 新增、内容修改、重命名到
    Upsert(PathBuf),
    /// 删除、重命名离开
    Remove(PathBuf),
    /// 内核缓冲区溢出：这一段时间的事件全丢了，只能全量对账
    Overflow,
    /// 某个 root 根本监听不了（目录不存在 / 权限不足 / 网络盘断开）。
    /// 必须让上层知道：静默失败会让人以为"后台在盯着"，其实什么也没听。
    Error(String),
}

/// 一个 root 的监听器集合。drop 时自动停止所有监听线程。
pub struct FsWatcher {
    rx: Receiver<FsEvent>,
    _inner: platform::Inner,
}

impl FsWatcher {
    /// 对每个 root 起一个监听。`roots` 里不存在的路径直接跳过（不报错：
    /// 用户配置里写了一个还没创建的目录是常事，daemon 不该因此拒绝启动）。
    pub fn start(roots: &[PathBuf], poll_interval: Duration) -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let inner = platform::Inner::start(roots, tx, poll_interval)?;
        Ok(Self { rx, _inner: inner })
    }

    /// 阻塞等待下一条事件，最多等 `timeout`。
    ///
    /// 返回 `Ok(())` 表示 `out` 里至少有一条事件（可能一次拿到一大批）；
    /// 返回 `Err(Timeout)` 表示这段时间内没有任何变化。
    pub fn drain_into(
        &self,
        out: &mut Vec<FsEvent>,
        timeout: Duration,
    ) -> Result<(), RecvTimeoutError> {
        // 先阻塞等第一条，避免空转
        let first = self.rx.recv_timeout(timeout)?;
        out.push(first);
        // 再尽量把已到达的取干净，避免一次变更被拆成多轮处理
        while let Ok(e) = self.rx.try_recv() {
            out.push(e);
            if out.len() >= 8192 {
                break;
            }
        }
        Ok(())
    }
}

// ============================================================
// Windows：ReadDirectoryChangesW
// ============================================================

#[cfg(windows)]
mod platform {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicUsize, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::IO::CancelSynchronousIo;
    use windows_sys::Win32::System::Threading::{GetCurrentThreadId, OpenThread, THREAD_TERMINATE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
        FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };

    use super::FsEvent;

    /// 单次读缓冲。64KB 大约能装 1000 条事件；再大就要考虑事件线程的栈开销了。
    /// 缓冲越小越容易溢出，溢出会退化成全量对账（正确但慢）。
    const BUF_BYTES: usize = 64 * 1024;

    pub(super) struct Inner {
        /// 存的是原始 HANDLE 值。`-1` 表示"已由停止逻辑关闭"。
        /// 用原子量而不是 `Mutex<HANDLE>`，是因为停止发生在另一个线程，
        /// 而监听线程正阻塞在 `ReadDirectoryChangesW` 里拿不到锁。
        handles: Vec<Arc<AtomicIsize>>,
        /// 每个监听线程的 tid，用来调 `CancelSynchronousIo`
        tids: Vec<Arc<AtomicU32>>,
        threads: Vec<JoinHandle<()>>,
    }

    impl Inner {
        pub(super) fn start(
            roots: &[PathBuf],
            tx: Sender<FsEvent>,
            _poll: Duration,
        ) -> std::io::Result<Self> {
            let mut handles = Vec::new();
            let mut tids = Vec::new();
            let mut threads = Vec::new();

            for root in roots {
                if !root.is_dir() {
                    continue;
                }
                let root = root.clone();
                let slot = Arc::new(AtomicIsize::new(0));
                let tid = Arc::new(AtomicU32::new(0));
                let (slot2, tid2) = (slot.clone(), tid.clone());
                let tx2 = tx.clone();

                let th = std::thread::Builder::new()
                    .name(format!("hub-watch-{}", root.display()))
                    .spawn(move || watch_root(root, slot2, tid2, tx2))?;
                handles.push(slot);
                tids.push(tid);
                threads.push(th);
            }

            Ok(Self { handles, tids, threads })
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            // ⚠️ 这里必须用 CancelSynchronousIo，不能只 CloseHandle：
            // 同步模式的 ReadDirectoryChangesW **不会**因为句柄被关闭而返回，
            // 线程会永远卡在系统调用里，`join()` 也就永远不返回 ——
            // 表现是日志已经打了"已退出"，进程却还挂在任务管理器里。
            for (h, t) in self.handles.iter().zip(self.tids.iter()) {
                let tid = t.load(Ordering::SeqCst);
                if tid != 0 {
                    unsafe {
                        let th = OpenThread(THREAD_TERMINATE, 0, tid);
                        if !th.is_null() {
                            CancelSynchronousIo(th);
                            CloseHandle(th);
                        }
                    }
                }
                let v = h.swap(-1, Ordering::SeqCst);
                if v > 0 {
                    unsafe { CloseHandle(v as HANDLE) };
                }
            }

            // 给取消一个短暂的生效窗口，然后**不等 join**。
            // 即使取消没命中（线程恰好还没进入阻塞），也不能让 Drop 卡住进程退出；
            // 监听线程持有的是主循环 channel 的发送端，主循环退出后它 send 失败自然会结束。
            std::thread::sleep(Duration::from_millis(20));
            for t in self.threads.drain(..) {
                std::mem::forget(t);
            }
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        OsStr::new(path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn watch_root(
        root: PathBuf,
        handle_slot: Arc<AtomicIsize>,
        tid_slot: Arc<AtomicU32>,
        tx: Sender<FsEvent>,
    ) {
        // 先把 tid 记下来：停止时需要它去调 CancelSynchronousIo
        tid_slot.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);

        let w = wide(&root);
        let handle = unsafe {
            CreateFileW(
                w.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                // 目录必须以 BACKUP_SEMANTICS 打开，否则 CreateFile 直接失败
                FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // 打不开就放弃这个 root，但要把这件事告诉上层
            let _ = tx.send(FsEvent::Error(format!(
                "无法监听目录（CreateFileW 失败）：{}",
                root.display()
            )));
            return;
        }
        handle_slot.store(handle as isize, Ordering::SeqCst);

        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_LAST_WRITE
            | FILE_NOTIFY_CHANGE_SIZE;

        let mut buf: Vec<u8> = vec![0; BUF_BYTES];
        // 记录已经报过的 Overflow，避免一次溢出风暴刷屏
        let overflow_flag = AtomicUsize::new(0);

        loop {
            let mut returned: u32 = 0;
            let ok = unsafe {
                ReadDirectoryChangesW(
                    handle,
                    buf.as_mut_ptr() as *mut _,
                    buf.len() as u32,
                    1, // 递归监听子树
                    filter,
                    &mut returned,
                    ptr::null_mut(),
                    None,
                )
            };

            if ok == 0 {
                // 句柄被 stop() 关掉了，或者目录被卸载 —— 两种情况都该退出
                break;
            }
            if returned == 0 {
                // 缓冲区溢出：这段时间的变更全丢了。上层必须做一次全量对账。
                if overflow_flag.swap(1, Ordering::SeqCst) == 0 {
                    if tx.send(FsEvent::Overflow).is_err() {
                        break;
                    }
                }
                continue;
            }
            overflow_flag.store(0, Ordering::SeqCst);

            let mut offset = 0usize;
            loop {
                // ⚠️ 这里**不能**用 `read_unaligned` 把 FILE_NOTIFY_INFORMATION
                // 整个拷贝出来再取 `fni.FileName.as_ptr()`：结构体里的 FileName 是
                // `[u16; 1]`，拷贝到栈上之后这个指针指向的是**副本里那一个元素**，
                // 后面全是从栈上读到的垃圾。表现是文件名只剩一两个字符、
                // 末尾挂着一串乱码，stat 报 "strings passed to WinAPI cannot contain NULs"。
                //
                // 字段布局（u32 对齐，无填充）：
                //   +0  NextEntryOffset
                //   +4  Action
                //   +8  FileNameLength（字节数，不是字符数）
                //   +12 FileName（UTF-16，宽度可变）
                const NEXT: usize = 0;
                const ACTION: usize = 4;
                const NAME_LEN: usize = 8;
                const NAME: usize = 12;

                let base = unsafe { buf.as_ptr().add(offset) };
                let next_entry = unsafe { ptr::read_unaligned(base.add(NEXT) as *const u32) };
                let action = unsafe { ptr::read_unaligned(base.add(ACTION) as *const u32) };
                let name_bytes =
                    unsafe { ptr::read_unaligned(base.add(NAME_LEN) as *const u32) } as usize;

                let name = if name_bytes == 0 {
                    String::new()
                } else {
                    let units = name_bytes / 2;
                    // 防越界：损坏的事件包可能给出一个超过缓冲区的长度
                    if offset + NAME + name_bytes > returned as usize {
                        break;
                    }
                    let slice =
                        unsafe { std::slice::from_raw_parts(base.add(NAME) as *const u16, units) };
                    String::from_utf16_lossy(slice)
                };

                if !name.is_empty() {
                    let full = root.join(name.replace('\\', "/"));
                    let ev = match action {
                        FILE_ACTION_ADDED | FILE_ACTION_MODIFIED | FILE_ACTION_RENAMED_NEW_NAME => {
                            Some(FsEvent::Upsert(full))
                        }
                        FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME => {
                            Some(FsEvent::Remove(full))
                        }
                        _ => None,
                    };
                    if let Some(ev) = ev {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                }

                if next_entry == 0 {
                    break;
                }
                offset += next_entry as usize;
                if offset >= returned as usize {
                    break;
                }
            }
        }

        // 线程自己退出时也要负责关句柄，否则 stop 路径没走到就泄漏了。
        // swap 保证只有一方能拿到非 -1 的值：要么 stop() 关了，要么这里关。
        let v = handle_slot.swap(-1, Ordering::SeqCst);
        if v > 0 {
            unsafe { CloseHandle(v as HANDLE) };
        }
    }
}

// ============================================================
// 其他平台：目录快照轮询
// ============================================================

#[cfg(not(windows))]
mod platform {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, UNIX_EPOCH};

    use hub_connectors::{walk, WalkOptions};

    use super::FsEvent;

    pub(super) struct Inner {
        stop: Arc<AtomicBool>,
        threads: Vec<JoinHandle<()>>,
    }

    impl Inner {
        pub(super) fn start(
            roots: &[PathBuf],
            tx: Sender<FsEvent>,
            poll: Duration,
        ) -> std::io::Result<Self> {
            let stop = Arc::new(AtomicBool::new(false));
            let mut threads = Vec::new();
            let opts = WalkOptions::default();

            for root in roots {
                if !root.is_dir() {
                    continue;
                }
                let root = root.clone();
                let tx = tx.clone();
                let stop = stop.clone();
                let opts = opts.clone();
                let interval = if poll.is_zero() { Duration::from_secs(5) } else { poll };

                let th = std::thread::Builder::new()
                    .name(format!("hub-poll-{}", root.display()))
                    .spawn(move || {
                        let mut prev: HashMap<PathBuf, i64> = snapshot(&root, &opts);
                        while !stop.load(Ordering::SeqCst) {
                            std::thread::sleep(interval);
                            if stop.load(Ordering::SeqCst) {
                                break;
                            }
                            let now = snapshot(&root, &opts);
                            for (p, m) in &now {
                                match prev.get(p) {
                                    Some(old) if old == m => {}
                                    _ => {
                                        if tx.send(FsEvent::Upsert(p.clone())).is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                            for p in prev.keys() {
                                if !now.contains_key(p)
                                    && tx.send(FsEvent::Remove(p.clone())).is_err()
                                {
                                    return;
                                }
                            }
                            prev = now;
                        }
                    })?;
                threads.push(th);
            }

            Ok(Self { stop, threads })
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            for t in self.threads.drain(..) {
                let _ = t.join();
            }
        }
    }

    fn snapshot(root: &PathBuf, opts: &WalkOptions) -> HashMap<PathBuf, i64> {
        let mut m = HashMap::new();
        let _ = walk(root, opts, |hit| {
            let mt = std::fs::metadata(&hit.path)
                .ok()
                .and_then(|x| x.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            m.insert(hit.path, mt);
        });
        m
    }
}
