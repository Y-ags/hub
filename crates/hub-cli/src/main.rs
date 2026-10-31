//! hub 命令行入口。
//!
//! 一个原则：**这里不写任何检索逻辑，也不写采集逻辑**。
//! 查询解析、排序、增量判定、持久化、遍历都是下面各层的事，
//! CLI 只负责参数、IO 和打印。
//!
//! 引入 daemon 后这条原则变得更关键：`hub index` 和后台自动更新必须走
//! **同一份**采集流水线（`hub-engine`），否则两条路径的行为会分叉 ——
//! 第一版就是这么坏的：`parse_query` 只在 CLI 里调用，持久化层自己另搞一套。

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use hub_core::ParsedQuery;
use hub_daemon::{Daemon, DaemonConfig};
use hub_engine::{Engine, EngineConfig};
use hub_store::indexer::now_secs;
use hub_store::{Hit, Indexer};

const USAGE: &str = "\
hub —— 本地离线多源个人检索中枢

日常用法（推荐）:
  hub daemon start ~/code ~/notes --history
        启动后台守护进程，之后改了文件自动进索引，不用再手动 index
  hub search <查询>
        自然语言检索。支持「上个月」「代码片段」\"精确短语\" -排除词

索引维护:
  hub index <目录>... [--history] [--max-mb N] [--reset]
        手动全量扫描（daemon 在跑时通常不需要）
  hub stats        查看索引规模与来源分布
  hub merge        回收已删文档占用的倒排空间
  hub reset --yes  清空索引与水位表（破坏性操作）

后台进程:
  hub daemon start [目录]... [--history] [--fg] [--reconcile 秒] [--debounce 毫秒]
        启动后台更新。首次启动会把目录记进配置，之后可省略目录直接 start
  hub daemon status    看是否在跑、监听了什么、干了多少活
  hub daemon stop [--force]
        优雅停止（写停止标记，等它把攒着的数据写完）；--force 强制结束
  hub daemon log [-n 50]
        看后台日志
  hub daemon run       前台运行（调试用，日志直接打屏）

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
        "daemon" | "d" => cmd_daemon(&data, &args[1..]),
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
                max_mb = a[9..]
                    .parse()
                    .map_err(|_| "--max-mb 需要一个整数".to_string())?;
            }
            _ if a.starts_with("--") => return Err(format!("未知参数 {a}")),
            _ => roots.push(PathBuf::from(a)),
        }
    }

    if roots.is_empty() && !with_history {
        return Err("没有指定扫描路径。用法：hub index <目录> [--history]".into());
    }

    // reset 必须先于 open：Windows 上被 mmap 的段文件删不掉
    if reset {
        Indexer::reset(data).map_err(|e| format!("清空索引失败：{e}"))?;
        println!("已清空 {}", data.display());
    }

    let cfg = EngineConfig {
        roots,
        max_file_bytes: max_mb.max(1) * 1024 * 1024,
        with_history,
        ..Default::default()
    };
    let mut engine =
        Engine::open(data, cfg).map_err(|e| format!("打开数据目录失败：{e}"))?;

    let started = Instant::now();
    let rep = engine.scan_all().map_err(|e| format!("扫描失败：{e}"))?;

    if rep.skipped_dir + rep.skipped_ext + rep.skipped_size + rep.skipped_symlink > 0 {
        println!(
            "跳过：目录 {} / 扩展名 {} / 超限 {} / 软链 {}",
            rep.skipped_dir, rep.skipped_ext, rep.skipped_size, rep.skipped_symlink
        );
    }

    println!(
        "\n完成：扫描 {} 条，入索引 {} 条，跳过 {} 条，出错 {} 次，用时 {:.2}s\n\
         数据目录：{}（用 hub stats 查看规模）",
        rep.scanned,
        rep.indexed,
        rep.skipped,
        rep.errors,
        started.elapsed().as_secs_f32(),
        data.display()
    );
    if !with_history {
        println!("提示：想让改动自动进索引，用 hub daemon start <目录>");
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
// daemon
// ============================================================

fn cmd_daemon(data: &Path, args: &[String]) -> Result<(), String> {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    let rest: &[String] = if args.is_empty() { &[] } else { &args[1..] };
    match sub {
        "start" | "up" => daemon_start(data, rest),
        "run" => daemon_run(data, rest),
        "stop" | "down" => daemon_stop(data, rest),
        "status" | "st" => daemon_status(data),
        "log" | "logs" => daemon_log(data, rest),
        other => Err(format!(
            "未知 daemon 子命令 `{other}`。可用：start / stop / status / log / run"
        )),
    }
}

/// 解析 `start` 的参数
fn parse_start_args(args: &[String]) -> Result<(Vec<PathBuf>, bool, bool, Option<u64>, Option<u64>), String> {
    let mut roots = Vec::new();
    let mut history = false;
    let mut fg = false;
    let mut reconcile = None;
    let mut debounce = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--history" | "-H" => history = true,
            "--fg" => fg = true,
            "--reconcile" => {
                reconcile = Some(
                    it.next()
                        .and_then(|v| v.parse().ok())
                        .ok_or_else(|| "--reconcile 需要秒数".to_string())?,
                );
            }
            "--debounce" => {
                debounce = Some(
                    it.next()
                        .and_then(|v| v.parse().ok())
                        .ok_or_else(|| "--debounce 需要毫秒数".to_string())?,
                );
            }
            _ if a.starts_with("--") => return Err(format!("未知参数 {a}")),
            _ => roots.push(PathBuf::from(a)),
        }
    }
    Ok((roots, history, fg, reconcile, debounce))
}

