//! 写租约 —— 让"同一瞬间只有一个写者"成为结构上的保证，而不是运气。
//!
//! ## 为什么需要它
//!
//! 这个项目有三个写者：
//!
//! | 写者 | 什么时候写 |
//! |---|---|
//! | daemon 的 watcher / 对账 | 改了文件之后 1 秒 |
//! | `hub index` | 用户手动全量扫 |
//! | `hub merge` / `hub reset` | 用户手动维护 |
//!
//! 它们写的是**同一对东西**：`<data>/index`（Tantivy）和 `<data>/meta.sqlite`（SQLite）。
//!
//! Tantivy 的写入器锁是**硬互斥**的 —— `Index::writer()` 拿不到锁就直接返回
//! `LockFailure`，不排队、不重试。所以"两个写者碰上了"的真实表现是：
//! 后到的那个拿到一条 `Failed to acquire index lock`，这一批白干了
//! （数据要等下一轮对账才补回来，而用户看到的是"合并失败"）。
//!
//! 加租约之前能跑，是因为写入窗口只有几十毫秒，撞上的概率低。**靠概率不算设计**：
//! 一次 `hub index` 会连续 flush 几百批，撞上一次就够让用户以为工具坏了。
//!
//! ## 它是什么
//!
//! 一把**操作系统级的文件锁**（`<data>/writer.lock`，内容为空，纯粹用来锁）：
//!
//! - Windows：`File::lock` → `LockFileEx`
//! - Unix：`File::lock` → `flock`
//!
//! 进程退出（包括崩溃、被任务管理器结束）时内核自动释放。这一条是**不要自己用
//! `create_new` 造锁文件**的全部理由：那种锁在进程被强杀之后会永远留在磁盘上，
//! 用户接下来每次写入都会"等 30 秒然后失败"，而且没有任何线索指向那个残留文件。
//!
//! ## 它保护什么、不保护什么
//!
//! 保护：`Indexer::flush` / `merge` / `reset` 的整个临界区 —— 也就是
//! "往 Tantivy 写文档 + commit"和"改水位表"这两件事之间不会插进另一个写者。
//! 这才是"两个 commit 交错"唯一可能造成真实损坏的地方。
//!
//! 不保护：`MetaStore::alloc_doc_id`（一条独立的小事务）和所有读路径。
//! 前者由 SQLite 自己的事务 + `busy_timeout` 兜住，且它不会造成"索引与水位表
//! 不一致"，只是发个号；后者在 WAL 下与写者天然共存。
//!
//! 也不保护"读文件 → 加锁写入"之间的时间差：daemon 可能在加锁之前读到了旧版本，
//! 等它拿到锁时文件已经变了。这不是丢失，因为水位表记的是 `(mtime, content_hash)`，
//! 下一轮对账发现磁盘上的 mtime 与水位表不符就会按磁盘重读一遍，
//! 差异自我纠正。要彻底消除这个窗口，就得"读文件前先拿锁"，代价是
//! 一次几百毫秒的扫描期间 daemon 完全停摆 —— 不值得。
//!
//! ## 用法
//!
//! 一般情况下什么都不用做：`Indexer::flush` / `merge` / `reset` 内部已经拿了。
//! 只有想在动手前告诉用户"为什么在等"时，才需要 [`peek`]。

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::indexer::now_secs;
use crate::{Result, StoreError};

/// 锁文件。内容恒为空 —— 别往里写东西，Windows 的字节范围锁会挡住别的进程读它。
pub const LEASE_FILE: &str = "writer.lock";

/// "是谁在写"的说明文件。**没有锁语义**，纯粹是给人看的。
///
/// 之所以不把它塞进 `writer.lock`：我们用 `File::lock` 锁的是**整个文件**
/// （Windows 上是 0..MAX 的字节范围锁），锁着的时候别的进程连读都会被拒。
/// 想让它能读，就得改成只锁某个偏移量 —— 那要自己调 `LockFileEx`，
/// 从此每个平台一套代码。两个文件能解决的问题不值得换那个复杂度。
pub const OWNER_FILE: &str = "writer.owner";

