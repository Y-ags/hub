# hub — 本地离线多源个人检索中枢

纯 Rust。把文件系统、终端历史等散落各处的数据统一采集、归一化、建索引，
用一句话搜到任何东西。**全部本地、离线、不上传。**

```bash
hub daemon start ~/code ~/notes --history --autostart   # 起一次，重启电脑也还在
hub search 上个月写的那个rust内存相关的代码片段
```

起了 daemon 之后，**改文件不用敲任何命令**，一秒后就能搜到新内容。
加了 `--autostart` 就再也不需要管它了：登录 Windows 自动拉起，索引段自己合并。

---

## 快速上手

```bash
cargo build --release          # 首次约 2–5 分钟（Tantivy + bundled SQLite）
cargo test                     # 90 条测试，全部离线可跑

./target/release/hub daemon start ~/code ~/notes --history --autostart  # 一步到位
./target/release/hub daemon status                            # 看它在干什么
./target/release/hub search "零拷贝 mmap" --limit 5
./target/release/hub daemon log -n 20                         # 后台日志
./target/release/hub daemon stop                              # 优雅停止（不影响开机自启）
```

目录会记进配置，第二次直接 `hub daemon start` 即可。

不想一开始就装自启，可以事后单独装：

```bash
./target/release/hub daemon install     # 只装开机自启，不启动
./target/release/hub daemon uninstall   # 取消
```

不想常驻也能手动索引（增量，内容没变的直接跳过）：

```bash
./target/release/hub index ~/code ~/notes --history
./target/release/hub stats
```

数据目录默认 `~/.hub`（可用 `HUB_HOME` 或 `--data <dir>` 覆盖）：

```
~/.hub/
├── meta.sqlite      水位表：content_hash / mtime / tombstone / doc_id（事实源）
├── index/           Tantivy 倒排（可重建，删了不心疼）
├── daemon.conf      监听哪些目录、debounce / 对账 / 合并 / 历史轮询间隔
├── daemon.state     pid / 心跳 / 段数 / 累计统计
└── daemon.log       后台日志（5MB 滚动）
```

**验证过的真实数字**（Windows，debug 构建）：

| 动作 | 结果 |
|---|---|
| 全量索引 11891 条 PowerShell 历史 | 1.97 s，去重后 6327 篇 |
| 单次查询 | 0.5–4 ms |
| 二次索引（增量） | 0.01 s，全部跳过 |
| 改一个文件 → 可搜到 | 1 秒内（debounce 800ms） |
| 冷启动对账（几千文件） | 数十毫秒 |

---

## 后台自动更新

三层保障，缺一层就会有幽灵数据：

| 层 | 触发 | 代价 | 覆盖的场景 |
|---|---|---|---|
| watcher 事件 | 文件变动后 ~1s | 极低 | 正常编辑、新建、删除 |
| 溢出对账 | 内核缓冲区满 | 中 | 大批量变更（git checkout、解压） |
| 周期对账 | 每 10 分钟 | 中 | 睡眠唤醒、进程没在跑的那段时间 |

只有 watcher 不够：`ReadDirectoryChangesW` 在事件多于缓冲区时**静默丢事件并返回 0 字节**。
只有周期对账也不够：延迟不可接受。

可调参数：`hub daemon start --reconcile 300 --debounce 500 --merge 7200`，或直接改 `daemon.conf`。

### 空闲时自动合并段

每批写入都是一个新索引段，删掉的文件只是打个墓碑。不合并的话：

- 段数随编辑次数线性增长，每次查询都要**逐段打分**，越用越慢
- 删除的文档一直占着磁盘，"删了但空间没回来"

daemon 每 3 小时（`merge_secs`）检查一次，在**距上次文件变更静默 2 分钟**之后动手，
免得合并抢占写入器、把"改完一秒能搜到"拖慢。到点但用户一直在编辑时最多再等一个周期，
超时就强行合并 —— 否则"连续工作一整天"就等于"永远不合并"。

判断依据是"段数 ≥ 8"或"有墓碑"，都没有就什么都不做（不会为了合并而重写文件）。

### 终端历史：轮询而不是监听