fn daemon_start(data: &Path, args: &[String]) -> Result<(), String> {
    let (roots, history, fg, reconcile, debounce) = parse_start_args(args)?;

    // 已经在跑就别再起一个：两个进程同时写同一个索引，Tantivy 的写入器锁
    // 会互相阻塞，最后变成两个都在等对方
    if let Some((st, true)) = hub_daemon::describe_state(data) {
        println!(
            "守护进程已在运行（pid {}，启动于 {}）。要重启请先 hub daemon stop",
            st.pid,
            hub_daemon::human_time(st.started_at)
        );
        return Ok(());
    }

    // 目录：本次给了就覆盖，没给就沿用上次的配置
    let mut cfg = DaemonConfig::load(data).map_err(|e| format!("读取配置失败：{e}"))?;
    if !roots.is_empty() {
        // 转成绝对路径再存：配置里留着相对路径，下次在别的目录启动就会指错地方
        cfg.engine.roots = roots
            .iter()
            .map(|r| std::path::absolute(r).unwrap_or_else(|_| r.clone()))
            .collect();
    }
    if history {
        cfg.engine.with_history = true;
    }
    if let Some(v) = reconcile {
        cfg.reconcile_secs = v;
    }
    if let Some(v) = debounce {
        cfg.debounce_ms = v;
    }

    if cfg.engine.roots.is_empty() {
        return Err(
            "没有要监听的目录。用法：hub daemon start <目录>... [--history]\n\
             目录会记进配置，下次直接 hub daemon start 即可"
                .into(),
        );
    }
    for r in &cfg.engine.roots {
        if !r.exists() {
            eprintln!("警告：目录不存在，会被跳过 —— {}", r.display());
        }
    }

    cfg.save(data).map_err(|e| format!("写配置失败：{e}"))?;

    if fg {
        return daemon_run(data, &[]);
    }

    let pid = hub_daemon::spawn_detached(data).map_err(|e| format!("启动失败：{e}"))?;

    // 验证真的站住了。
    //
    // 不能只看"心跳文件出现了"就报成功：`Daemon::new` 会在启动瞬间写一次心跳，
    // 所以一个随后立刻死掉（被 Job 带走 / 一启动就 panic）的进程，也会留下心跳。
    // 这里等一段时间**看进程是否还活着**，这是唯一可靠的判据。
    let mut alive_at_end = false;
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !hub_daemon::pid_running(pid) {
            break;
        }
        alive_at_end = true;
    }

    if alive_at_end {
        println!("守护进程已启动（pid {pid}，后台运行）");
        println!("监听目录：");
        for r in &cfg.engine.roots {
            println!("  {}", r.display());
        }
        println!("\n之后改了文件会自动进索引，不需要再手动 index。");
        println!("日志：{}", data.join("daemon.log").display());
        println!("查看状态：hub daemon status    停止：hub daemon stop");
        Ok(())
    } else {
        Err(format!(
            "守护进程拉起后立刻退出了（pid {pid}）。\n\
             常见原因：数据目录不可写、索引被别的进程占用。\n\
             日志：{}",
            data.join("daemon.log").display()
        ))
    }
}

fn daemon_run(data: &Path, args: &[String]) -> Result<(), String> {
    let (roots, history, _, reconcile, debounce) = parse_start_args(args)?;
    let mut cfg = DaemonConfig::load(data).map_err(|e| format!("读取配置失败：{e}"))?;
    if !roots.is_empty() {
        cfg.engine.roots = roots
            .iter()
            .map(|r| std::path::absolute(r).unwrap_or_else(|_| r.clone()))
            .collect();
    }
    if history {
        cfg.engine.with_history = true;
    }
    if let Some(v) = reconcile {
        cfg.reconcile_secs = v;
    }
    if let Some(v) = debounce {
        cfg.debounce_ms = v;
    }
    if cfg.engine.roots.is_empty() {
        return Err("没有要监听的目录。用法：hub daemon run <目录>...".into());
    }
    // 前台调试时给的目录也记进配置：否则调完再 `hub daemon start` 会突然说
    // "没有要监听的目录"，用户还得再把目录敲一遍
    cfg.save(data).map_err(|e| format!("写配置失败：{e}"))?;

    let d = Daemon::new(data, cfg, true).map_err(|e| e.to_string())?;
    d.run()
}