/// 环境变量：等租约最多等多少秒。`0` = 不等待（立刻返回忙）。
pub const ENV_WAIT_SECS: &str = "HUB_WRITE_WAIT_SECS";

/// 默认等待时长。
///
/// 30 秒是"用户手动跑 `hub merge`，daemon 正在写"的合理耐心上限：
/// 一次 daemon 批次是几十毫秒，一次合并是几百毫秒到几秒，
/// 真等到 30 秒说明对面卡住了，这时候报错比继续挂着有用。
pub const DEFAULT_WAIT_SECS: u64 = 30;

/// 轮询间隔。
///
/// 5ms 是"临界区长度"的下限：一次 flush 是几十毫秒，一次 merge 是几百毫秒到几秒。
/// 取 5ms 让等待方在对方放锁之后几乎立刻进来，CPU 代价是每 5ms 醒一次 —— 相比
/// 一轮扫描动辄几万次 `stat`，可以忽略。取 20ms 会让每次交接都白白多等 15ms，
/// 而 `hub index` 会连续 flush 几百批。
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// 谁拿着租约。从 [`OWNER_FILE`] 读出来的，只能当参考 ——
/// 里面写的是"最后一个成功拿到租约的人自报的身份"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseHolder {
    pub pid: u32,
    /// 自报的角色：`daemon` / `index` / `merge` / `reset` / `flush`
    pub role: String,
    /// 拿到租约的时刻（unix 秒）
    pub since: i64,
}

impl LeaseHolder {
    /// 已经持有多少秒。时钟回拨时给 0，不出现负数。
    pub fn held_secs(&self) -> i64 {
        (now_secs() - self.since).max(0)
    }

    /// 给人看的一行：`pid 1234 的 daemon，已持有 3 秒`
    pub fn describe(&self) -> String {
        format!("pid {} 的 {}，已持有 {} 秒", self.pid, self.role, self.held_secs())
    }
}

/// 写租约。**拿到它才允许写索引与水位表**，drop 即释放。
#[derive(Debug)]
pub struct WriteLease {
    file: File,
    dir: PathBuf,
    role: &'static str,
}

impl WriteLease {
    /// 拿租约，最多等 [`default_wait`]。
    pub fn acquire(dir: impl AsRef<Path>, role: &'static str) -> Result<Self> {
        Self::acquire_within(dir, role, default_wait())
    }

    /// 拿租约，最多等 `wait`。等不到返回 [`StoreError::Busy`]。
    pub fn acquire_within(
        dir: impl AsRef<Path>,
        role: &'static str,
        wait: Duration,
    ) -> Result<Self> {
        let dir = dir.as_ref();
        if let Some(l) = Self::try_acquire(dir, role)? {
            return Ok(l);
        }

        let deadline = Instant::now() + wait;
        loop {
            // 先判超时再睡：wait 为 0 时这里立刻返回，
            // daemon 要的正是"试一下，忙就算了"。
            if Instant::now() >= deadline {
                return Err(StoreError::Busy(timeout_message(dir, wait)));
            }
            std::thread::sleep(POLL_INTERVAL);
            if let Some(l) = Self::try_acquire(dir, role)? {
                return Ok(l);
            }
        }
    }

    /// 试一次，拿不到就是 `None`（不等待）。
    pub fn try_acquire(dir: impl AsRef<Path>, role: &'static str) -> Result<Option<Self>> {
        let dir = dir.as_ref();
        // 数据目录不存在时先把目录建出来：锁文件要落在这儿。
        // 不建目录直接 open 会得到一个含糊的"系统找不到指定的路径"。
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lease_path(dir))?;

        match file.try_lock() {
            Ok(()) => {
                let lease = Self {
                    file,
                    dir: dir.to_path_buf(),
                    role,
                };
                lease.stamp();
                Ok(Some(lease))
            }
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }

    pub fn role(&self) -> &'static str {
        self.role
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 把"我是谁"写进 `writer.owner`。失败不影响正确性 —— 那只是一行给人看的说明。
    fn stamp(&self) {
        let text = format!("{}\t{}\t{}\n", std::process::id(), self.role, now_secs());
        let p = owner_path(&self.dir);
        if let Ok(mut f) = OpenOptions::new().create(true).write(true).truncate(true).open(p) {
            let _ = f.write_all(text.as_bytes());
        }
    }
}

