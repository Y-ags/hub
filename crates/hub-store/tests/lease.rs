//! 写者串行化的集成测试。
//!
//! 这些测试回答两个问题，顺序不能反：
//!
//! 1. **为什么非要一把租约**（`tantivy_writer_lock_is_exclusive`）：Tantivy 的
//!    写入器锁是硬互斥的，第二个写入器拿不到就直接报错、不排队。
//! 2. **加了租约之后是不是真的不会撞上**（`two_indexers_writing_at_once_never_fail`）：
//!    两个写者各写 200 篇，一条错误都不许有。
//!
//! 第 2 条在加租约之前是会挂的 —— 失败信息是
//! `tantivy: Failed to acquire index lock`，而且挂不挂取决于两个线程的调度，
//! 所以它是一条**概率性**回归测试：跑 40 轮 × 2 线程，撞上的概率足够高。
//! 如果哪天有人把 `flush` 里的租约删了，这条会开始偶发失败 —— 这正是它存在的意义。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hub_core::SourceId;
use hub_store::lease::{self, WriteLease};
use hub_store::{HubIndex, Indexer, StoreError};
use tempfile::TempDir;

fn tmp() -> TempDir {
    tempfile::tempdir().unwrap()
}

fn add(ix: &mut Indexer, uri: &str, body: &str) -> bool {
    ix.add(SourceId::Files, uri, format!("标题 {uri}"), body.to_string(), 1_700_000_000)
        .unwrap()
}

/// 根因：没有租约的时候，第二个写入器会直接失败。
///
/// 这条不是"测试我们的代码"，是**钉住依赖的行为**。tantivy 哪天改成排队等待了，
/// 这条会失败，那时候就该重新评估租约还要不要 —— 而不是让它悄悄变成一个
/// 谁也不知道为什么存在的模块。
#[test]
fn tantivy_writer_lock_is_exclusive() {
    let dir = tmp();
    let index = HubIndex::open_or_create(dir.path().join("index")).unwrap();

    let first = index.writer(15_000_000).expect("第一个写入器应该拿得到");
    let second = index.writer(15_000_000);
    assert!(second.is_err(), "第二个写入器居然也拿到了锁 —— 前提变了，回去看 lease 模块");
    drop(first);
    assert!(index.writer(15_000_000).is_ok(), "放掉之后应该能拿");
}

