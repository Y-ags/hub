//! hub-daemon —— 常驻后台，盯着你的目录，改了自动进索引。
//!
//! ## 它解决的问题
//!
//! 在此之前，索引只在**你手动敲 `hub index` 的那一刻**是新的。写过的东西
//! 半小时后再搜，大概率搜不到 —— 不是检索错了，是索引根本没跟上。
//! 一个"本地知识中枢"如果要求用户记得手动重建索引，那它就不是中枢，
//! 是个需要人喂的玩具。
//!
//! ## 三层保障（缺一层就会出现幽灵数据）
//!
//! | 层 | 触发 | 代价 | 覆盖的场景 |
//! |---|---|---|---|
//! | watcher 事件 | 文件变动后 ~1 秒 | 极低 | 正常编辑、新建、删除 |
//! | 溢出对账 | 内核缓冲区满 | 中 | 大批量变更（git checkout、解压） |
//! | 周期对账 | 每 10 分钟 | 中 | 睡眠唤醒、进程没在跑的那段时间 |
//!
//! 只有 watcher 是不够的：`ReadDirectoryChangesW` 在事件多于缓冲区时会
//! **静默丢事件并返回 0 字节**，是标准坑。只有周期对账也不够：延迟不可接受。
//!
//! ## 主循环的形状
//!
//! ```text
//! loop {
//!   收事件（最多等 min(debounce 到期, 下次对账, 500ms)）→ 累积到 pending
//!   pending 非空 且 静默超过 debounce → 批量入库
//!   到点 → 对账
//!   到点 → 刷心跳
//!   看到 stop 标记 → 收尾退出
//! }
//! ```
//!
//! 那个 500ms 上限是为了让 `hub daemon stop` 能在半秒内生效 ——
//! 否则一个"等 10 分钟对账"的阻塞调用会让停止操作卡住。

pub mod autostart;
pub mod config;
pub mod log;
pub mod process;
pub mod state;
pub mod watch;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hub_core::SourceId;
use hub_engine::{Engine, IngestReport};

pub use config::{DaemonConfig, HEARTBEAT_STALE_SECS};
pub use process::{force_kill, pid_running, spawn_detached};
pub use state::State;
pub use watch::{FsEvent, FsWatcher};

use log::Log;
use state::State as St;

/// 单轮最多等多久就要检查一次停止标记。
const MAX_TICK: Duration = Duration::from_millis(500);