impl Drop for WriteLease {
    fn drop(&mut self) {
        // 顺序不能反：先删说明、再解锁。
        //
        // 反过来的话存在一个窗口：我们已经解锁，别人拿到锁并写下自己的说明，
        // 然后我们才 remove —— 删掉的是**别人**的说明，之后报错信息里就会
        // 出现"另一个进程"这种毫无帮助的话。
        let _ = std::fs::remove_file(owner_path(&self.dir));
        // unlock 失败只可能是句柄已经没了（panic 展开途中之类），
        // 那种情况下 close 本身就会释放锁，不用管。
        let _ = File::unlock(&self.file);
    }
}

/// `<data>/writer.lock`
pub fn lease_path(dir: impl AsRef<Path>) -> PathBuf {
    dir.as_ref().join(LEASE_FILE)
}

/// `<data>/writer.owner`
pub fn owner_path(dir: impl AsRef<Path>) -> PathBuf {
    dir.as_ref().join(OWNER_FILE)
}

/// 现在**是否真的**有人持着租约。
///
/// 判据是"能不能锁上"，不是"锁文件在不在" —— 崩溃留下的文件是常态，
/// 文件存在完全不代表有人在写（而锁状态由内核维护，进程一死就没了）。
pub fn is_held(dir: impl AsRef<Path>) -> bool {
    let path = lease_path(dir);
    if !path.exists() {
        return false;
    }
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        // 打不开（权限 / 被独占）通常意味着别人正拿着，按"有人"处理更安全：
        // 这条路径只用于打提示，不参与任何判定。
        Err(_) => return true,
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = File::unlock(&file);
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => true,
    }
}

/// 读"是谁在写"。**它不判断真假**：锁已经放掉之后文件可能还在（我们从没在
/// 正常路径下留残留，但强杀会），所以要不要采用这个结果由调用方决定 ——
/// 一般用 [`peek`]。
pub fn read_holder(dir: impl AsRef<Path>) -> Option<LeaseHolder> {
    let text = std::fs::read_to_string(owner_path(dir)).ok()?;
    let line = text.lines().next()?.trim();
    let mut it = line.split('\t');
    let pid: u32 = it.next()?.trim().parse().ok()?;
    let role = it.next()?.trim().to_string();
    let since: i64 = it.next()?.trim().parse().ok()?;
    if role.is_empty() {
        return None;
    }
    Some(LeaseHolder { pid, role, since })
}

/// 有人持着租约时返回持有者信息；没人持有时 `None`。
///
/// 这是给 CLI 用的：动手之前问一句"要等谁"，比默默挂 30 秒强得多。
pub fn peek(dir: impl AsRef<Path>) -> Option<LeaseHolder> {
    let dir = dir.as_ref();
    if !is_held(dir) {
        return None;
    }
    read_holder(dir)
}

/// 等租约的默认时长：`HUB_WRITE_WAIT_SECS` 或 [`DEFAULT_WAIT_SECS`]。
///
/// 解析失败（"30s"、"很多"）时退回默认值而不是报错：这是个调优旋钮，
/// 不是配置项，写错不该让 `hub index` 起不来。
pub fn default_wait() -> Duration {
    match std::env::var(ENV_WAIT_SECS) {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => Duration::from_secs(DEFAULT_WAIT_SECS),
        },
        Err(_) => Duration::from_secs(DEFAULT_WAIT_SECS),
    }
}

