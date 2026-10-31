//! hub 命令行入口。
//!
//! 一个原则：**这里不写任何检索逻辑**。查询解析、排序、增量判定、持久化全在下面三层，
//! CLI 只负责参数、IO 和打印。第一版 CLI 直接 `MemoryIndex::default()` 塞了两条假数据，
//! 关掉进程就什么都没了 —— 那不是"能跑的部分"，那是演示。

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use hub_connectors::{WalkOptions, default_history_files, is_noise, parse_history, read_text, walk};
use hub_core::{ParsedQuery, SourceId};
use hub_store::indexer::{hash_bytes, now_secs};
use hub_store::{Hit, Indexer, IndexerStats};

const USAGE: &str = "\
hub —— 本地离线多源个人检索中枢

用法:
  hub index <目录>... [--history] [--max-mb N] [--reset]
        把目录（可选：终端历史）扫进索引。内容没变的文件会被跳过。
  hub search <查询> [--limit N]
        自然语言检索。支持「上个月」「代码片段」\"精确短语\" -排除词。
  hub stats        查看索引规模与来源分布
  hub merge        回收已删文档占用的倒排空间
  hub reset --yes  清空索引与水位表（破坏性操作）
  hub              进入交互模式（只有检索）

全局参数:
  --data <目录>    数据目录，默认 $HUB_HOME 或 ~/.hub
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误：{e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let raw: Vec<String> = env::args().skip(1).collect();
    let (data, args) = split_data_dir(raw);
    let cmd = args.first().map(String::as_str).unwrap_or("");

    match cmd {
        "" => repl(&data),
        "index" | "i" => cmd_index(&data, &args[1..]),
        "search" | "s" | "q" => cmd_search(&data, &args[1..]),
        "stats" => cmd_stats(&data),
        "merge" => cmd_merge(&data),
        "reset" => cmd_reset(&data, &args[1..]),
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("未知子命令 `{other}`，输入 hub help 查看用法")),
    }
}

fn default_data_dir() -> PathBuf {
    if let Some(v) = env::var_os("HUB_HOME") {
        return PathBuf::from(v);
    }
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".hub")
}

/// 把 `--data <dir>` / `--data=<dir>` 从参数里摘出来，剩下的交给子命令
fn split_data_dir(raw: Vec<String>) -> (PathBuf, Vec<String>) {
    let mut data = default_data_dir();
    let mut rest = Vec::with_capacity(raw.len());
    let mut it = raw.into_iter();
    while let Some(a) = it.next() {
        if a == "--data" {
            if let Some(v) = it.next() {
                data = PathBuf::from(v);
            }
        } else if let Some(v) = a.strip_prefix("--data=") {
            data = PathBuf::from(v);
        } else {
            rest.push(a);
        }
    }
    (data, rest)
}

// ============================================================
// index
// ============================================================

