# 踩坑清单

这份清单里的每一条都是**实际遇到过**并修掉的，不是从文档里抄的警告。
写下来的目的只有一个：下次有人（包括我自己）照着"天真想法"做时，能早一点停手。

格式统一为「原假设 → 真实情况」。

---

## Tantivy / rusqlite

| 原假设 | 真实情况 |
|---|---|
| `MmapDirectory::open` 会建目录 | **不会**。目录不存在直接 `DoesNotExist`，`create_dir_all` 得自己写 |
| `IndexWriterBuilder::memory_budget()` | 不存在，入口是 `index.writer(budget)` |
| `IndexWriterOptions` 是 0.27 才有的 | **0.26.1 就有了**（`indexer/index_writer.rs:50`），入口 `writer_with_options` |
| writer 预算可以随便给小 | 每线程下限 15MB，小于它返回 `InvalidArgument` |
| `TopDocs` 可直接当 Collector | 0.26 breaking change：必须 `.order_by_score()` |
| SQLite 支持 u64 | 不支持，`FromSql` 无 u64 实现，用 i64 存取再转 |
| `rusqlite features = ["fts5"]` | 没有这个 feature，只有 `bundled`（fts5 是 SQLite 编译开关） |

## 检索语义

| 原假设 | 真实情况 |
|---|---|
| 索引用 jieba、查询用自研词典 | term 对不上，静默搜不到。必须同源 |
| 给 source 加 `Must` 过滤就行 | **错**。外层有 `Must` 时 `Should` 变可选，同来源文档 0 分混入结果 |
| "十" 可以跳过 | `cn_digit('十')` 是 `None`，收集循环会在"十"处断开：`十二个月前`→2 个月 |
| 改文件时直接 `add_document` 就是"更新" | **错**。Tantivy 没有 upsert，旧文档变成僵尸：搜索结果按 doc_id 去重把它藏起来，`stats` 里"倒排中"才会露馅（实测 15 vs 存活 6）。写入前必须先 `delete_term(uri)` |
| 历史条目的 uri 用 `mtime + 命令` 算 | 纯文本历史（PSReadLine）没有时间戳，mtime 一追加就变 → **整份历史的 uri 全变**，每轮重写几千条。无时间戳时只能按命令算 uri |

## 本地 IPC

| 原假设 | 真实情况 |
|---|---|
| IPC 是为了让搜索更快 | 大半时间花在进程启动上，"打开索引 + 检索"只有几毫秒。省这个没意义 —— 真正的理由是**单一写者**和**看到同一份提交** |
| IPC 线程自己 `open` 一次索引就行 | **不行**。两套 reader 会各自刷新：主循环 commit 后立刻可见，另一套要等 `OnCommitWithDelay`。必须把同一个 `Arc<HubIndex>` 交过去 |
| 设个标记就能让阻塞在 `ConnectNamedPipe` 的线程退出 | **不能**。得自己连一次，用一条"连上就断"的空连接把它踢醒 |
| 同步 `ReadFile` 能设读超时 | 字节模式管道上不能。要么上 overlapped I/O，要么用 `PeekNamedPipe` 轮询字节数自己算截止时间 |

## 并发写（Windows 上尤其阴）

| 原假设 | 真实情况 |
|---|---|
| Tantivy 的写入器锁会排队，第二个写入器等就行 | **硬互斥**。拿不到直接返回 `LockFailure`，不排队。实测两个写入器并存必有一个失败（`tantivy_writer_lock_is_exclusive` 钉住这条） |
| 两个写者撞上只是"慢一点" | 后到的那个**丢一整批**，用户看到"合并失败"，数据要等下一轮对账才回来 |
| 进程被强杀会留下锁文件，把所有人卡死 | 自己用 `create_new` 造锁文件才会。用操作系统的文件锁（`LockFileEx` / `flock`），进程一死内核就释放 |
| `commit()` 返回就代表"写完了" | **不是**。提交之后合并线程还在重写段文件，而目录锁在 drop 时就放掉了。放锁必须晚于 `wait_merging_threads` |
| 先写水位表、再提交倒排也一样 | 提交失败会留下**永久**不一致：水位表说"入过了"，倒排里没有，增量闸门从此挡住重读 —— 这篇文件永远搜不到。必须反着来 |
| 两个进程同时 `MetaStore::open` 是安全的 | WAL 库的并发打开会撞恢复流程，后到的一方拿到 `SQLITE_READONLY / attempt to write a readonly database`（字面意思是"只读库"，和真实原因无关）。`busy_timeout` 装在最前面 + 6 次退避重试才干净 |
| 后台进程的标准流设成 null 就不会占用父进程的管道 | **不够**。`CreateProcess(bInheritHandles=TRUE)` 会把父进程里所有可继承句柄一起复制过去，于是 `hub daemon start \| tail` 会一直挂着等 EOF。必须先 `SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0)` 再 spawn |

## 文件监听（`ReadDirectoryChangesW`）

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

## 界面（Win32）

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

更完整的推理过程（为什么这么修、修完用什么钉住）见 [`软件设计文档.md`](../软件设计文档.md) 的缺陷表（#1–#43）。