/// 超时报错的话术。
///
/// 必须回答三个问题，否则用户只能干瞪眼：谁在写、写了多久、我能做什么。
fn timeout_message(dir: &Path, wait: Duration) -> String {
    let who = match peek(dir) {
        Some(h) => h.describe(),
        // 锁被占着却读不到说明：可能是别的用户/权限，也可能对面刚拿到锁还没写完。
        None => "一个没能报出身份的进程".to_string(),
    };
    format!(
        "等写租约超时（{:.1}s）：{}。\n\
         索引同一时刻只允许一个写者。大扫描期间排队是正常的；\
         若它已经卡住，用 `hub daemon stop`（或任务管理器结束该 pid）后再试。\n\
         愿意多等就把 {} 调大（单位秒）。",
        wait.as_secs_f32(),
        who,
        ENV_WAIT_SECS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn second_acquire_is_refused_while_first_is_alive() {
        let d = tmp();
        let a = WriteLease::acquire(d.path(), "test-a").unwrap();
        assert!(is_held(d.path()));
        assert_eq!(peek(d.path()).unwrap().role, "test-a");
        // 同一个进程里的第二个句柄也会撞上：Windows 上的字节范围锁是按句柄算的，
        // Unix 上 flock 是按打开的文件描述算的 —— 两边都不认"同进程"这层关系。
        // 这条性质很重要：单元测试里能直接验证互斥，不用真的 fork 一个进程。
        assert!(WriteLease::try_acquire(d.path(), "test-b").unwrap().is_none());
        drop(a);
        assert!(!is_held(d.path()));
        assert!(WriteLease::try_acquire(d.path(), "test-b").unwrap().is_some());
    }

    #[test]
    fn timeout_names_the_holder() {
        let d = tmp();
        let _a = WriteLease::acquire(d.path(), "index").unwrap();
        let err = WriteLease::acquire_within(d.path(), "merge", Duration::from_millis(60))
            .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, StoreError::Busy(_)), "应该是 Busy：{msg}");
        assert!(msg.contains("index"), "报错要说清是谁在写：{msg}");
        assert!(msg.contains(&std::process::id().to_string()), "要带 pid：{msg}");
        assert!(msg.contains(ENV_WAIT_SECS), "要给出路：{msg}");
    }

    #[test]
    fn zero_wait_fails_fast() {
        let d = tmp();
        let _a = WriteLease::acquire(d.path(), "index").unwrap();
        let t = Instant::now();
        let err = WriteLease::acquire_within(d.path(), "flush", Duration::ZERO).unwrap_err();
        assert!(matches!(err, StoreError::Busy(_)));
        assert!(t.elapsed() < Duration::from_millis(500), "wait=0 不该真去睡");
    }

    #[test]
    fn waiting_holder_eventually_gets_it() {
        let d = tmp();
        let path = d.path().to_path_buf();
        let a = WriteLease::acquire(&path, "index").unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            drop(a);
        });
        let started = Instant::now();
        let b = WriteLease::acquire_within(&path, "merge", Duration::from_secs(5)).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(100), "应该真的等了一会儿");
        assert_eq!(b.role(), "merge");
    }

    #[test]
    fn owner_file_is_cleaned_up_and_stale_text_is_ignored() {
        let d = tmp();
        drop(WriteLease::acquire(d.path(), "index").unwrap());
        assert!(!owner_path(d.path()).exists(), "正常释放后不该留下说明文件");
        assert!(peek(d.path()).is_none());
        // 伪造一个残留说明（模拟强杀）：没人持锁时必须报"没人"
        std::fs::write(owner_path(d.path()), "99999\tghost\t1\n").unwrap();
        assert!(!is_held(d.path()));
        assert!(peek(d.path()).is_none(), "锁空闲时残留说明不算数");
    }

    #[test]
    fn missing_lock_file_means_free_not_busy() {
        let d = tmp();
        assert!(!is_held(d.path()));
        assert!(peek(d.path()).is_none());
    }

    #[test]
    fn holder_describes_itself_in_one_line() {
        let h = LeaseHolder { pid: 4242, role: "daemon".into(), since: now_secs() - 3 };
        assert_eq!(h.held_secs(), 3);
        let line = h.describe();
        assert!(line.contains("4242") && line.contains("daemon") && line.contains('3'), "{line}");
        // 时钟回拨（since 在未来）不该出现"已持有 -5 秒"
        let h = LeaseHolder { pid: 1, role: "x".into(), since: now_secs() + 100 };
        assert_eq!(h.held_secs(), 0);
    }
}
