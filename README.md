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

> 从零开始（装工具链 → 构建 → 第一次运行 → 验证清单 → 排错）的完整步骤在
> **[RUNBOOK.md](RUNBOOK.md)**。下面是最短的一条路。

```bash
cargo build --release          # 首次约 2–5 分钟（Tantivy + bundled SQLite）
cargo test                     # 142 条测试，全部离线可跑

./target/release/hub daemon start ~/code ~/notes --history --autostart  # 一步到位
./target/release/hub daemon status                            # 看它在干什么
./target/release/hub search "零拷贝 mmap" --limit 5
./target/release/hub ui --detach                              # 搜索窗常驻，Ctrl+Alt+Space 唤出
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
├── daemon.log       后台日志（5MB 滚动）
├── writer.lock      写租约的锁（0 字节，内核保证进程一死就释放）
└── writer.owner     「现在是谁在写」，只在有人持锁时存在
```

**验证过的真实数字**（Windows，debug 构建）：

| 动作 | 结果 |
|---|---|
| 全量索引 11891 条 PowerShell 历史 | 1.97 s，去重后 6327 篇 |
| 单次查询 | 0.5–4 ms |
| 二次索引（增量） | 0.01 s，全部跳过 |
| 改一个文件 → 可搜到 | 1 秒内（debounce 800ms） |
| 冷启动对账（几千文件） | 数十毫秒 |

**内存实测**（6372 篇真实语料：6324 条历史 + 48 个源文件）：

| 场景 | 工作集 | 私有提交 | 说明 |
|---|---|---|---|
| daemon 空闲 | 19.9 MB | **7.0 MB** | 跑过几次查询后 23.6 / 9.4，之后不再涨 |
| 搜索窗（隐藏等热键） | 18.0 MB | **2.4 MB** | 窗口本身很轻，工作集大半是 exe 与系统 DLL 的映像页 |
| 搜索窗（出结果，走 IPC） | 23.5 MB | 2.9 MB | 它不打开索引，所以私有一直很小 |
| 搜索窗（daemon 没在跑，自己开索引） | 22.3 → 24.4 MB | 3.1 → 3.8 MB | 多出来的就是自己那份 Tantivy reader |
| `hub search`（本地） | 峰值 18–19 MB | 峰值 4.7–5.3 MB | 其中检索只占 3–7 ms，其余全是进程启动 |
| `hub search`（经 daemon） | 峰值 16 MB | 峰值 2.1 MB | 不打开索引，省 2–3 MB |
| `hub stats`（经 daemon / 本地） | 16.1 / 16.8 MB | 2.0 / 4.4 MB | |
| 全量索引 6372 篇（一次性） | 峰值 50.5 MB | 峰值 45.1 MB | Tantivy writer 的预算，不在常驻路径上 |

**常驻合计：私有提交 9.4–13 MB，工作集 42–48 MB。**

两个数差三十多 MB，差在**映射的文件页**上 —— hub.exe 和 user32/gdi32/comctl32
这些系统 DLL 的映像页（任何 Win32 程序都要付，空窗口也有 15 MB），加上 Tantivy
的段文件页（随索引变大，但系统随时能回收）。所以：

- 说"占 45 MB"是**最坏口径**，其中大部分是可以被系统回收的 cache；
- 说"占 10 MB"是**私有口径**，这才是真正挤占别人内存的部分。

两个都不算撒谎，但不说清就是。Linux（PSS）和 macOS（Private Memory）没有实测。

---

## 界面：一个键唤出来的搜索框

```bash
hub ui                     # 前台跑，Ctrl+Alt+Space 唤出
hub ui --detach            # 丢到后台（关掉终端也不影响）
hub ui --hotkey ctrl+alt+j # 默认组合被别的软件占了时换一个
hub ui install             # 登录时自动起来（隐藏窗口，只等热键）
hub ui status              # 自启开了吗、窗口在不在跑
```

窗口是无边框的，不占任务栏也不进 Alt+Tab —— 它是"一个浮层"，不是一个窗口。
再执行一次 `hub ui` 不会开出第二个窗口，而是把已经有的那个叫到前面来。

