# hub — 本地离线多源个人检索中枢

纯 Rust。把文件系统、终端历史等散落各处的数据统一采集、归一化、建索引，
用一句话搜到任何东西。**全部本地、离线、不上传。**

```
hub index ~/code ~/notes --history     # 建索引（增量，内容没变的直接跳过）
hub search 上个月写的那个rust内存相关的代码片段
hub stats
```

---

## 现在能跑什么

```bash
cargo build --release          # 首次约 2–5 分钟（Tantivy + bundled SQLite）
cargo test                     # 61 条测试，全部离线可跑

# 建索引
./target/release/hub-cli index <目录> [--history] [--max-mb 2]
# 检索
./target/release/hub-cli search "零拷贝 mmap" --limit 5
# 交互
./target/release/hub-cli
```

数据目录默认 `~/.hub`（可用 `HUB_HOME` 或 `--data <dir>` 覆盖）：

```
~/.hub/
├── meta.sqlite    水位表：content_hash / mtime / tombstone / doc_id（事实源）
└── index/         Tantivy 倒排（可重建，删了不心疼）
```

**验证过的真实数字**（Windows，debug 构建，1.2 万条 PowerShell 历史）：

| 动作 | 结果 |
|---|---|
| 全量索引 11891 条候选 | 1.97 s，去重后 6327 篇 |
| 单次查询 | 0.5–4 ms |
| 二次索引（增量） | 0.01 s，全部跳过 |

---

## 架构

```
hub-cli         参数 / IO / 打印，不含任何检索逻辑
hub-store       MetaStore(SQLite) + HubIndex(Tantivy) + Indexer(胶水)
hub-core        查询解析 / 中文分词 / BM25 / tombstone（零依赖）
hub-connectors  files / terminal 采集器（零依赖）
```

**三条不可违背的边界**

1. **SQLite 是事实源，Tantivy 是能重建的缓存。** 索引删了能重建，元数据删了就丢了。
2. **`parse_query` 是语义的唯一入口。** 索引层不许自己解析字符串 —— 第一版就是把原始
   字符串丢给 Tantivy 的 `QueryParser`，结果「上个月」「代码片段」「-排除」一个都没生效，
   而 hub-core 那 30 条测试照样全绿。
3. **分词必须同源。** 索引与查询都走 `hub_word` / `hub_bigram` 两个自定义分词器，
   底层就是 `hub_core::word_tokens` / `bigram_tokens`。索引用 jieba、查询用自研词典，
   切出来的 term 对不上，会变成"能建索引但搜不到"，且不报错。

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

## 纠错清单（照着旧设计做会卡住的地方）

| 原假设 | 真实情况 |
|---|---|
| `MmapDirectory::open` 会建目录 | **不会**。目录不存在直接 `DoesNotExist`，`create_dir_all` 得自己写 |
| `IndexWriterBuilder::memory_budget()` | 不存在，入口是 `index.writer(budget)` |
| `IndexWriterOptions` 是 0.27 才有的 | **0.26.1 就有了**（`indexer/index_writer.rs:50`），入口 `writer_with_options` |
| writer 预算可以随便给小 | 每线程下限 15MB，小于它返回 `InvalidArgument` |
| `TopDocs` 可直接当 Collector | 0.26 breaking change：必须 `.order_by_score()` |
| SQLite 支持 u64 | 不支持，`FromSql` 无 u64 实现，用 i64 存取再转 |
| `rusqlite features = ["fts5"]` | 没有这个 feature，只有 `bundled`（fts5 是 SQLite 编译开关） |
| 索引用 jieba、查询用自研词典 | term 对不上，静默搜不到。必须同源（见上） |
| 给 source 加 `Must` 过滤就行 | **错**。外层有 `Must` 时 `Should` 变可选，同来源文档 0 分混入结果 |

---

## 已知边界（这一版没做的）

- **没有 watcher**：改了文件要重跑 `hub index`（增量很快，全量 12k 也就 2 秒）
- **没有 daemon / IPC / UI**：只有 CLI
- **没有截图 OCR、聊天记录 connector**
- **非 UTF-8 文件直接跳过**（GBK 会变乱码，宁可漏不要脏）
- **重排窗口 2000**：时间/来源惩罚在 BM25 之后做，超出窗口的文档不参与重排
- **Windows 上索引目录被占用时删不掉**：`hub reset` 需要先退出所有 hub 进程

## 下一步

```
1. watcher + debounce + 周期 reconcile   ← 目前靠手动重跑
2. hub-daemon + IPC（Unix socket / named pipe）
3. egui UI + global-hotkey
4. 截图 OCR / 聊天记录 connector
5. 真机内存基准（上线前必须做）
```