fn daemon_stop(data: &Path, args: &[String]) -> Result<(), String> {
    let force = args.iter().any(|a| a == "--force" || a == "-f");

    let st = match hub_daemon::describe_state(data) {
        Some((st, _)) => st,
        None => {
            println!("守护进程没有在运行（无状态文件）");
            return Ok(());
        }
    };

    // 进程已经不在：不用写标记，直接清状态。
    // 这里必须查 pid 而不是心跳 —— 心跳 15 秒才写一次，
    // 只看心跳会把"刚退出"的进程误判成还在跑，白等 15 秒。
    if !st.pid_alive() {
        println!(
            "守护进程没有在运行（pid {} 已退出，最后一次心跳 {}）",
            st.pid,
            hub_daemon::human_time(st.heartbeat_at)
        );
        hub_daemon::State::clear(data);
        return Ok(());
    }

    if force {
        if hub_daemon::force_kill(st.pid) {
            hub_daemon::State::clear(data);
            println!("已强制结束 pid {}（可能有未提交的变更，下次启动会重建）", st.pid);
        } else {
            return Err(format!("强制结束 pid {} 失败（权限不足或进程已退出）", st.pid));
        }
        return Ok(());
    }

    // 优雅停止：写标记 → 等它把攒着的变更写完
    hub_daemon::state::request_stop(data).map_err(|e| format!("写停止标记失败：{e}"))?;
    print!("正在停止");
    io::stdout().flush().ok();

    for _ in 0..150 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        print!(".");
        io::stdout().flush().ok();
        if !st.pid_alive() {
            println!("\n已停止");
            hub_daemon::State::clear(data);
            return Ok(());
        }
    }

    println!();
    Err(format!(
        "15 秒内没有退出（pid {} 可能卡在系统调用里）。\n\
         可以再试一次，或用 hub daemon stop --force 强制结束",
        st.pid
    ))
}

fn daemon_status(data: &Path) -> Result<(), String> {
    let (st, _) = match hub_daemon::describe_state(data) {
        Some(x) => x,
        None => {
            println!("守护进程：未运行（没有状态文件）");
            println!("启动：hub daemon start <目录>");
            return Ok(());
        }
    };

    if st.is_alive() {
        println!(
            "守护进程：运行中    pid {}    已运行 {}",
            st.pid,
            human_age(st.started_at, now_secs())
        );
    } else if st.is_stalled() {
        println!(
            "守护进程：无响应    pid {} 还在，但心跳停在 {}（{}前）",
            st.pid,
            hub_daemon::human_time(st.heartbeat_at),
            human_age(st.heartbeat_at, now_secs())
        );
        println!("  可能卡在系统调用里，或正在做大目录的冷启动扫描");
    } else {
        println!(
            "守护进程：已停止（pid {} 已退出，最后心跳 {}）",
            st.pid,
            hub_daemon::human_time(st.heartbeat_at)
        );
    }

    if !st.roots.is_empty() {
        println!("监听目录：");
        for r in &st.roots {
            let mark = if Path::new(r).exists() { "" } else { "  [不存在]" };
            println!("  {r}{mark}");
        }
    }

    println!(
        "最近变化：{}",
        if st.last_event_at > 0 {
            hub_daemon::human_time(st.last_event_at)
        } else {
            "（还没有）".into()
        }
    );
    println!(
        "最近对账：{}（共 {} 次）",
        if st.last_reconcile_at > 0 {
            hub_daemon::human_time(st.last_reconcile_at)
        } else {
            "（还没跑）".into()
        },
        st.reconcile_runs
    );
    println!(
        "累计写入：{} 条    删除：{} 条",
        st.indexed_total, st.deleted_total
    );
    println!("日志：{}", data.join("daemon.log").display());
    Ok(())
}

fn daemon_log(data: &Path, args: &[String]) -> Result<(), String> {
    let mut n = 50usize;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-n" || a == "--lines" {
            n = it
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| "-n 需要一个整数".to_string())?;
        }
    }
    let path = data.join("daemon.log");
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("读取日志失败（{}）：{e}", path.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    for l in &lines[start..] {
        println!("{l}");
    }
    Ok(())
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
