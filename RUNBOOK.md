# 怎么运行这个项目

从零到"能搜到自己的文件"的完整步骤。全部命令都在 Windows 上验证过
（Windows 11 + Rust 1.94 + MSVC 工具链）。

- 只想跑起来：看 §1 → §3 → §4，十分钟够了
- 想知道每一步在验证什么：看 §7 的清单，一条条对
- 出问题了：直接跳 §10 排错表

---

## 0. 一句话说清它是啥

一个纯本地的检索工具：把散落在各个目录里的文件、终端历史采集起来建索引，
然后用一句中文搜到它。**不起服务、不上传、不联网**，所有数据都在 `~/.hub` 里。

平时它是个常驻后台进程（daemon）：你在编辑器里保存文件，约 1 秒后就能搜到。
想搜的时候按 `Ctrl+Alt+Space`，弹出搜索框，打字，回车就打开选中项。

---

## 1. 环境准备

| 需要 | 版本 | 怎么确认 |
|---|---|---|
| Windows | 10 / 11（界面和开机自启只做了 Windows） | — |
| Rust | **1.89 或更高**（写租约用到 `std::fs::File::lock`） | `rustc --version` |
| MSVC 构建工具 | VS 2019/2022 的「使用 C++ 的桌面开发」 | `cargo build` 不报 link.exe 错 |
| 磁盘 | 编译产物约 1.5 GB；运行时索引约原文的 5–10% | — |

没装 Rust 的话：

```bash
# 装 rustup（会自动带上 cargo）
winget install Rustlang.Rustup
# 或者从 https://rustup.rs 下 rustup-init.exe
rustup default stable
rustc --version    # 应该 >= 1.89
```

> 需要链接器（`link.exe`）。装 VS Build Tools 时勾上「使用 C++ 的桌面开发」，
> 里面包含 Windows SDK 与 MSVC。**不需要**装完整 Visual Studio。

整个项目**离线可跑**：依赖都在 `Cargo.lock` 里锁死了版本，
`cargo build` / `cargo test` 不需要任何网络服务（首次编译要拉 crate，
如果机器完全不能上网，请先把 `~/.cargo/registry` 拷过来）。

---

## 2. 构建与自检

```bash
git clone <这个仓库> && cd hub

cargo build --release      # 首次 2–5 分钟（Tantivy + bundled SQLite）
cargo test                 # 142 条测试，全部离线，约 20 秒
```

- 产物：`target/release/hub.exe`（**就一个可执行文件**，没有安装包、没有服务注册）
- 想让命令短一点，把它的目录加进 `PATH`：

```powershell
# PowerShell（当前用户，永久生效）
[Environment]::SetEnvironmentVariable(
  "PATH", $env:PATH + ";C:\edge\hub\target\release", "User")
```

下面全部示例用 `hub`，等价于 `./target/release/hub`（debug 构建同理，把路径换成
`target/debug/hub.exe`）。

> 开发时用 `cargo build`（debug）更快；**日常用 release**：
> 索引 6000 篇的耗时差 5–10 倍。

---

## 3. 第一次运行（三步）

```bash
# 1) 先扫一遍，把已有文件进索引
hub index C:\Users\你\code C:\Users\你\notes --history

# 2) 起后台进程，以后不用再手动 index
hub daemon start C:\Users\你\code C:\Users\你\notes --history --autostart

# 3) 搜
hub search 上个月写的那个内存泄漏的笔记
```

第 1 步的输出长这样（真实数字，800 篇）：

```
完成：扫描 800 条，入索引 800 条，跳过 0 条，出错 0 次，用时 0.66s
数据目录：C:\Users\你\.hub（用 hub stats 查看规模）
```

第 2 步会打印：

```
守护进程已启动（pid 12024，后台运行）
监听目录：
  C:\Users\你\code
  C:\Users\你\notes

之后改了文件会自动进索引，不需要再手动 index。
日志：C:\Users\你\.hub\daemon.log
查看状态：hub daemon status    停止：hub daemon stop
```