/// 两个写者同时跑，一条错误都不该有，而且最后一次都不许丢。
#[test]
fn two_indexers_writing_at_once_never_fail() {
    let dir = tmp();
    let path = dir.path().to_path_buf();

    // 预热：把库和索引建出来，避免两个线程同时做建表
    drop(Indexer::open(&path).unwrap());

    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let written = Arc::new(AtomicUsize::new(0));
    const ROUNDS: usize = 40;
    const PER_ROUND: usize = 5;

    let mut handles = Vec::new();
    for t in 0..2 {
        let path = path.clone();
        let errors = Arc::clone(&errors);
        let written = Arc::clone(&written);
        handles.push(std::thread::spawn(move || {
            // 和 CLI 一样：愿意等（daemon 那边是 0，见下一条测试）
            let mut ix = Indexer::open(&path).unwrap();
            ix.set_lease_wait(Duration::from_secs(10));
            for r in 0..ROUNDS {
                for i in 0..PER_ROUND {
                    let uri = format!("file:///t{t}/r{r}/d{i}.txt");
                    if add(&mut ix, &uri, "同一批写入的语料 内存 索引 并发 串行化") {
                        written.fetch_add(1, Ordering::Relaxed);
                    }
                }
                if let Err(e) = ix.flush() {
                    errors.lock().unwrap().push(format!("t{t} r{r}: {e}"));
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let errs = errors.lock().unwrap();
    assert!(
        errs.is_empty(),
        "并发写入出现了 {} 条错误，第一条：{}\n（如果是索引锁错误，说明 flush 没有持租约）",
        errs.len(),
        errs.first().cloned().unwrap_or_default()
    );
    drop(errs);

    assert_eq!(written.load(Ordering::Relaxed), 2 * ROUNDS * PER_ROUND, "入队条数不对");

    let ix = Indexer::open(&path).unwrap();
    let (live, tombs, docs, _) = ix.stats().unwrap();
    assert_eq!(live, 2 * ROUNDS * PER_ROUND, "水位表里的存活条数丢了");
    assert_eq!(tombs, 0);
    assert_eq!(docs, 2 * ROUNDS * PER_ROUND, "倒排里的文档数对不上");
}

/// 租约被别人占着时，`flush` 应该返回 **Busy**，而不是一个 tantivy 的锁错误。
///
/// 这个区分很重要：daemon 靠 `is_busy()` 决定"稍后重试、别丢批"，
/// 其它错误它只能记一笔。错误类型糊在一起，丢数据就是静默的。
#[test]
fn flush_reports_busy_instead_of_a_tantivy_error() {
    let dir = tmp();
    let mut ix = Indexer::open(dir.path()).unwrap();
    ix.set_lease_wait(Duration::ZERO);

    // 模拟"daemon 正在写"
    let holder = WriteLease::acquire(dir.path(), "daemon").unwrap();

    add(&mut ix, "file:///busy.txt", "内容不该被写进去");
    let err = ix.flush().expect_err("有人在写的时候 flush 必须失败");
    assert!(err.is_busy(), "应该是 Busy，实际：{err}");
    assert!(
        !err.to_string().contains("tantivy"),
        "不该把底层锁错误漏出来：{err}"
    );
    assert_eq!(ix.stats().unwrap().0, 0, "失败时不该留下半条数据");

    drop(holder);
    // 同一批数据还在缓冲里，等对方走了立刻就能写
    assert_eq!(ix.flush().unwrap(), 1, "对方放锁之后这批应该照常写进去");
    assert_eq!(ix.stats().unwrap().0, 1);
}

/// `merge` 也一样，而且它更重（要重写段文件）。
#[test]
fn merge_waits_for_the_other_writer() {
    let dir = tmp();
    let mut ix = Indexer::open(dir.path()).unwrap();
    add(&mut ix, "file:///m1.txt", "要被删除的内容");
    add(&mut ix, "file:///m2.txt", "留着的内容");
    ix.flush().unwrap();
    ix.delete(SourceId::Files, "file:///m1.txt");
    ix.flush().unwrap();
    assert_eq!(ix.stats().unwrap().1, 1, "应该有一条墓碑");

    let holder = WriteLease::acquire(dir.path(), "daemon").unwrap();
    ix.set_lease_wait(Duration::ZERO);
    let err = ix.merge().expect_err("有人持锁时 merge 必须失败");
    assert!(err.is_busy(), "应该是 Busy，实际：{err}");

    drop(holder);
    ix.merge().unwrap();
    assert_eq!(ix.stats().unwrap().1, 0, "合并之后墓碑该被清掉");
}

/// `reset` 会删索引目录 —— 有人在写的时候绝不能硬来。
#[test]
fn reset_refuses_while_someone_holds_the_lease() {
    let dir = tmp();
    let mut ix = Indexer::open(dir.path()).unwrap();
    add(&mut ix, "file:///r1.txt", "内容");
    ix.flush().unwrap();
    drop(ix);

    let holder = WriteLease::acquire(dir.path(), "daemon").unwrap();
    let err = Indexer::reset_within(dir.path(), Duration::ZERO).unwrap_err();
    assert!(err.is_busy(), "应该是 Busy，实际：{err}");
    drop(holder);

    Indexer::reset(dir.path()).unwrap();
    let ix = Indexer::open(dir.path()).unwrap();
    assert_eq!(ix.stats().unwrap().0, 0, "reset 之后应该是空的");
}

/// 真实的等待：拿不到就等着，对面放掉之后自己接着写。
///
/// 用 3 秒上限而不是默认 30 秒：这条要证明"会等"，不是"会等很久"。
#[test]
fn waiting_writer_gets_through_after_the_holder_finishes() {
    let dir = tmp();
    let path = dir.path().to_path_buf();
    let mut ix = Indexer::open(&path).unwrap();
    ix.set_lease_wait(Duration::from_secs(3));

    let holder = WriteLease::acquire(&path, "daemon").unwrap();
    add(&mut ix, "file:///w1.txt", "等它写完");

    let probe = path.clone();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        assert!(lease::is_held(&probe));
        drop(holder);
    });

    let started = Instant::now();
    let n = ix.flush().expect("等一会儿应该能拿到");
    let waited = started.elapsed();
    releaser.join().unwrap();

    assert_eq!(n, 1);
    assert!(waited >= Duration::from_millis(100), "应该真的等过，实际等了 {waited:?}");
    assert!(!lease::is_held(&path), "写完必须把租约放掉");
}

/// 写完不留残骸：锁文件留着（当占位符没问题），说明文件必须没了。
#[test]
fn flush_leaves_no_owner_file_behind() {
    let dir = tmp();
    let mut ix = Indexer::open(dir.path()).unwrap();
    add(&mut ix, "file:///c1.txt", "内容");
    ix.flush().unwrap();
    assert!(lease::lease_path(dir.path()).exists(), "锁文件可以留着");
    assert!(!lease::owner_path(dir.path()).exists(), "「谁在写」的说明必须清掉");
    assert!(lease::peek(dir.path()).is_none());
}

/// 并发**打开**也要安全 —— 这不是租约管的范围，但同属"多进程共存"。
///
/// daemon 常驻之后，`hub search --local`、`hub ui`、`hub stats` 的本地退路
/// 会随时打开同一个库；两个进程恰好同时打开时，后到的那个会撞上对方正在做
/// WAL 恢复，拿到一个瞬时错误（实测 `SQLITE_READONLY`，字面意思是"只读库"，
/// 和真实原因毫无关系）。`MetaStore::open` 里那层重试就是为这个加的。
///
/// 这条测试之前 50 轮里失败 4 次；现在 2 线程 × 50 轮必须 0 失败。
#[test]
fn concurrent_open_is_safe() {
    let dir = tmp();
    let path = dir.path().join("meta.sqlite");
    // 先建库，让 -wal / -shm 存在 —— 恢复路径才是会撞的那条
    drop(hub_store::MetaStore::open(&path).unwrap());

    let errs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut hs = Vec::new();
    for t in 0..2 {
        let p = path.clone();
        let e = Arc::clone(&errs);
        hs.push(std::thread::spawn(move || {
            for i in 0..50 {
                if let Err(err) = hub_store::MetaStore::open(&p) {
                    e.lock().unwrap().push(format!("t{t} i{i}: {err}"));
                }
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let e = errs.lock().unwrap();
    assert!(e.is_empty(), "并发打开失败 {} 次，第一条：{}", e.len(), e[0]);
}

/// 写好一批之后立刻有人打开同一个库：这一步在真实使用里是常态
/// （daemon 刚写完，你敲了 `hub search`），不能有任何瞬时错误。
#[test]
fn open_right_after_a_commit_is_safe() {
    let dir = tmp();
    let path = dir.path().to_path_buf();
    let mut ix = Indexer::open(&path).unwrap();
    for i in 0..30 {
        add(&mut ix, &format!("file:///hot/{i}.txt"), "刚写完马上读");
        ix.flush().unwrap();
        drop(Indexer::open(&path).unwrap());
    }
    assert_eq!(ix.stats().unwrap().0, 30);
}

/// 那条"Busy 不等于损坏"的旁证：失败之后重新 flush，数据要能完整落盘。
#[test]
fn a_busy_flush_loses_nothing() {
    let dir = tmp();
    let mut ix = Indexer::open(dir.path()).unwrap();
    ix.set_lease_wait(Duration::ZERO);
    for i in 0..20 {
        add(&mut ix, &format!("file:///batch/{i}.txt"), "批内数据 一条都不能少");
    }
    let holder = WriteLease::acquire(dir.path(), "index").unwrap();
    assert!(matches!(ix.flush(), Err(StoreError::Busy(_))));
    drop(holder);
    assert_eq!(ix.flush().unwrap(), 20);
    let (live, _, docs, _) = ix.stats().unwrap();
    assert_eq!((live, docs), (20, 20));
}