正因为没有任务栏按钮，**托盘里留了一个图标**（靛蓝放大镜）：左键唤出，右键是
「打开搜索窗 / 快捷键 / 索引状态 / 退出」。没有它，用户想彻底关掉这个进程
就只能去任务管理器。Windows 11 默认把新图标收进折叠区，拖出来一次它就常驻了。

`hub ui install` 把这条命令写进 `HKCU\...\Run`（和 `hub daemon install` 是**两个
不同的注册表值**，互不覆盖），所以登录之后它已经在那儿了，不用先想起来启动它。

| 键 | 行为 |
|---|---|
| ↑ ↓ / PgUp PgDn | 选结果 |
| Enter | 用系统默认程序打开 |
| Ctrl+Enter | 在资源管理器里定位 |
| Ctrl+Shift+C | 复制路径（不抢 `Ctrl+C`，那是复制你打的字） |
| Esc | 收起 |

底部状态栏会写清楚这次查询走的是哪条路：

```
27 条 · 5.1 ms · 实时 · 索引 6371 篇     ← daemon 在跑，走 IPC，新提交立刻可见
13 条 · 8.3 ms · 本地 · 索引 6371 篇     ← daemon 没在跑，界面自己开索引
```

**为什么是 Win32 原生控件而不是 egui**：两个理由，第一个是硬的。

1. **中文输入法**。用中文搜中文，而 IME 在自绘窗口上要自己接 `WM_IME_COMPOSITION`、
   自己画候选窗、自己处理光标位置。Edit 控件这些全是现成的，跟着系统输入法走。
2. **零新依赖**。egui 要新拉一堆 crate；这里只把已经躺在依赖树里的 `windows-sys`
   多开几个 feature。结果列表是自绘的（`LBS_OWNERDRAWFIXED`），其余全是系统行为。

代价是**只有 Windows 版**。Linux/macOS 以后要做，IME 在 GTK/Qt 上是现成的，
不需要现在为它做架构妥协。

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

### 查询走 IPC

daemon 在跑的时候，`hub search` 不再自己打开索引，而是向它发一个请求
（Windows 上走命名管道 `\\.\pipe\hub-<数据目录的哈希>`）。

先说清楚它**不是**为了让搜索更快：`hub search` 的耗时几乎全在进程启动上，
几百篇文档规模下"打开索引 + 检索"只有个位数毫秒，IPC 省掉的就是这几毫秒。

它真正解决的是三件事：

| 问题 | 走 IPC 之后 |
|---|---|
| daemon 在写、CLI 又开一份引擎，两个写者抢同一份目录锁 | 查询只读，不碰写入器 |
| CLI 新开的 reader 要等 `OnCommitWithDelay` 才看到最新提交 | 用 daemon 手里那个 commit 即 reload 的 reader |
| 以后要加 UI，需要一条"问一句就出结果"的通道 | 协议已经就位（搜索窗现在就走它） |

```
$ hub search 零拷贝
命中 3 条，0.9 ms（经 daemon）
```

`hub stats` 现在也走这条路。它原本自己 `Indexer::open`，而那是**可写**打开
（`MetaStore::open` 要跑迁移），于是 daemon 常驻之后每跑一次 stats 就多一个写者。
现在 IPC 线程用一条 `SQLITE_OPEN_READONLY` 连接读同一个库：

```
$ hub stats
存活文档：6372    墓碑：0    倒排中：6372    段：16    （实时，来自 daemon）
```

只读是**数据库层面**的约束而不是自觉 —— 以后谁在这条路径上加了写语句，测试会立刻炸。
顺带的好处是数字跟 daemon 实时一致：新建一个文件，stats 不用等任何东西就能看到。

实测 6372 篇：走 IPC 73–80ms / 15.5MB，走本地 105–113ms / 16.8MB。
30ms 不是重点，重点是不再碰索引目录。

**连不上就静默退回本地**：daemon 没在跑、协议版本对不上（升级后旧进程还在）、
3 秒没响应，三种情况都走原来那条路。用户不该因为后台进程的状态而改变命令用法。
`--local` 可以强制走本地。