`--autostart` = 写一条 HKCU 的 `Run` 项，登录 Windows 时自动拉起。不想现在就装，
可以事后单独 `hub daemon install` / `hub daemon uninstall`。

> **目录会记进配置**：第二次直接 `hub daemon start` 就行，不用再敲目录。
> 想换目录就再带一次参数，会覆盖。

---

## 4. 日常怎么用

```bash
hub search 零拷贝           # 搜（daemon 在跑时走 IPC，省掉打开索引）
hub "上个月 memcpy 的那段代码" --limit 5
hub stats                   # 看规模：存活 / 墓碑 / 倒排 / 段数
hub daemon status           # 看它在干什么、IPC 通不通、累计写了多少
hub daemon log -n 30        # 后台日志
hub                         # 交互模式（只有检索）
```

界面（可选）：

```bash
hub ui --detach             # 常驻，Ctrl+Alt+Space 唤出
hub ui install              # 登录时自动起来（隐藏窗口，只等热键）
hub ui status               # 看自启状态 + 窗口在不在
hub ui uninstall
```

- `Esc` 收起，`Enter` 打开选中项，`↑`/`↓` 选
- 托盘里有图标：左键唤出搜索框，右键「打开搜索窗 / 退出」
- Win11 默认把新图标收进折叠区，拖出来一次才会常驻（系统限制，程序改不了）

**改了文件不用敲任何命令**：watcher 会在 debounce（默认 800ms）之后批量入库，
每 10 分钟还会对账一次，睡眠唤醒、daemon 没在跑的那段时间的变更也能补上。

---

## 5. 命令一览

| 命令 | 作用 | 备注 |
|---|---|---|
| `hub index <目录>... [--history] [--max-mb N] [--reset]` | 手动全量扫描 | `--max-mb` 单文件上限，默认 2 |
| `hub search <查询> [--limit N] [--local]` | 检索 | `--local` 强制不走 daemon |
| `hub stats` | 规模与来源分布 | daemon 在跑时数字是实时的 |
| `hub merge` | 回收已删文档占用的倒排空间 | 和 daemon 同时跑会排队，不会撞锁 |
| `hub reset --yes` | 清空索引与水位表 | **要求 daemon 已停**；原始文件不受影响 |
| `hub daemon start [目录]... [--history] [--fg] [--autostart]` | 起后台进程 | 目录会记进配置 |
| `hub daemon stop [--force]` | 优雅停止 | 不影响开机自启 |
| `hub daemon status` / `log [-n N]` | 状态 / 日志 | `log` 默认 50 行 |
| `hub daemon install` / `uninstall` | 开机自启 | 写/删 HKCU Run 项 `HubDaemon` |
| `hub ui [--hotkey ctrl+alt+space] [--hidden] [--detach]` | 搜索窗 | 已在跑时只唤出，不开第二个 |
| `hub ui install` / `uninstall` / `status` | 界面自启 | 写/删 HKCU Run 项 `HubUi` |
| `--data <目录>` | 换数据目录 | 也可用环境变量 `HUB_HOME` |

`daemon start` 的调优参数：`--debounce 毫秒`、`--reconcile 秒`、`--merge 秒`。
它们会写进 `daemon.conf`，下次不写也生效。

---

## 6. 数据目录里都是什么

默认 `C:\Users\<你>\.hub`（`HUB_HOME` 或 `--data` 可改）：

| 文件 | 是什么 | 删了会怎样 |
|---|---|---|
| `meta.sqlite` | 水位表：content_hash / mtime / 删除标记 / doc_id —— **事实源** | 只能重建（重新扫一遍） |
| `index/` | Tantivy 倒排，**可重建的缓存** | 下次 `hub index` 重建 |
| `daemon.conf` | 监听哪些目录、各种间隔 | 下次 `daemon start` 重新生成 |
| `daemon.state` | pid / 心跳 / 段数 / 累计统计 | 无害，重启即重建 |
| `daemon.log` | 后台日志，5MB 滚动 | 无害 |
| `writer.lock` | 写租约的锁文件（0 字节） | 无害，自动重建 |
| `writer.owner` | "现在是谁在写"，只在有人持锁时存在 | 无害 |