fn cmd_index(data: &Path, args: &[String]) -> Result<(), String> {
    let mut max_mb: u64 = 2;
    let mut with_history = false;
    let mut reset = false;
    let mut roots: Vec<PathBuf> = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--max-mb" => {
                max_mb = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| "--max-mb 需要一个整数".to_string())?;
            }
            "--history" => with_history = true,
            "--reset" => reset = true,
            _ if a.starts_with("--max-mb=") => {
                max_mb = a[9..].parse().map_err(|_| "--max-mb 需要一个整数".to_string())?;
            }
            _ if a.starts_with("--") => return Err(format!("未知参数 {a}")),
            _ => roots.push(PathBuf::from(a)),
        }
    }

    if roots.is_empty() && !with_history {
        return Err("没有指定扫描路径。用法：hub index <目录> [--history]".into());
    }

    if reset {
        Indexer::reset(data).map_err(|e| format!("清空索引失败：{e}"))?;
        println!("已清空 {}", data.display());
    }

    let started = Instant::now();
    let mut indexer = Indexer::open(data).map_err(|e| format!("打开数据目录失败：{e}"))?;
    let opts = WalkOptions {
        max_file_bytes: max_mb.max(1) * 1024 * 1024,
        ..Default::default()
    };

    let mut total = IndexerStats::default();
    let mut first_err: Option<String> = None;
    // 真正落盘的文档数。不能用 add() 的返回次数：同一批次里重复的命令/文件会被
    // flush 去重（比如 PowerShell 历史没有时间戳，同一条命令出现 N 次只留一条），
    // 用入队次数报数会虚高。这里只认 flush 的返回值。
    let mut committed = 0usize;

    for root in &roots {
        if !root.exists() {
            eprintln!("跳过不存在的路径：{}", root.display());
            continue;
        }
        let mut pending = 0usize;
        let ws = walk(root, &opts, |hit| {
            if first_err.is_some() {
                return;
            }
            total.scanned += 1;
            // 统一成 / 分隔：同一文件在 Windows 上不该出现两种 uri
            let uri = hit.path.to_string_lossy().replace('\\', "/");

            match indexer.needs_read(SourceId::Files, &uri, hit.mtime) {
                Ok(false) => {
                    total.skipped += 1;
                    return;
                }
                Ok(true) => {}
                Err(e) => {
                    first_err = Some(e.to_string());
                    return;
                }
            }

            match read_text(&hit.path, opts.max_file_bytes) {
                Ok(Some(text)) => {
                    let title = hit
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    match indexer.add(SourceId::Files, &uri, title, text, hit.mtime) {
                        Ok(true) => pending += 1,
                        Ok(false) => total.skipped += 1,
                        Err(e) => {
                            first_err = Some(e.to_string());
                            return;
                        }
                    }
                }
                // 二进制 / 非 UTF-8 / 过大：都算跳过，不算错误
                Ok(None) => total.skipped += 1,
                Err(_) => total.skipped += 1,
            }

            // 分批提交：writer 短命，写完立刻释放内存预算
            if pending >= 500 {
                match indexer.flush() {
                    Ok(n) => committed += n,
                    Err(e) => first_err = Some(e.to_string()),
                }
                pending = 0;
            }
        })
        .map_err(|e| format!("遍历 {} 失败：{e}", root.display()))?;

        println!(
            "扫描 {}：{} 个文件，跳过 目录 {}/扩展名 {}/超限 {}/软链 {}，出错 {} 次",
            root.display(),
            ws.files,
            ws.skipped_dir,
            ws.skipped_ext,
            ws.skipped_size,
            ws.skipped_symlink,
            ws.errors
        );
    }

    if with_history {
        index_history(&mut indexer, &mut total, &mut first_err)?;
    }

    committed += indexer.flush().map_err(|e| format!("提交失败：{e}"))?;
    total.indexed = committed;
    if let Some(e) = first_err {
        return Err(e);
    }

    println!(
        "\n完成：扫描 {} 条，入索引 {} 条，跳过 {} 条，用时 {:.2}s\n\
         数据目录：{}（用 hub stats 查看规模）",
        total.scanned,
        total.indexed,
        total.skipped,
        started.elapsed().as_secs_f32(),
        data.display()
    );
    Ok(())
}