协议是 4 字节长度前缀 + 一行制表符分隔的文本（字段里的 `\t` `\n` `\` 做转义），
只有两个动词：`ping` 和 `search`。`ping` 带协议版本号，CLI 靠它判断
"对面这个 daemon 我还认得吗"。

`hub daemon status` 会真的连一次并在状态里写明白：

```
IPC：\\.\pipe\hub-021d15d4524f4531
     应答正常（协议 hub-ipc/1，倒排 66 篇，pid 20020）
```

---

## 三个写者怎么排队：写租约

索引有两份东西要写：`<data>/index`（Tantivy）和 `<data>/meta.sqlite`（SQLite）。
会写的只有三处：

| 写者 | 什么时候 |
|---|---|
| daemon 的 watcher / 对账 | 改了文件之后约 1 秒 |
| `hub index` | 你手动扫一遍 |
| `hub merge` / `hub reset` | 你手动维护 |

**Tantivy 的写入器锁是硬互斥的**：`Index::writer()` 拿不到锁就直接返回错误，
不排队、不重试。所以在加租约之前，"两个写者撞上了"的真实表现是：后到的那个拿到
`Failed to acquire index lock`，**这一批白干**（数据要等下一轮对账才补回来），
而用户看到的是"合并失败"。

之前能跑，是因为写入窗口只有几十毫秒，撞上的概率低。靠概率不算设计 ——
一次 `hub index` 会连续提交几百批，撞上一次就够让人以为工具坏了。

现在的规则很简单：**要写，先拿租约**。

```
<data>/writer.lock    锁本身（内容为空，纯粹用来锁）
<data>/writer.owner   谁在写（pid / 角色 / 开始时刻），只给人看，没有锁语义
```

- 锁是**操作系统级的文件锁**（Windows `LockFileEx`，Unix `flock`）。
  进程崩了、被任务管理器杀了，内核自动释放 —— 不会留下"永远卡住"的残留锁。
- 拿到锁的人把"我是谁"写进 `writer.owner`；等不到的时候，报错就能说清
  "pid 4321 的 index 已持有 8 秒"，而不是干瞪眼。
- 两个文件而不是一个，是因为 `File::lock` 锁的是整个文件，锁着的时候别的进程连读
  都会被拒 —— 那行说明就没法看了。

**两种耐心，同一个机制**：

| 谁 | 等多久 | 为什么 |
|---|---|---|
| `hub index` / `merge` / `reset` | 默认 30 秒（`HUB_WRITE_WAIT_SECS` 可调） | 你明确说了"现在写"，等是对的行为 |
| daemon 的每一批 | **0 秒**（试一下，忙就算了） | 它是后台线程：为等锁而挂住主循环，会让心跳停、事件堆在缓冲区、`daemon stop` 变慢。拿不到就这批路径排回去，下个 tick 再试 |

所以你会看到两种日志/提示，它们都是正常的：

```
$ hub merge
另一个写者正在写索引（pid 9892 的 flush，已持有 0 秒）。合并会等这一批写完再开始，最多等 30 秒。
合并完成：存活 800，墓碑 0，倒排中 800，用时 0.32s
```

```
daemon.log
[INFO] 写租约被占用（等写租约超时（0.0s）：pid 2828 的 merge，已持有 0 秒。⏎ ...），这批稍后重试
[INFO] 变更：检查 120 个路径 → 入库 120，删除 0，跳过 0，失败 0，用时 212ms
```

### 租约边界划在哪儿（这一条最容易被做错）

租约覆盖的是"写倒排 + 提交 + 写水位表"这整段，但**放锁必须晚于写入器彻底停手**。
两个坑都是实测出来的：

1. **`commit()` 返回 ≠ 写完了**。提交之后 Tantivy 的合并线程还在重写段文件、删旧段，
   而这些动作发生在 `commit()` 返回**之后**，目录锁在 drop 时就放掉了。
   结果：另一个进程刚拿到租约开始写，两边的文件操作撞上，报的是
   `Failed to open file for write ... 拒绝访问` —— 一个看上去和并发毫无关系的错误。
   所以 `flush` 用 `commit_and_finish()`：提交 → 等合并线程 join → 才释放写入器。
2. **先提交倒排，再写水位表**。反过来的话，任何一次提交失败都会留下**永久**
   不一致：水位表说"这篇按这个 hash 入过库了"，倒排里却没有，之后每轮对账都被
   增量闸门挡住 —— 这篇文件永远搜不到，而且不会自愈。
   现在的顺序最坏是"倒排有、水位表没有"，下一轮对账重读一遍就补上。

### 它不保护什么（说清楚比藏起来好）

- **`doc_id` 分配**是一条独立的小事务，由 SQLite 自己的事务 + `busy_timeout` 兜住。
  它不参与租约，因为它不可能造成"索引与水位表不一致"，只是发个号。
- **"读文件 → 拿锁写入"之间的时间差**：daemon 可能读到了旧版本才拿到锁。
  这不是丢失：水位表记的是 `(mtime, content_hash)`，下一轮对账发现磁盘上的 mtime
  和水位表不符，会按磁盘重读一遍，差异自我纠正。彻底消除它要"读文件前先拿锁"，
  代价是一次几百毫秒的扫描期间 daemon 完全停摆 —— 不值得。
- **提交失败的重试只有 3 次**（15/30ms 退避）。它覆盖的是 Windows 上"另一个进程
  正在 reload、挡住了重命名"这种毫秒级碰撞；磁盘满、索引损坏这类错误重试没有意义，
  直接报出来。

实测（800 篇语料，daemon 常驻 + 后台不停改文件）：

| 动作 | 结果 |
|---|---|
| 3 次 `hub index` + 3 次 `hub merge` 与 daemon 并发 | 6 次全部 exit 0，daemon 日志 0 条 ERROR |
| daemon 连续撞上别人持锁 12 次 | 12 次全部"这批稍后重试"，最终 920 篇一条不少 |
| `hub reset` 在 daemon 运行时 | 直接拒绝并说明原因（段文件被 mmap，删不掉） |

---

## 架构

```
hub-cli         参数 / IO / 打印，不含检索逻辑也不含采集逻辑
hub-ui          唤起式搜索窗 + 托盘图标（Win32 原生控件，零第三方依赖，仅 Windows）
hub-daemon      常驻进程：watcher + debounce + 对账 + 空闲合并 + 历史轮询 + 自启 + IPC
hub-engine      采集流水线（scan_all / index_paths / remove_paths / reconcile）
hub-store       MetaStore(SQLite) + HubIndex(Tantivy) + Indexer(胶水)
hub-core        查询解析 / 中文分词 / BM25 / tombstone（零依赖）
hub-connectors  files / terminal 采集器（零依赖）
```

**五条不可违背的边界**

1. **SQLite 是事实源，Tantivy 是能重建的缓存。** 索引删了能重建，元数据删了就丢了。
2. **`parse_query` 是语义的唯一入口。** 索引层不许自己解析字符串 —— 第一版就是把原始
   字符串丢给 Tantivy 的 `QueryParser`，结果「上个月」「代码片段」「-排除」一个都没生效，
   而 hub-core 那 30 条测试照样全绿。
3. **分词必须同源。** 索引与查询都走 `hub_word` / `hub_bigram` 两个自定义分词器，
   底层就是 `hub_core::word_tokens` / `bigram_tokens`。索引用 jieba、查询用自研词典，
   切出来的 term 对不上，会变成"能建索引但搜不到"，且不报错。
4. **采集只有一条路径。** `hub index` 和 daemon 都只能通过 `hub-engine` 写数据。
   两条路径各写一遍"怎么遍历、怎么判重"，行为必然分叉 —— 第 2 条就是这么坏的。
5. **同一瞬间只有一个写者。** 任何要写索引或水位表的路径都必须先拿写租约
   （`hub-store::lease`）。Tantivy 的写入器锁是硬互斥的，省掉这一步就等于把
   "两个写者撞上"变成一条随机出现的 `Failed to acquire index lock`。

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

### 本地 IPC

| 原假设 | 真实情况 |
|---|---|
| IPC 是为了让搜索更快 | 大半时间花在进程启动上，"打开索引 + 检索"只有几毫秒。省这个没意义 —— 真正的理由是**单一写者**和**看到同一份提交** |
| IPC 线程自己 `open` 一次索引就行 | **不行**。两套 reader 会各自刷新：主循环 commit 后立刻可见，另一套要等 `OnCommitWithDelay`。必须把同一个 `Arc<HubIndex>` 交过去 |
| 设个标记就能让阻塞在 `ConnectNamedPipe` 的线程退出 | **不能**。得自己连一次，用一条"连上就断"的空连接把它踢醒 |
| 同步 `ReadFile` 能设读超时 | 字节模式管道上不能。要么上 overlapped I/O，要么用 `PeekNamedPipe` 轮询字节数自己算截止时间 |

### 并发写（Windows 上尤其阴）

| 原假设 | 真实情况 |
|---|---|
| Tantivy 的写入器锁会排队，第二个写入器等就行 | **硬互斥**。拿不到直接返回 `LockFailure`，不排队。实测两个写入器并存必有一个失败（`tantivy_writer_lock_is_exclusive` 钉住这条） |
| 两个写者撞上只是"慢一点" | 后到的那个**丢一整批**，用户看到"合并失败"，数据要等下一轮对账才回来 |
| 进程被强杀会留下锁文件，把所有人卡死 | 自己用 `create_new` 造锁文件才会。用操作系统的文件锁（`LockFileEx` / `flock`），进程一死内核就释放 |
| `commit()` 返回就代表"写完了" | **不是**。提交之后合并线程还在重写段文件，而目录锁在 drop 时就放掉了。放锁必须晚于 `wait_merging_threads` |
| 先写水位表、再提交倒排也一样 | 提交失败会留下**永久**不一致：水位表说"入过了"，倒排里没有，增量闸门从此挡住重读 —— 这篇文件永远搜不到。必须反着来 |
| 两个进程同时 `MetaStore::open` 是安全的 | WAL 库的并发打开会撞恢复流程，后到的一方拿到 `SQLITE_READONLY / attempt to write a readonly database`（字面意思是"只读库"，和真实原因无关）。`busy_timeout` 装在最前面 + 6 次退避重试才干净 |
| 后台进程的标准流设成 null 就不会占用父进程的管道 | **不够**。`CreateProcess(bInheritHandles=TRUE)` 会把父进程里所有可继承句柄一起复制过去，于是 `hub daemon start \| tail` 会一直挂着等 EOF。必须先 `SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0)` 再 spawn |

### 文件监听（`ReadDirectoryChangesW`）

| 原假设 | 真实情况 |
|---|---|
| `CloseHandle` 能取消阻塞中的调用 | **不能**。同步模式下必须 `CancelSynchronousIo`，否则线程永远卡住、进程退不掉 |
| 把 `FILE_NOTIFY_INFORMATION` 拷出来再读 `FileName` | **错**。`FileName` 是 `[u16; 1]`，拷贝到栈上后指针指向副本，读到一串垃圾，stat 报 "strings passed to WinAPI cannot contain NULs"。要按固定偏移直接从缓冲区读 |
| 缓冲区 64KB 够用 | 溢出时**返回 0 字节而不是报错**，必须当作"全量对账"信号 |
| mtime 相同就是没变 | mtime 只有**秒**精度，同一秒内的修改会被吃掉。要给"刚改过的文件"加新鲜度保护 |
| 跳过规则拿绝对路径判 | root 在 `%TEMP%` / `%APPDATA%` 下时整棵树都被判成噪声。只能判 root 以下的相对路径 |
| `DETACHED_PROCESS` 就够脱离父进程 | 父进程在 Job Object 里时还会被一起带走，要加 `CREATE_BREAKAWAY_FROM_JOB` |
| 收到 `TaskbarCreated` 就重加托盘图标，加过就跳过 | `added` 只表示"我们加过"，不代表**新的** explorer 认识它。资源管理器一重启，标记还是 true，于是永远跳过重加，图标再也回不来 |
| 托盘图标重加 = 再发一次 `NIM_ADD` | 同一个 hWnd + uID 还没删时系统不覆盖，直接返回失败。必须先 `NIM_DELETE` 再 `NIM_ADD`（删不掉也无所谓，真重启时本来就没了） |
| `TrackPopupMenu` 弹之前不用管前台窗口 | 不先 `SetForegroundWindow` 的话，点空白处菜单不收起，要点两次；弹完还要补一个 `PostMessage(WM_NULL)` |

### 界面（Win32）

| 原假设 | 真实情况 |
|---|---|
| 子类化输入框是正统做法 | **踩坑**。换掉 Edit 的窗口过程后，那段代码要靠父窗口 `GWLP_USERDATA` 拿 `App`，而 Edit 是在 App 指针写进去**之前**创建的 —— 那段时间消息全落到 `DefWindowProc`，表现是"能存文本、但画不出来、也从不发 `EN_CHANGE`"。改成在消息循环里拦 `WM_KEYDOWN`，Edit 的窗口过程一行不动 |
| 监听 `EN_CHANGE` 就够了 | 程序化 `SetWindowTextW` **只发** `EN_UPDATE(0x0400)`，不发 `EN_CHANGE(0x0300)`。只听后者会让脚本注入 / 输入法上屏 / 粘贴全失效 |
| `DT_PATH_ELLIPSIS` 会省略路径中间 | 它只认反斜杠。终端历史的 uri 是 `terminal://C:/Users/...`，一个反斜杠都没有 → 它只把开头换成 `...`，剩下照画，**直接画到窗口外面**。得自己按像素量 |
| `Box::new(app)` + `&mut *app` 交给窗口就行 | 双重释放：`run()` 返回时 drop 一次，`WM_NCDESTROY` 里又 drop 一次。要 `Box::into_raw` 把所有权交给窗口 |
| `PostMessage` 能判断对方还活着 | 只要句柄有效就返回成功，**哪怕对端卡死**。要用 `SendMessageTimeoutW(..., SMTO_ABORTIFHUNG, 800ms)` |
| 脱离标志位抄一份就行 | 抄的那份漏了 `CREATE_BREAKAWAY_FROM_JOB`，表现是"报已启动、一秒后进程没了"。现在只维护一份（`hub_daemon::process`） |
| 截图工具能直接抓窗口 | UI 是 `PER_MONITOR_AWARE_V2`（客户区 950×650），截图工具按 96DPI 虚拟化后 `GetClientRect`/`BitBlt` 只算得到 760×520，抓到的是窗口左上角一块 |