环境变量：

| 变量 | 作用 | 默认 |
|---|---|---|
| `HUB_HOME` | 数据目录 | `%USERPROFILE%\.hub` |
| `HUB_WRITE_WAIT_SECS` | 用户命令等写租约的上限（秒） | 30 |
| `HUB_UI_TRACE` | 设成任意值 → 搜索窗把诊断打到 stderr | 关 |

---

## 7. 验证清单（跑完这套就确认整条链路是通的）

```bash
# ① 二进制能跑
hub --help

# ② 索引有东西
hub stats
#   期望：存活文档：N（N > 0）    墓碑：0    倒排中：N    段：少量

# ③ 搜索能命中
hub search 关键词 --limit 3
#   期望：命中 x 条，几毫秒；每条带来源、时间、路径

# ④ 改了文件能自动进索引（这一步验证 daemon 真的在干活）
echo "自动更新验证 $(date +%s)" > 某个被监听目录/测试.txt
sleep 2
hub search 自动更新验证
#   期望：刚写的那条在最前面；hub daemon log 里有一行「变更：检查 1 个路径 → 入库 1」

# ⑤ daemon 状态健康
hub daemon status
#   期望：守护进程：运行中；IPC 应答正常（协议 hub-ipc/1）

# ⑥ 写者排队（可选，但能证明这套机制在你机器上也生效）
hub merge
#   期望：合并完成……；若 daemon 恰好在写，会先打印一行「另一个写者正在写索引……」

# ⑦ 界面
hub ui --detach
#   期望：Ctrl+Alt+Space 能唤出搜索框；托盘有图标；输入能出结果
```

全部通过 = 采集、分词、持久化、增量、IPC、界面六层都在工作。

---

## 8. 几个"原来如此"

- **`hub index` 和 daemon 同时跑是安全的**。两者都是写者，靠写租约排队：
  谁先拿到谁写，另一个等（用户命令最多等 30 秒，daemon 干脆不等 —— 试一下，
  忙就把这批路径排回去，下个 tick 再试）。不会再出现
  `Failed to acquire index lock` 那种随机失败。细节见 README「三个写者怎么排队」。
- **`hub reset` 要求 daemon 先停**。daemon 用 mmap 拿着段文件，Windows 上这些文件
  删不掉；硬来会在删到一半时失败。命令会直接拦下来并告诉你先 `hub daemon stop`。
- **`daemon stop` 不影响开机自启**。"下次开机还要自动起来"正是自启的意思；
  真要取消用 `hub daemon uninstall`。
- **索引能删，元数据不能删**。`index/` 是缓存，删了重扫即可；`meta.sqlite` 是事实源，
  删了等于全量重来（`hub reset` 就是两个一起清）。

---

## 9. 停、卸、清

```bash
hub daemon stop              # 停后台进程（数据保留）
hub daemon uninstall         # 取消开机自启
hub ui uninstall             # 取消界面自启
hub reset --yes              # 清空索引与水位表（先 stop daemon）
```

彻底删干净：

1. `hub daemon stop` + `hub daemon uninstall` + `hub ui uninstall`
2. 删数据目录 `%USERPROFILE%\.hub`
3. 删 `target/` 或整个仓库目录

自启项在注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的
`HubDaemon` / `HubUi` 两个值，两个命令就是删它们；手工删也行。

---

## 10. 排错