/// 终端历史入索引。
///
/// uri 用 (时间戳 + 命令) 的哈希，而不是"文件第几行"：历史文件是**追加**写的，
/// 用行号做 uri，文件一追加所有旧条目的 uri 全部漂移，等于每次全量重索引。
fn index_history(
    indexer: &mut Indexer,
    total: &mut IndexerStats,
    first_err: &mut Option<String>,
) -> Result<(), String> {
    let files = default_history_files();
    if files.is_empty() {
        println!("没找到终端历史文件（.zsh_history / .bash_history / PSReadLine），跳过");
        return Ok(());
    }

    for f in files {
        let mtime = fs::metadata(&f)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let text = match read_text(&f, 32 * 1024 * 1024) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("读取 {} 失败：{e}", f.display());
                continue;
            }
        };

        let mut kept = 0usize;
        for e in parse_history(&text, mtime) {
            if is_noise(&e.cmd) {
                continue;
            }
            total.scanned += 1;
            let stamp = format!("{}:{}", e.ts, e.cmd);
            let uri = format!("terminal://{}/{:016x}", f.display(), hash_bytes(stamp.as_bytes()));

            match indexer.needs_read(SourceId::Terminal, &uri, e.ts) {
                Ok(false) => {
                    total.skipped += 1;
                    continue;
                }
                Ok(true) => {}
                Err(err) => {
                    *first_err = Some(err.to_string());
                    return Ok(());
                }
            }

            let title: String = e.cmd.lines().next().unwrap_or("").chars().take(80).collect();
            match indexer.add(SourceId::Terminal, &uri, title, e.cmd.clone(), e.ts) {
                // 这里只统计"入队"，真正落盘数以 flush 返回值为准（重复命令会被去重）
                Ok(true) => kept += 1,
                Ok(false) => total.skipped += 1,
                Err(err) => {
                    *first_err = Some(err.to_string());
                    return Ok(());
                }
            }
        }
        println!("终端历史 {}：{} 条候选（重复命令会合并为一条）", f.display(), kept);
    }
    Ok(())
}

// ============================================================
// search
// ============================================================

fn cmd_search(data: &Path, args: &[String]) -> Result<(), String> {
    let mut limit = 10usize;
    let mut words: Vec<String> = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--limit" || a == "-n" {
            limit = it
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| "--limit 需要一个整数".to_string())?;
        } else if let Some(v) = a.strip_prefix("--limit=") {
            limit = v.parse().map_err(|_| "--limit 需要一个整数".to_string())?;
        } else {
            words.push(a.clone());
        }
    }

    let q = words.join(" ");
    if q.trim().is_empty() {
        return Err("查询不能为空。用法：hub search <查询>".into());
    }

    let indexer = Indexer::open(data).map_err(|e| format!("打开数据目录失败：{e}"))?;
    let (live, _, _, _) = indexer.stats().map_err(|e| e.to_string())?;
    if live == 0 {
        return Err("索引是空的。先运行：hub index <目录>".into());
    }

    let started = Instant::now();
    let pq = indexer.parse(&q);
    let hits = indexer.search(&pq, limit).map_err(|e| e.to_string())?;
    let elapsed = started.elapsed().as_secs_f32() * 1000.0;

    print_plan(&pq);
    if hits.is_empty() {
        println!("（无结果）换个说法，或去掉时间/来源约束试试");
        return Ok(());
    }
    println!("命中 {} 条，{:.1} ms\n", hits.len(), elapsed);
    print_hits(&hits, now_secs());
    Ok(())
}

/// 把解析结果摊开给用户看 —— 搜不到时能立刻判断是"没这东西"还是"约束太严"
fn print_plan(pq: &ParsedQuery) {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = &pq.time {
        parts.push(format!("时间 {}~{} 天前", t.min_days, t.max_days));
    }
    if let Some(s) = pq.source {
        parts.push(format!("来源 {}", s.as_str()));
    }
    if !pq.phrases.is_empty() {
        parts.push(format!("短语 {:?}", pq.phrases));
    }
    if !pq.excludes.is_empty() {
        parts.push(format!("排除 {:?}", pq.excludes));
    }
    if parts.is_empty() {
        parts.push("无附加约束".into());
    }
    println!("识别：{}", parts.join("  ·  "));
    println!("检索词：{}\n", pq.residue);
}

