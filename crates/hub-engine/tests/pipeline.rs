//! 采集流水线的行为测试。
//!
//! 这些测试覆盖的是 daemon 真正依赖的那几条路径 —— 尤其是
//! "只给一个文件路径，它能不能被正确索引"，这是 watcher 的全部工作内容。

use std::fs;
use std::path::{Path, PathBuf};

use hub_engine::{Engine, EngineConfig};

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn write(p: &Path, s: &str) {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, s).unwrap();
}

fn engine(data: &Path, roots: Vec<PathBuf>) -> Engine {
    Engine::open(
        data,
        EngineConfig {
            roots,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn scan_all_then_index_single_file() {
    let t = tmp();
    let root = t.path().join("src");
    let data = t.path().join("data");
    let f = root.join("pool.md");
    write(&f, "# 内存池\n用 free list 管理切块。");

    let mut e = engine(&data, vec![root.clone()]);
    let rep = e.scan_all().unwrap();
    assert_eq!(rep.indexed, 1, "冷启动应索引 1 篇");

    // 改内容 → 单文件入口应该识别为变化
    write(&f, "# 内存池\n用 free list 管理切块。\n配合 mmap 零拷贝。");
    let rep = e.index_paths(&[f.clone()]).unwrap();
    assert_eq!(rep.indexed, 1, "内容变了应重新入库，实际 {rep:?}");

    let hits = e.search_text("零拷贝", 5).unwrap();
    assert_eq!(hits.len(), 1, "新内容应能搜到");
}

#[test]
fn index_single_file_skips_unchanged() {
    let t = tmp();
    let root = t.path().join("src");
    let data = t.path().join("data");
    let f = root.join("a.md");
    write(&f, "内存管理");

    let mut e = engine(&data, vec![root.clone()]);
    e.scan_all().unwrap();

    // 内容没变 → 不该重复写
    let rep = e.index_paths(&[f.clone()]).unwrap();
    assert_eq!(rep.indexed, 0, "内容没变不该重新索引");
    assert_eq!(rep.skipped, 1);
}

#[test]
fn new_file_in_new_subdir_is_indexed() {
    let t = tmp();
    let root = t.path().join("proj");
    let data = t.path().join("data");
    fs::create_dir_all(&root).unwrap();

    let mut e = engine(&data, vec![root.clone()]);
    e.scan_all().unwrap();

    let f = root.join("notes").join("deep").join("cross.md");
    write(&f, "aarch64-linux-ohos 交叉编译");
    let rep = e.index_paths(&[f.clone()]).unwrap();
    assert_eq!(rep.indexed, 1, "新目录下的新文件应被索引，实际 {rep:?}");
}

/// watcher 报上来的路径可能是 `C:\a\b/src/x.md` 这种混合分隔符
/// （Windows API 给反斜杠，我们替换成分隔符再 join）。这条必须能识别。
#[test]
fn mixed_separators_path_is_indexed() {
    let t = tmp();
    let root = t.path().join("proj");
    let data = t.path().join("data");
    fs::create_dir_all(root.join("src")).unwrap();
    write(&root.join("src").join("m.md"), "混合适配");

    let mut e = engine(&data, vec![root.clone()]);
    e.scan_all().unwrap();

    let f = root.join("src").join("mix.md");
    write(&f, "零拷贝内存");

    // 构造混合分隔符形态
    let mixed = PathBuf::from(f.to_string_lossy().replace('\\', "/"));
    let rep = e.index_paths(&[mixed]).unwrap();
    assert_eq!(rep.indexed, 1, "混合分隔符路径应被索引，实际 {rep:?}");
}

#[test]
fn noise_paths_are_skipped() {
    let t = tmp();
    let root = t.path().join("proj");
    let data = t.path().join("data");
    fs::create_dir_all(root.join("node_modules/x")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    write(&root.join("node_modules/x/index.js"), "console.log(1)");
    write(&root.join("src/logo.png"), "not really");

    let mut e = engine(&data, vec![root.clone()]);
    let rep = e.index_paths(&[
        root.join("node_modules/x/index.js"),
        root.join("src/logo.png"),
    ])
    .unwrap();
    assert_eq!(rep.indexed, 0, "噪声路径不该入库");
    assert_eq!(rep.skipped, 2);
}

/// root 落在 Temp / AppData 这类目录下时，跳过规则必须只看 root 以下的部分
#[test]
fn root_under_appdata_still_indexes() {
    let t = tmp();
    // tempdir 本身就在 %TEMP% 下，正好复现这个场景
    let root = t.path().join("proj");
    let data = t.path().join("data");
    write(&root.join("src").join("a.md"), "内存管理");

    let mut e = engine(&data, vec![root.clone()]);
    let rep = e.scan_all().unwrap();
    assert_eq!(
        rep.indexed, 1,
        "root 在 %TEMP% 下时不该被跳过规则整棵砍掉，实际 {rep:?}"
    );
}

#[test]
fn reconcile_detects_new_and_deleted() {
    let t = tmp();
    let root = t.path().join("proj");
    let data = t.path().join("data");
    let a = root.join("a.md");
    let b = root.join("b.md");
    write(&a, "内存");
    write(&b, "管理");

    let mut e = engine(&data, vec![root.clone()]);
    e.scan_all().unwrap();
    assert_eq!(e.indexer().stats().unwrap().0, 2);

    // 删一个、加一个，然后对账
    fs::remove_file(&b).unwrap();
    let c = root.join("c.md");
    write(&c, "零拷贝");

    let rep = e.reconcile().unwrap();
    assert_eq!(rep.indexed, 1, "新文件应被补进来");
    assert_eq!(rep.deleted, 1, "被删的文件应打墓碑");

    let live = e.indexer().stats().unwrap().0;
    assert_eq!(live, 2, "对账后存活应是 a 和 c");
    assert_eq!(e.search_text("零拷贝", 5).unwrap().len(), 1);
    assert!(e.search_text("管理", 5).unwrap().is_empty());
}

#[test]
fn remove_paths_marks_child_tree() {
    let t = tmp();
    let root = t.path().join("proj");
    let data = t.path().join("data");
    write(&root.join("dir/x.md"), "第一篇");
    write(&root.join("dir/sub/y.md"), "第二篇");

    let mut e = engine(&data, vec![root.clone()]);
    e.scan_all().unwrap();
    assert_eq!(e.indexer().stats().unwrap().0, 2);

    // 整个目录被删：watcher 只会报一条 dir 的 Remove
    fs::remove_dir_all(root.join("dir")).unwrap();
    let rep = e.remove_paths(&[root.join("dir")]).unwrap();
    assert_eq!(rep.deleted, 2, "子树下的两篇都要打墓碑");
    // 但 indexer 的 delete_under 已经把子树都清了
    assert_eq!(e.indexer().stats().unwrap().0, 0, "目录下所有文档都应消失");
}

// ============================================================
// 终端历史（daemon 每 5 秒轮询一次，见 hub-daemon/src/lib.rs 第 6 步）
// ============================================================

/// 用临时文件当历史，避免测试去读开发机上真实的 shell 历史
fn history_engine(data: &Path, hist: &Path) -> Engine {
    Engine::open(
        data,
        EngineConfig {
            with_history: true,
            history_files: vec![hist.to_path_buf()],
            ..Default::default()
        },
    )
    .unwrap()
}

/// 追加一条命令后，**只**索引新增的那条；文件没变时连解析都不做。
#[test]
fn history_append_indexes_only_new_entries() {
    let t = tmp();
    let hist = t.path().join("history.txt");
    let data = t.path().join("data");
    write(&hist, "cargo build --release\ngit status\n");

    let mut e = history_engine(&data, &hist);
    let r1 = e.ingest_history().unwrap();
    assert_eq!(r1.indexed, 2, "首次应索引两条：{r1:?}");

    // 文件没变 → (len, mtime) 快路径，一次 stat 就返回，连 parse 都不做
    let r2 = e.ingest_history().unwrap();
    assert_eq!((r2.indexed, r2.scanned), (0, 0), "文件没变不该重新解析：{r2:?}");

    // 追加一条 → 全量解析，但只写新的
    let mut text = fs::read_to_string(&hist).unwrap();
    text.push_str("rustup target add aarch64-pc-windows-msvc\n");
    fs::write(&hist, text).unwrap();

    let r3 = e.ingest_history().unwrap();
    assert_eq!(r3.indexed, 1, "只该写入新增那条：{r3:?}");
    assert!(r3.scanned >= 3, "解析仍然要扫全量：{r3:?}");

    let hits = e.search_text("aarch64-pc-windows-msvc", 5).unwrap();
    assert_eq!(hits.len(), 1, "新命令应能搜到");
}

/// 纯文本历史的 uri 只由命令决定。别的工具 touch 一下文件（mtime 变、内容没变），
/// 不该导致几千条历史被重写一遍 —— 修复前正是这个行为。
#[test]
fn history_touch_does_not_reindex() {
    let t = tmp();
    let hist = t.path().join("history.txt");
    let data = t.path().join("data");
    write(&hist, "cargo build --release\n");

    let mut e = history_engine(&data, &hist);
    assert_eq!(e.ingest_history().unwrap().indexed, 1);

    // 只改 mtime，不碰内容
    let f = fs::OpenOptions::new().append(true).open(&hist).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(30))
        .unwrap();
    drop(f);

    let rep = e.ingest_history().unwrap();
    assert_eq!(rep.scanned, 1, "mtime 变了要重新解析");
    assert_eq!(rep.indexed, 0, "但内容没变，不该重写索引：{rep:?}");
}

/// 同一条命令敲很多次只留一条 —— 这是"纯文本历史按命令去重"的副作用，
/// 也是想要的：PSReadLine 历史里 60% 是重复命令，全索引进来只会稀释排序。
#[test]
fn repeated_commands_collapse_to_one() {
    let t = tmp();
    let hist = t.path().join("history.txt");
    let data = t.path().join("data");
    write(&hist, "cargo test\ncargo test\ncargo test\n");

    let mut e = history_engine(&data, &hist);
    let rep = e.ingest_history().unwrap();
    assert_eq!(rep.indexed, 1, "同一条命令只索引一次：{rep:?}");

    let hits = e.search_text("cargo test", 10).unwrap();
    assert_eq!(hits.len(), 1, "搜索也只该出一条：{hits:?}");
}