历史文件不归 watcher 管 —— watcher 监听的是你挑的目录，而 PSReadLine 的历史在
`%APPDATA%\...\PSReadLine\` 下。为了它把整个 `%APPDATA%` 加进监听列表，会把上千个
无关文件也索引进来。

所以改成每 5 秒轮询（`history_poll_secs`）。代价可以忽略：

| 情况 | 代价 |
|---|---|
| 文件没变（99% 的轮次） | 几次 `stat`，连解析都不做 |
| 文件追加了一条命令 | 解析全文 + 只写那一条（已索引的靠内存集合挡掉） |
| 冷启动（进程刚起） | 全解析一次，几千条历史约几百毫秒到几秒 |

实测：11890 条 PowerShell 历史，首次入库 6326 条（重复命令自动去重）用时 4.4 秒；
之后追加一条命令，2 秒内可搜到。

### 开机自启

```bash
hub daemon start ~/code --autostart   # 启动 + 装自启
hub daemon install                    # 只装自启
hub daemon uninstall                  # 取消
```

写在 `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`，值名 `HubDaemon`：

- **不需要管理员权限**（HKCU 而非 HKLM），个人工具没有理由要 UAC
- 在任务管理器「启动」标签里能看到，也能临时禁用而不删
- `hub daemon status` 会显示当前自启命令，方便核对指向哪个路径

`daemon stop` 只是停进程，**不会**关掉自启 —— 那正是"下次开机还要自动起来"的意思。

---

## 架构

```
hub-cli         参数 / IO / 打印，不含检索逻辑也不含采集逻辑
hub-daemon      常驻进程：watcher + debounce + 对账 + 空闲合并 + 历史轮询 + 自启
hub-engine      采集流水线（scan_all / index_paths / remove_paths / reconcile）
hub-store       MetaStore(SQLite) + HubIndex(Tantivy) + Indexer(胶水)
hub-core        查询解析 / 中文分词 / BM25 / tombstone（零依赖）
hub-connectors  files / terminal 采集器（零依赖）
```

**四条不可违背的边界**

1. **SQLite 是事实源，Tantivy 是能重建的缓存。** 索引删了能重建，元数据删了就丢了。
2. **`parse_query` 是语义的唯一入口。** 索引层不许自己解析字符串 —— 第一版就是把原始
   字符串丢给 Tantivy 的 `QueryParser`，结果「上个月」「代码片段」「-排除」一个都没生效，
   而 hub-core 那 30 条测试照样全绿。
3. **分词必须同源。** 索引与查询都走 `hub_word` / `hub_bigram` 两个自定义分词器，
   底层就是 `hub_core::word_tokens` / `bigram_tokens`。索引用 jieba、查询用自研词典，
   切出来的 term 对不上，会变成"能建索引但搜不到"，且不报错。
4. **采集只有一条路径。** `hub index` 和 daemon 都只能通过 `hub-engine` 写数据。
   两条路径各写一遍"怎么遍历、怎么判重"，行为必然分叉 —— 第 2 条就是这么坏的。

---

## 查询语义

| 能力 | 例子 | 实现位置 |
|---|---|---|
| 时间 | `上个月`、`三个月前`、`3 天前`、`十二个月前` | `time_expr.rs` |
| 来源 | `代码片段`→files、`命令`→terminal、`截图`→ocr | `source_hint.rs` |
| 显式来源 | `source:terminal xxx`、`in:code` | `query.rs` |
| 精确短语 | `内存 "零拷贝"`（能**引入**文档，不只提权） | `index.rs` |
| 排除 | `-malloc`（大小写不敏感） | `index.rs` |

排序：BM25（词级 1.0 / bigram 0.6 双通道）→ 短语相对加成 `max×2+1` → 排除 →
时间惩罚 ×0.15 → 来源硬过滤（空结果回退软惩罚）→ 时间衰减（90 天，62 天半衰）。

---

## 纠错清单（照着天真想法做会卡住的地方）

### Tantivy / rusqlite

| 原假设 | 真实情况 |
|---|---|
| `MmapDirectory::open` 会建目录 | **不会**。目录不存在直接 `DoesNotExist`，`create_dir_all` 得自己写 |
| `IndexWriterBuilder::memory_budget()` | 不存在，入口是 `index.writer(budget)` |
| `IndexWriterOptions` 是 0.27 才有的 | **0.26.1 就有了**（`indexer/index_writer.rs:50`），入口 `writer_with_options` |
| writer 预算可以随便给小 | 每线程下限 15MB，小于它返回 `InvalidArgument` |
| `TopDocs` 可直接当 Collector | 0.26 breaking change：必须 `.order_by_score()` |
| SQLite 支持 u64 | 不支持，`FromSql` 无 u64 实现，用 i64 存取再转 |
| `rusqlite features = ["fts5"]` | 没有这个 feature，只有 `bundled`（fts5 是 SQLite 编译开关） |

### 检索语义

| 原假设 | 真实情况 |
|---|---|
| 索引用 jieba、查询用自研词典 | term 对不上，静默搜不到。必须同源 |
| 给 source 加 `Must` 过滤就行 | **错**。外层有 `Must` 时 `Should` 变可选，同来源文档 0 分混入结果 |
| "十" 可以跳过 | `cn_digit('十')` 是 `None`，收集循环会在"十"处断开：`十二个月前`→2 个月 |
| 改文件时直接 `add_document` 就是"更新" | **错**。Tantivy 没有 upsert，旧文档变成僵尸：搜索结果按 doc_id 去重把它藏起来，`stats` 里"倒排中"才会露馅（实测 15 vs 存活 6）。写入前必须先 `delete_term(uri)` |
| 历史条目的 uri 用 `mtime + 命令` 算 | 纯文本历史（PSReadLine）没有时间戳，mtime 一追加就变 → **整份历史的 uri 全变**，每轮重写几千条。无时间戳时只能按命令算 uri |

### 文件监听（`ReadDirectoryChangesW`）

| 原假设 | 真实情况 |
|---|---|
| `CloseHandle` 能取消阻塞中的调用 | **不能**。同步模式下必须 `CancelSynchronousIo`，否则线程永远卡住、进程退不掉 |
| 把 `FILE_NOTIFY_INFORMATION` 拷出来再读 `FileName` | **错**。`FileName` 是 `[u16; 1]`，拷贝到栈上后指针指向副本，读到一串垃圾，stat 报 "strings passed to WinAPI cannot contain NULs"。要按固定偏移直接从缓冲区读 |
| 缓冲区 64KB 够用 | 溢出时**返回 0 字节而不是报错**，必须当作"全量对账"信号 |
| mtime 相同就是没变 | mtime 只有**秒**精度，同一秒内的修改会被吃掉。要给"刚改过的文件"加新鲜度保护 |
| 跳过规则拿绝对路径判 | root 在 `%TEMP%` / `%APPDATA%` 下时整棵树都被判成噪声。只能判 root 以下的相对路径 |
| `DETACHED_PROCESS` 就够脱离父进程 | 父进程在 Job Object 里时还会被一起带走，要加 `CREATE_BREAKAWAY_FROM_JOB` |

---

## 已知边界

- **没有图形界面**：只有 CLI
- **没有截图 OCR、聊天记录 connector**
- **开机自启只实现了 Windows**（HKCU Run 键）；Linux / macOS 需自己配 systemd unit / launchd
- **非 UTF-8 文件直接跳过**（GBK 会变乱码，宁可漏不要脏）
- **重排窗口 2000**：时间/来源惩罚在 BM25 之后做，超出窗口的文档不参与重排
- **Windows 上索引目录被占用时删不掉**：`hub reset` 需要先 `hub daemon stop`
- **终端历史每 5 秒轮询一次**（不是事件监听）：文件大小和 mtime 都没变时只花几次
  `stat`；敲完命令到能搜到最多差 5 秒。`history_poll_secs=0` 可关掉
- **历史条目常驻内存一份 uri 集合**：约 50 字节/条，10 万条历史约 5MB
- **目录重命名会当成"删 + 增"**：内容没变也要重新分词一遍，正确但有开销

## 下一步

```
1. hub-daemon ↔ CLI 的 IPC（现在靠状态文件，能读不能问）
2. egui UI + global-hotkey
3. 截图 OCR / 聊天记录 connector
4. 真机内存基准（上线前必须做）
```