fn print_hits(hits: &[Hit], now: i64) {
    for (i, h) in hits.iter().enumerate() {
        println!(
            "{:>2}. [{:<8}] {:>8}  {:<38}  {:.3}",
            i + 1,
            h.source.as_str(),
            human_age(h.mtime, now),
            truncate(&h.title, 38),
            h.score
        );
        println!("    {}", h.uri);
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

fn human_age(mtime: i64, now: i64) -> String {
    if mtime <= 0 {
        return "未知".into();
    }
    let secs = now - mtime;
    if secs < 0 {
        return "刚刚".into();
    }
    match secs {
        s if s < 60 => format!("{s} 秒前"),
        s if s < 3600 => format!("{} 分钟前", s / 60),
        s if s < 86_400 => format!("{} 小时前", s / 3600),
        s if s < 30 * 86_400 => format!("{} 天前", s / 86_400),
        s if s < 365 * 86_400 => format!("{} 个月前", s / (30 * 86_400)),
        s => format!("{} 年前", s / (365 * 86_400)),
    }
}

// ============================================================
// 其他子命令
// ============================================================

fn cmd_stats(data: &Path) -> Result<(), String> {
    let indexer = Indexer::open(data).map_err(|e| format!("打开数据目录失败：{e}"))?;
    let (live, tombs, docs, by_source) = indexer.stats().map_err(|e| e.to_string())?;

    println!("数据目录：{}", data.display());
    println!("存活文档：{live}    墓碑：{tombs}    倒排中：{docs}");
    if by_source.is_empty() {
        println!("来源分布：（空）");
    } else {
        println!("来源分布：");
        for (src, n) in by_source {
            println!("  {src:<10} {n}");
        }
    }
    if tombs > 0 {
        println!("\n有 {tombs} 条墓碑，运行 hub merge 回收空间");
    }
    Ok(())
}

fn cmd_merge(data: &Path) -> Result<(), String> {
    let started = Instant::now();
    let mut indexer = Indexer::open(data).map_err(|e| format!("打开数据目录失败：{e}"))?;
    indexer.merge().map_err(|e| format!("合并失败：{e}"))?;
    let (live, tombs, docs, _) = indexer.stats().map_err(|e| e.to_string())?;
    println!(
        "合并完成：存活 {live}，墓碑 {tombs}，倒排中 {docs}，用时 {:.2}s",
        started.elapsed().as_secs_f32()
    );
    Ok(())
}

fn cmd_reset(data: &Path, args: &[String]) -> Result<(), String> {
    if !args.iter().any(|a| a == "--yes") {
        return Err(format!(
            "reset 会删除 {} 下的索引与元数据（原始文件不受影响）。\n确认请加 --yes：hub reset --yes",
            data.display()
        ));
    }
    Indexer::reset(data).map_err(|e| format!("清空失败：{e}"))?;
    println!("已清空 {}", data.display());
    Ok(())
}

// ============================================================
// 交互模式
// ============================================================

fn repl(data: &Path) -> Result<(), String> {
    let indexer = Indexer::open(data).map_err(|e| format!("打开数据目录失败：{e}"))?;
    let (live, _, _, _) = indexer.stats().map_err(|e| e.to_string())?;

    println!("hub —— 本地离线多源检索中枢");
    println!("索引 {live} 篇，数据目录 {}", data.display());
    println!("直接输入查询；:stats 看规模，exit 退出\n");

    let stdin = io::stdin();
    loop {
        print!("hub> ");
        io::stdout().flush().ok();

        let mut line = String::new();
        if stdin.read_line(&mut line).is_err() || line.is_empty() {
            break;
        }
        let q = line.trim();
        if q.is_empty() {
            continue;
        }
        match q {
            "exit" | "quit" | ":q" => break,
            ":stats" | ":s" => {
                if let Ok((live, tombs, docs, _)) = indexer.stats() {
                    println!("存活 {live} / 墓碑 {tombs} / 倒排 {docs}");
                }
                println!();
                continue;
            }
            ":help" | ":h" => {
                println!(":stats 规模   exit 退出");
                println!("子命令请另开终端：hub index <目录> / hub search <查询>\n");
                continue;
            }
            _ => {}
        }

        let started = Instant::now();
        let hits = indexer.search_text(q, 10).map_err(|e| e.to_string())?;
        let ms = started.elapsed().as_secs_f32() * 1000.0;
        if hits.is_empty() {
            println!("  （无结果）\n");
        } else {
            print_hits(&hits, now_secs());
            println!("  {:.1} ms\n", ms);
        }
    }
    Ok(())
}