| 症状 | 大概率原因 | 怎么办 |
|---|---|---|
| `cargo build` 报 `link.exe not found` | 没装 MSVC 工具链 | 装 VS Build Tools 的「使用 C++ 的桌面开发」 |
| 编译报需要更高版本的 rustc | 工具链低于 1.89 | `rustup update stable` |
| 搜不到刚改的内容 | daemon 没在跑 / 目录没被监听 | `hub daemon status`；`hub daemon log -n 30` 看有没有「变更：」行 |
| 搜不到任何东西 | 索引是空的 | `hub stats` 看存活数；`hub index <目录>` |
| `hub daemon start` 说已在运行 | 上次的进程还活着 | `hub daemon status`，要重启就先 `hub daemon stop` |
| daemon 起了又立刻没了 | 启动失败（目录权限 / 被 Job 带走） | `hub daemon run` 前台跑一次，错误直接打屏 |
| 提示「等写租约超时」 | 另一个写者在写（多半是 `hub index` 大扫描） | 等它扫完；或把 `HUB_WRITE_WAIT_SECS` 调大 |
| `hub reset` 被拒绝 | daemon 还在跑 | `hub daemon stop` 再 reset |
| 删除索引时报「拒绝访问」 | 有进程 mmap 着段文件 | 同上：先停 daemon / 关掉搜索窗 |
| 热键没反应 | 被别的软件占了（很多截图工具用 Ctrl+Alt+Space） | `hub ui --hotkey ctrl+alt+j`；先 `hub ui` 关掉旧的再起 |
| 托盘图标不见了 | Win11 折叠区 / Explorer 重启 | 点任务栏的 `^` 展开；Explorer 重启后图标会自己回来 |
| 搜到乱码或漏文件 | 非 UTF-8 文件会被跳过（GBK 会变乱码，宁可漏不要脏） | 转成 UTF-8 再索引 |
| 中文分词切得很碎 | 没有词表时走 bigram 兜底，召回高噪声也多 | 正常行为；用短语 `"精确短语"` 收窄 |

调试时最有用的两条：

```bash
hub daemon run          # 前台运行，日志直接打屏（Ctrl+C 退出）
hub daemon log -n 100   # 后台运行时的日志
```

日志里出现这些字样都是**正常**的，不是错误：

- `写租约被占用（……），这批稍后重试` —— daemon 让路给一次手动命令
- `另一个写者正在写索引（……）` —— 你的命令在等 daemon 写完这一批
- `段合并完成：N 个段` —— 空闲时回收空间

---

## 11. 开发者：改代码时怎么跑

```bash
cargo build                 # debug，快
cargo test                  # 142 条，全离线
cargo test -p hub-store     # 只跑持久化层（41 条）
cargo test --test lease     # 只跑并发/写租约那组
cargo build --release       # 交付用
```

目录结构：

```
crates/
  hub-core          查询解析 / 分词 / BM25（零依赖）
  hub-store         MetaStore(SQLite) + HubIndex(Tantivy) + Indexer + lease(写租约)
  hub-connectors    files / terminal 采集器
  hub-engine        采集流水线（唯一写入路径）
  hub-daemon        watcher + debounce + 对账 + 空闲合并 + IPC + 自启
  hub-ui            Win32 唤出式搜索窗 + 托盘（仅 Windows）
  hub-cli           参数 / IO / 打印
```

几条改代码时别踩的：

- 写索引 / 写水位表的新代码，必须在 `hub-store::lease` 的临界区里
  （`Indexer::flush` / `merge` / `reset` 已经包好了，别绕开它们直接拿写入器）
- 想看两个写者撞上会怎样：`cargo test --test lease`，里面有一条测试专门钉住
  "Tantivy 的写入器锁是硬互斥的"这个前提
- 跑完别忘了 `hub stats` 对一眼：**存活数应等于倒排文档数**，
  差得多说明墓碑没合并（`hub merge`）

---

## 12. 一页速查

```bash
# 装好之后每天就这三下
hub daemon status
hub search 想找的东西
hub ui --detach          # 想用热键就起一次，之后它常驻

# 出问题三件套
hub daemon status
hub daemon log -n 50
hub stats
```