pub struct Daemon {
    data: PathBuf,
    cfg: DaemonConfig,
    engine: Engine,
    log: Log,
    state: St,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Daemon {
    pub fn new(data: impl AsRef<Path>, cfg: DaemonConfig, echo: bool) -> Result<Self, String> {
        let data = data.as_ref().to_path_buf();
        std::fs::create_dir_all(&data).map_err(|e| format!("创建数据目录失败：{e}"))?;
        let engine =
            Engine::open(&data, cfg.engine.clone()).map_err(|e| format!("打开索引失败：{e}"))?;
        let log = Log::open(St::log_path(&data), echo);
        let state = St {
            pid: std::process::id(),
            started_at: now_secs(),
            heartbeat_at: now_secs(),
            roots: cfg
                .engine
                .roots
                .iter()
                .map(|r| r.display().to_string())
                .collect(),
            ..Default::default()
        };
        Ok(Self { data, cfg, engine, log, state })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn data_dir(&self) -> &Path {
        &self.data
    }

    /// 主循环。返回即表示已经收到停止请求并完成了收尾。
    pub fn run(mut self) -> Result<(), String> {
        // 上一轮残留的 stop 标记必须清掉，否则一启动就退出
        state::clear_stop(&self.data);
        self.state
            .save(&self.data)
            .map_err(|e| format!("写状态失败：{e}"))?;

        self.log.info(format!(
            "启动 pid={} 监听 {} 个目录：{}",
            self.state.pid,
            self.cfg.engine.roots.len(),
            self.cfg
                .engine
                .roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join("  ")
        ));

        // 先把 watcher 起起来再跑冷启动对账：对账期间发生的变更会堆在
        // channel 里，等对账结束再一起处理，一条都不会丢。
        let watcher = match FsWatcher::start(&self.cfg.engine.roots, Duration::from_secs(5)) {
            Ok(w) => {
                self.log.info("文件监听已就绪");
                Some(w)
            }
            Err(e) => {
                self.log
                    .warn(format!("文件监听启动失败（仍会按周期对账）：{e}"));
                None
            }
        };

        // 冷启动对账：一次遍历把"磁盘上有什么"和"索引里有什么"拉平
        self.log.info("开始冷启动对账…");
        match self.engine.reconcile() {
            Ok(rep) => {
                self.apply(&rep, "冷启动对账");
                self.state.last_reconcile_at = now_secs();
                self.state.reconcile_runs += 1;
            }
            Err(e) => self.log.error(format!("冷启动对账失败：{e}")),
        }
        if self.cfg.engine.with_history {
            match self.engine.ingest_history() {
                Ok(rep) => self.apply(&rep, "终端历史"),
                Err(e) => self.log.error(format!("终端历史采集失败：{e}")),
            }
        }
        let _ = self.state.save(&self.data);

        let debounce = Duration::from_millis(self.cfg.debounce_ms.max(50));
        let reconcile_every = Duration::from_secs(self.cfg.reconcile_secs.max(5));
        let heartbeat_every = Duration::from_secs(self.cfg.heartbeat_secs.max(1));
        let merge_every = Duration::from_secs(self.cfg.merge_secs.max(5));
        let merge_idle = Duration::from_secs(self.cfg.merge_idle_secs);
        // 只开了 with_history 才轮询：没开的人不该为一份不采集的数据付 stat 成本
        let history_poll = if self.cfg.engine.with_history && self.cfg.history_poll_secs > 0 {
            Some(Duration::from_secs(self.cfg.history_poll_secs.max(1)))
        } else {
            None
        };

        let mut pending: BTreeSet<PathBuf> = BTreeSet::new();
        let mut buf: Vec<FsEvent> = Vec::new();
        let mut last_evt = Instant::now();
        let mut next_reconcile = Instant::now() + reconcile_every;
        let mut next_heartbeat = Instant::now() + heartbeat_every;
        let mut next_history =
            Instant::now() + history_poll.unwrap_or(Duration::from_secs(3600));
        // 冷启动刚扫完，段可能已经积了一堆（旧版本留下的，或者停机期间攒的）。
        // 这种一次性的堆积不该再等一个完整周期，给它 15 秒就收拾掉。
        self.state.segments = self.engine.segment_count();
        let backlogged = self.state.segments > self.cfg.merge_min_segments * 2;
        let mut next_merge = Instant::now()
            + if backlogged {
                Duration::from_secs(15)
            } else {
                merge_every
            };
        let _ = self.state.save(&self.data);
        if backlogged {
            self.log.info(format!(
                "索引有 {} 个段（阈值 {}），15 秒后做一次合并",
                self.state.segments, self.cfg.merge_min_segments
            ));
        }
        let mut force_reconcile = false;

        loop {
            if state::stop_requested(&self.data) {
                self.log.info("收到停止请求，收尾中…");
                break;
            }

            // ---- 1. 收事件 ----
            let tick_start = Instant::now();
            let until_debounce = if pending.is_empty() {
                MAX_TICK
            } else {
                (last_evt + debounce).saturating_duration_since(tick_start)
            };
            let until_reconcile = next_reconcile.saturating_duration_since(tick_start);
            let wait = until_debounce.min(until_reconcile).min(MAX_TICK);

            buf.clear();
            match &watcher {
                Some(w) => {
                    let _ = w.drain_into(&mut buf, wait);
                }
                None => std::thread::sleep(wait),
            }

            // ---- 2. 归并事件 ----
            if !buf.is_empty() {
                for ev in buf.drain(..) {
                    match ev {
                        FsEvent::Upsert(p) | FsEvent::Remove(p) => {
                            // 刻意不区分类型：同一路径可能在一次保存里
                            // 既 Remove 又 Upsert，最终以处理时刻的 stat 为准
                            pending.insert(p);
                        }
                        FsEvent::Overflow => {
                            self.log.warn("内核事件缓冲区溢出，安排一次全量对账");
                            force_reconcile = true;
                        }
                        FsEvent::Error(msg) => self.log.error(msg),
                    }
                }
                last_evt = Instant::now();
            }

            // ---- 3. debounce 到期 → 批量入库 ----
            if !pending.is_empty() && Instant::now().duration_since(last_evt) >= debounce {
                self.process_pending(&mut pending);
                self.state.last_event_at = now_secs();
            }

            // ---- 4. 对账 ----
            let now = Instant::now();
            let reconcile_due = now >= next_reconcile
                || (force_reconcile && now.duration_since(last_evt) >= debounce);
            if reconcile_due {
                force_reconcile = false;
                next_reconcile = now + reconcile_every;
                match self.engine.reconcile() {
                    Ok(rep) => {
                        if rep.churned() || rep.errors > 0 {
                            self.apply(&rep, "周期对账");
                        }
                        self.state.last_reconcile_at = now_secs();
                        self.state.reconcile_runs += 1;
                    }
                    Err(e) => self.log.error(format!("周期对账失败：{e}")),
                }
            }

            // ---- 5. 空闲时合并段 ----
            //
            // 为什么必须做：每批写入都是一个新段，删掉的文档只是墓碑。
            // 不合并的话，段数会随编辑次数线性增长，查询要逐段打分越来越慢，
            // 而删掉的文件还一直占着磁盘。
            //
            // 为什么不能随时做：合并要独占写入器并重写段文件，
            // 撞上正在保存的文件会让"改完立刻能搜到"失效。所以只在
            // 距上次变更静默 merge_idle 之后动手。
            let merge_now = Instant::now();
            if merge_now >= next_merge {
                let idle = pending.is_empty() && merge_now.duration_since(last_evt) >= merge_idle;
                // 到点之后最多再等一个完整周期。没有这条兜底，"用户一直在编辑"
                // 就等于"永远不合并" —— 而连续编辑几小时恰恰是最需要合并的场景。
                // 超时后硬合并的代价只是这一瞬间的写入要等写入器锁，不会丢数据。
                let overdue = merge_now.duration_since(next_merge) >= merge_every;
                if idle || overdue {
                    next_merge = merge_now + merge_every;
                    self.state.segments = self.engine.segment_count();
                    match self.engine.merge_if_needed(self.cfg.merge_min_segments) {
                        Ok(true) => {
                            self.state.last_merge_at = now_secs();
                            self.state.merge_runs += 1;
                            self.state.segments = self.engine.segment_count();
                            self.log.info(format!(
                                "段合并完成{}：{} 个段，累计第 {} 次",
                                if overdue && !idle { "（排队太久，强行合并）" } else { "" },
                                self.state.segments,
                                self.state.merge_runs
                            ));
                            let _ = self.state.save(&self.data);
                        }
                        // 没什么可合的。不刷日志：这条每 3 小时一次，
                        // 刷出来只会让日志里全是"无事发生"
                        Ok(false) => {}
                        Err(e) => self.log.error(format!("段合并失败：{e}")),
                    }
                }
                // 不空闲就什么都不动，下一个 tick 再看 —— 判断成本只是两个
                // 内存比较，比"推迟 5 分钟"精确得多（推迟会整整错过一个空窗）
            }

            // ---- 6. 终端历史轮询 ----
            //
            // 历史文件不归 watcher 管：watcher 监听的是用户挑的目录，而
            // PSReadLine 的历史在 `%APPDATA%\...\PSReadLine\` 下。为它把整个
            // APPDATA 加进 roots 会把上千个无关文件也索引进来。
            //
            // 所以改成轮询。代价可以忽略 —— 文件大小和 mtime 都没变时，
            // Engine 内部直接返回，一次轮询只是几次 stat。
            if let Some(every) = history_poll {
                let hnow = Instant::now();
                if hnow >= next_history {
                    next_history = hnow + every;
                    self.state.history_polls += 1;
                    match self.engine.ingest_history() {
                        Ok(rep) => {
                            if rep.churned() {
                                self.apply(&rep, "终端历史");
                                self.state.last_history_at = now_secs();
                                let _ = self.state.save(&self.data);
                            }
                        }
                        Err(e) => self.log.error(format!("终端历史采集失败：{e}")),
                    }
                }
            }

            // ---- 7. 心跳 ----
            let now = Instant::now();
            if now >= next_heartbeat {
                next_heartbeat = now + heartbeat_every;
                self.state.heartbeat_at = now_secs();
                self.state.ticks += 1;
                if let Err(e) = self.state.save(&self.data) {
                    self.log.warn(format!("写状态失败：{e}"));
                }
            }
        }

        // 收尾：把攒着的一次性写掉，别留下"改了但没进索引"的状态
        if !pending.is_empty() {
            self.process_pending(&mut pending);
        }
        if let Err(e) = self.engine.indexer_mut().flush() {
            self.log.error(format!("退出前提交失败：{e}"));
        }
        state::clear_stop(&self.data);
        St::clear(&self.data);
        self.log.info("已退出");
        Ok(())
    }

    /// 处理攒下来的路径。
    ///
    /// 关键设计：**不区分事件类型**，只看处理时刻的 `stat` 结果。
    /// 因为一个保存动作可能产生 `Remove + Upsert` 序列（编辑器先删再写），
    /// 按事件类型处理会让同一条路径既进又出。以最终状态为准才是对的。
    fn process_pending(&mut self, pending: &mut BTreeSet<PathBuf>) {
        if pending.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = std::mem::take(pending).into_iter().collect();

        // 先处理"已经不存在"的：这些要连同子树一起删
        let mut removed = 0usize;
        for p in &paths {
            if std::fs::symlink_metadata(p).is_ok() {
                continue;
            }
            let uri = hub_engine::engine::to_uri(p);
            match self
                .engine
                .indexer_mut()
                .delete_under(SourceId::Files, &uri)
            {
                Ok(n) => removed += n,
                Err(e) => self.log.warn(format!("删除 {} 失败：{e}", p.display())),
            }
        }

        // 再处理"存在"的：一次 flush 会把上面的删除和这里的写入合成一次提交
        let report = match self.engine.index_paths(&paths) {
            Ok(r) => r,
            Err(e) => {
                self.log.error(format!("索引变更失败：{e}"));
                return;
            }
        };

        let total_deleted = removed + report.deleted;
        // 逐条记下每个路径看到了什么，但**只在整批都没做成时**输出。
        // 正常编辑时每批都会打进索引，把每个路径都打一遍会把日志淹掉；
        // 而一旦"什么都没做成"，这几行就是唯一能看出原因的东西
        // （事件没来 / 路径对不上 / 跳过规则误杀 / 文件正在被写）。
        if report.indexed == 0 && total_deleted == 0 {
            for p in &paths {
                let desc = match std::fs::metadata(p) {
                    Ok(m) => format!(
                        "文件={} 大小={} 可索引={}",
                        m.is_file(),
                        m.len(),
                        self.engine.path_indexable(p)
                    ),
                    Err(e) => format!("stat 失败({e})"),
                };
                self.log.info(format!("  · {} → {desc}", p.display()));
            }
        }

        // 无条件打日志：只统计"成功写入"的话，一旦跳过规则误杀或 stat 失败，
        // 表现就是"改了文件、daemon 一点反应都没有"，最难排查那种。
        self.log.info(format!(
            "变更：检查 {} 个路径 → 入库 {}，删除 {}，跳过 {}，失败 {}，用时 {}ms",
            paths.len(),
            report.indexed,
            total_deleted,
            report.skipped,
            report.errors,
            report.elapsed_ms
        ));
        if report.indexed > 0 || total_deleted > 0 {
            self.state.indexed_total += report.indexed;
            self.state.deleted_total += total_deleted;
            let _ = self.state.save(&self.data);
        }
    }

    fn apply(&mut self, rep: &IngestReport, what: &str) {
        self.state.indexed_total += rep.indexed;
        self.state.deleted_total += rep.deleted;
        if rep.churned() || rep.errors > 0 {
            self.log.info(format!(
                "{}：扫描 {}，入库 {}，删除 {}，跳过 {}，用时 {}ms",
                what, rep.scanned, rep.indexed, rep.deleted, rep.skipped, rep.elapsed_ms
            ));
        } else {
            self.log.info(format!(
                "{}：扫描 {}，无变化，用时 {}ms",
                what, rep.scanned, rep.elapsed_ms
            ));
        }
    }
}

/// 供 `hub daemon status` 使用：读状态并判断进程是否还活着
pub fn describe_state(data: &Path) -> Option<(State, bool)> {
    let s = State::load(data)?;
    let alive = s.is_alive();
    Some((s, alive))
}

/// 把时间戳格式化，供 CLI 复用
pub fn human_time(secs: i64) -> String {
    state::fmt_time(secs)
}