---

## 已知边界

- **界面只有 Windows 版**：Win32 原生控件（原因见上），Linux/macOS 没做
- **托盘图标在 Win11 默认收进折叠区**：系统对新图标一律折叠，用户拖出来一次才常驻。
  程序无法替用户固定（微软没给 API）
- **托盘菜单里的"索引 N 篇"是上次查询留下的值**：不是实时去问索引 —— 右键菜单必须
  立刻弹出，不能让它等一次 IO
- **没有截图 OCR、聊天记录 connector**
- **开机自启只实现了 Windows**（HKCU Run 键）；Linux / macOS 需自己配 systemd unit / launchd
- **非 UTF-8 文件直接跳过**（GBK 会变乱码，宁可漏不要脏）
- **重排窗口 2000**：时间/来源惩罚在 BM25 之后做，超出窗口的文档不参与重排
- **Windows 上索引目录被占用时删不掉**：`hub reset` 需要先 `hub daemon stop`
- **终端历史每 5 秒轮询一次**（不是事件监听）：文件大小和 mtime 都没变时只花几次
  `stat`；敲完命令到能搜到最多差 5 秒。`history_poll_secs=0` 可关掉
- **历史条目常驻内存一份 uri 集合**：约 50 字节/条，10 万条历史约 5MB
- **目录重命名会当成"删 + 增"**：内容没变也要重新分词一遍，正确但有开销
- **IPC 只回答 search / stats**：`hub index`、`hub merge`、`hub repl` 仍直接打开索引。
  前两个按定义就是写者，靠**写租约**和 daemon 排队（同一瞬间只有一个写者，见上），
  不会互相撞锁；`hub repl` 是长会话，只读。
- **写租约是一个数据目录一把**：两个 `--data` 指向同一个磁盘目录时才有意义；
  指向不同目录的两个 daemon 互不相干，也互不保护

## 下一步

```
1. 截图 OCR / 聊天记录 connector
2. Linux / macOS 版界面（IME 在 GTK/Qt 上是现成的，成本比 Windows 低）
3. Linux（PSS）/ macOS（Private Memory）的内存基准 —— 现在只有 Windows 有数
```
