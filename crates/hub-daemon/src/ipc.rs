//! 本地 IPC —— 让 `hub search` 直接问常驻进程，而不是自己再开一次索引。
//!
//! ## 它解决的不是"性能"
//!
//! 实测（debug 构建，300 篇文档）：`hub search` 整个进程 280ms，其中 250ms 是
//! 进程启动与加载动态库，真正花在"打开索引 + 检索"上的只有 30ms。所以 IPC
//! 省不出多少时间，**它解决的是三件别的事**：
//!
//! 1. **单一写者**。daemon 在跑的时候，CLI 再自己 `Indexer::open` 就是两个进程
//!    抢同一份 Tantivy 目录锁和同一个 SQLite 库。读没事，写会互相等。
//!    有了 IPC，"问一下"就能拿到答案，不必把整个引擎再开一遍。
//! 2. **答案与 daemon 一致**。daemon 的 reader 是随 commit 实时 reload 的，
//!    CLI 那边新开的 reader 要等 `OnCommitWithDelay`。走 IPC 拿到的永远是
//!    daemon 眼下的那一份。
//! 3. **给 UI 留通道**。egui 窗口以后要的是"随手一问就出结果"，
//!    而不是每次都重新加载索引。
//!
//! ## 传输
//!
//! | 平台 | 端点 |
//! |---|---|
//! | Windows | 命名管道 `\\.\pipe\hub-<数据目录路径的哈希>` |
//! | Unix | `<数据目录>/hub.sock` |
//!
//! 名字由**数据目录**决定，不是全局单例：同一台机器上开两个数据目录，
//! 两个 daemon 各用各的端点，不会串台。
//!
//! 用命名管道而不是回环 TCP，是因为前者由内核按创建者的默认 DACL 授权 ——
//! 同一用户可连，别的用户连不上，也不会在 `netstat` 里多出一个能扫到的端口。
//!
//! ## 协议
//!
//! 一帧 = 4 字节小端长度 + UTF-8 载荷（上限 8MB，防止畸形长度前缀把对端
//! 拖进一次巨额分配）。载荷是单行、字段用 `\t` 分隔，字段里的
//! `\` `\t` `\n` `\r` 做转义 —— 标题里带制表符和换行是常见的。
//!
//! | 请求 | 响应 |
//! |---|---|
//! | `ping` | `pong\t<pid>\t<hub-ipc/1>\t<启动时刻>\t<倒排文档数>` |
//! | `search\t<条数>\t<查询>` | `ok\t<命中数>\t<倒排文档数>` + 每行一条命中 |
//! | 其它 | `err\t<原因>` |
//!
//! 每条命中是 6 个字段：`doc_id` `score` `mtime` `source` `uri` `title`。
//!
//! `ping` 里带协议版本是刻意的：升级之后旧 daemon 可能还在跑，CLI 发现版本
//! 对不上就退回本地打开索引 —— 宁可慢一点，也不能让 `hub search` 给出
//! 一个"看起来正常、实际是旧格式"的答案。

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use hub_core::SourceId;
use hub_store::index::Hit;
use hub_store::HubIndex;

use crate::log::Log;

/// 协议版本。改字段布局就必须改它。
pub const PROTO: &str = "hub-ipc/1";

/// 单帧上限。搜索请求是一行文本，命中最多几百行，8MB 绰绰有余。
const MAX_FRAME: usize = 8 << 20;

/// 客户端默认等多久。daemon 正常工作时的耗时是个位数毫秒，
/// 3 秒还没回应基本可以断定它卡住了，早点退回本地比干等着强。
pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 服务端单条连接最多占多久。防止某个客户端连上来不发数据，
/// 把唯一的一条服务线程钉死，后面所有查询都排队。
const CONN_IDLE: std::time::Duration = std::time::Duration::from_secs(10);

// ============================================================
// 端点名字
// ============================================================

/// 由数据目录算出端点。CLI 与 daemon 必须得到同一个结果。
///
/// 用 `canonicalize` 而不是原样拼接：`hub search --data ./data` 和
/// `hub daemon start --data C:/edge/hub/data` 指的是同一个目录，
/// 必须映射到同一个端点，否则 daemon 在跑、CLI 却说"没在跑"。
pub fn endpoint(data: &Path) -> String {
    let key = std::fs::canonicalize(data)
        .or_else(|_| std::path::absolute(data))
        .unwrap_or_else(|_| data.to_path_buf());

    #[cfg(windows)]
    {
        let h = hub_store::indexer::hash_bytes(key.to_string_lossy().as_bytes());
        format!(r"\\.\pipe\hub-{h:016x}")
    }
    #[cfg(not(windows))]
    {
        key.join("hub.sock").to_string_lossy().into_owned()
    }
}

// ============================================================
// 帧
// ============================================================

pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("帧过大：{} 字节", payload.len()),
        ));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut hdr = [0u8; 4];
    r.read_exact(&mut hdr)?;
    let n = u32::from_le_bytes(hdr) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("对端声明的帧长度不合理：{n}"),
        ));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// ============================================================
// 转义
// ============================================================

/// 字段里的分隔符与控制字符转义。`\` 必须第一个处理，否则会二次转义。
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// `esc` 的逆运算。遇到不认识的转义序列原样保留（包括那个反斜杠），
/// 这样即使以后加了新转义，旧客户端也不会把内容吃掉。
pub fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

// ============================================================
// 消息
// ============================================================

pub fn encode_ping() -> String {
    "ping".to_string()
}

pub fn encode_search(k: usize, query: &str) -> String {
    format!("search\t{k}\t{}", esc(query))
}

/// 搜索结果。`num_docs` 是倒排里的文档总数 —— CLI 用它区分
/// "索引是空的，先去 index" 和 "有东西，但这个词没命中"。
#[derive(Debug, Clone)]
pub struct SearchReply {
    pub hits: Vec<Hit>,
    pub num_docs: usize,
}

pub fn encode_search_reply(hits: &[Hit], num_docs: usize) -> String {
    let mut s = String::with_capacity(64 + hits.len() * 96);
    s.push_str(&format!("ok\t{}\t{num_docs}\n", hits.len()));
    for h in hits {
        // score 固定 6 位小数：f32 的默认 Display 有时会打出 0.0049999999，
        // 解析回去值是对的，但日志里难看
        s.push_str(&format!(
            "{}\t{:.6}\t{}\t{}\t{}\t{}\n",
            h.doc_id,
            h.score,
            h.mtime,
            h.source.as_str(),
            esc(&h.uri),
            esc(&h.title)
        ));
    }
    s
}

pub fn parse_search_reply(payload: &str) -> Result<SearchReply, String> {
    let mut lines = payload.lines();
    let head = lines.next().ok_or_else(|| "响应为空".to_string())?;
    let f: Vec<&str> = head.split('\t').collect();

    match f.first().copied() {
        Some("err") => {
            return Err(unesc(f.get(1).copied().unwrap_or("对端没有给出原因")));
        }
        Some("ok") => {}
        Some(other) => return Err(format!("不认识的开头 `{other}`（协议版本不一致？）")),
        None => return Err("响应为空".into()),
    }

    let n: usize = f
        .get(1)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| "响应里缺少命中数".to_string())?;
    let num_docs: usize = f.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);

    let mut hits = Vec::with_capacity(n.min(512));
    for line in lines.take(n) {
        let p: Vec<&str> = line.split('\t').collect();
        if p.len() < 6 {
            return Err(format!("命中行字段不足（{} 个）", p.len()));
        }
        hits.push(Hit {
            doc_id: p[0].parse().map_err(|_| "doc_id 不是整数".to_string())?,
            score: p[1].parse().map_err(|_| "score 不是数字".to_string())?,
            mtime: p[2].parse().map_err(|_| "mtime 不是整数".to_string())?,
            source: SourceId::parse(p[3]).unwrap_or(SourceId::Files),
            uri: unesc(p[4]),
            title: unesc(p[5]),
        });
    }
    if hits.len() != n {
        return Err(format!("响应被截断：声明 {n} 条，实际 {}", hits.len()));
    }
    Ok(SearchReply { hits, num_docs })
}

/// `ping` 的回应
#[derive(Debug, Clone)]
pub struct Pong {
    pub pid: u32,
    pub proto: String,
    pub started_at: i64,
    pub num_docs: usize,
}

pub fn parse_pong(payload: &str) -> Result<Pong, String> {
    let f: Vec<&str> = payload.trim_end().split('\t').collect();
    if f.first().copied() != Some("pong") {
        return Err(format!("不是 pong：{payload}"));
    }
    Ok(Pong {
        pid: f.get(1).and_then(|v| v.parse().ok()).unwrap_or(0),
        proto: f.get(2).copied().unwrap_or("").to_string(),
        started_at: f.get(3).and_then(|v| v.parse().ok()).unwrap_or(0),
        num_docs: f.get(4).and_then(|v| v.parse().ok()).unwrap_or(0),
    })
}

// ============================================================
// 服务端
// ============================================================

pub struct Server {
    endpoint: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// 起一条后台线程守着端点。返回时端点已经可连 —— 监听器在启动线程之前
    /// 就建好了，否则调用方"看到启动成功"和"真的能连上"之间有个空窗。
    ///
    /// `started_at` 只用来回答 `ping`：CLI 靠它算 uptime。
    pub fn start(
        data: &Path,
        index: Arc<HubIndex>,
        started_at: i64,
        log: Log,
    ) -> io::Result<Self> {
        let endpoint = endpoint(data);
        let mut listener = platform::Listen::bind(&endpoint)?;
        let stop = Arc::new(AtomicBool::new(false));

        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("hub-ipc".into())
            .spawn(move || {
                let mut log = log;
                // accept 的错误不能把线程打死，但也不能无限刷日志：
                // 连续失败时退避，成功一次就归零
                let mut backoff = 0u32;
                while !flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok(conn) => {
                            backoff = 0;
                            serve(conn, &index, started_at);
                        }
                        Err(e) => {
                            if flag.load(Ordering::Relaxed) {
                                break;
                            }
                            if backoff < 3 {
                                log.warn(format!("IPC 接受连接失败：{e}"));
                            }
                            backoff = backoff.saturating_add(1);
                            std::thread::sleep(std::time::Duration::from_millis(
                                50 * (backoff as u64),
                            ));
                        }
                    }
                }
            })?;

        Ok(Self {
            endpoint,
            stop,
            thread: Some(thread),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// 停止并等线程退出。不用就 drop 也行（进程退出时线程自然消失），
    /// 但 `daemon run` 是前台进程，能干净收尾就别留给操作系统。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // 线程此刻多半阻塞在 ConnectNamedPipe。设标记不会唤醒它，
        // 只能自己连一次，让它带着一条"发了就断"的空连接返回。
        // 连不上也没关系（比如另一条连接正占着），线程会在
        // CONN_IDLE 之后自己醒。
        let _ = platform::connect(&self.endpoint, std::time::Duration::from_millis(500));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // Unix 的 socket 文件不会随监听器消失，得自己删；
        // Windows 的管道对象随最后一个句柄关闭就没了
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop();
        }
    }
}

fn serve(mut conn: platform::Conn, index: &HubIndex, started_at: i64) {
    // 一条连接可以问多次：客户端问完就关，但服务端这里照样循环，
    // 省得为"一次连接一次问答"额外写状态机
    while let Ok(payload) = read_frame(&mut conn) {
        let reply = handle(&payload, index, started_at);
        if write_frame(&mut conn, reply.as_bytes()).is_err() {
            break;
        }
    }
}

fn handle(payload: &[u8], index: &HubIndex, started_at: i64) -> String {
    let req = match std::str::from_utf8(payload) {
        Ok(s) => s,
        Err(_) => return "err\t请求不是 UTF-8".to_string(),
    };
    let mut f = req.split('\t');

    match f.next().unwrap_or("") {
        "ping" => format!(
            "pong\t{}\t{}\t{}\t{}",
            std::process::id(),
            PROTO,
            started_at,
            index.num_docs()
        ),
        "search" => {
            let k: usize = match f.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) => v.clamp(1, 200),
                None => return "err\tsearch 缺少条数".to_string(),
            };
            // 查询本身可能含被转义过的 \t，所以剩下的字段要拼回去再解转义
            let raw = f.collect::<Vec<_>>().join("\t");
            let q = unesc(&raw);
            if q.trim().is_empty() {
                return "err\t查询为空".to_string();
            }
            match index.search_text(&q, k, crate::now_secs()) {
                Ok(hits) => encode_search_reply(&hits, index.num_docs()),
                Err(e) => format!("err\t检索失败：{e}"),
            }
        }
        "" => "err\t空请求".to_string(),
        other => format!("err\t未知请求 `{other}`"),
    }
}

// ============================================================
// 客户端
// ============================================================

pub struct Client {
    conn: platform::Conn,
}

impl Client {
    /// 连不上就返回错误 —— 调用方的正确反应是退回本地打开索引，不是重试。
    pub fn connect(data: &Path) -> io::Result<Self> {
        Self::connect_timeout(data, DEFAULT_TIMEOUT)
    }

    pub fn connect_timeout(data: &Path, timeout: std::time::Duration) -> io::Result<Self> {
        let ep = endpoint(data);
        Ok(Self {
            conn: platform::connect(&ep, timeout)?,
        })
    }

    pub fn call(&mut self, req: &str) -> io::Result<String> {
        write_frame(&mut self.conn, req.as_bytes())?;
        let payload = read_frame(&mut self.conn)?;
        String::from_utf8(payload).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "响应不是 UTF-8")
        })
    }

    /// 问一下还活着吗。协议版本不匹配时仍返回 `Ok`，由调用方比对 `proto`。
    pub fn ping(&mut self) -> io::Result<Pong> {
        let resp = self.call(&encode_ping())?;
        parse_pong(&resp).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

// ============================================================
// Windows：命名管道
// ============================================================

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr::{null, null_mut};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, ReadFile, WriteFile, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PeekNamedPipe, WaitNamedPipeW,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES,
    };

    /// `PIPE_ACCESS_DUPLEX`：两边都能读写。
    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    /// 内核缓冲区。请求几十字节、响应最大几十 KB，64KB 足够一次写完，
    /// 不会出现"服务端写了一半就阻塞"的情况。
    const BUF: u32 = 64 * 1024;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn last_error() -> io::Error {
        io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
    }

    /// 一条已连接的管道。读之前先 Peek，是为了给客户端实现"超时"：
    /// 同步 `ReadFile` 在字节模式管道上是无限阻塞的，没有 overlapped I/O
    /// 就没法给它设截止时间。Peek 不消费数据，纯查询。
    pub struct Conn {
        h: OwnedHandle,
        deadline: Option<Instant>,
    }

    impl Conn {
        fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
            self.h.as_raw_handle() as _
        }

        fn wait_readable(&self, deadline: Instant) -> io::Result<()> {
            loop {
                let mut avail: u32 = 0;
                let ok = unsafe {
                    PeekNamedPipe(self.raw(), null_mut(), 0, null_mut(), &mut avail, null_mut())
                };
                if ok == 0 {
                    return Err(last_error());
                }
                if avail > 0 {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "等 daemon 响应超时",
                    ));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    impl Read for Conn {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let Some(d) = self.deadline {
                self.wait_readable(d)?;
            }
            let mut n: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    self.raw(),
                    buf.as_mut_ptr(),
                    buf.len().min(u32::MAX as usize) as u32,
                    &mut n,
                    null_mut(),
                )
            };
            if ok == 0 {
                let e = unsafe { GetLastError() };
                // 对端关闭：字节流里这就是 EOF，不是错误
                if e == ERROR_BROKEN_PIPE || e == ERROR_PIPE_NOT_CONNECTED || e == ERROR_NO_DATA {
                    return Ok(0);
                }
                return Err(io::Error::from_raw_os_error(e as i32));
            }
            Ok(n as usize)
        }
    }

    impl Write for Conn {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut n: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    self.raw(),
                    buf.as_ptr(),
                    buf.len().min(u32::MAX as usize) as u32,
                    &mut n,
                    null_mut(),
                )
            };
            if ok == 0 {
                return Err(last_error());
            }
            Ok(n as usize)
        }

        fn flush(&mut self) -> io::Result<()> {
            // 管道不需要 flush 才能被读，但显式 Flush 能让"写完立刻断连"时
            // 数据不至于丢在缓冲区里
            unsafe { FlushFileBuffers(self.raw()) };
            Ok(())
        }
    }

    pub struct Listen {
        name: Vec<u16>,
        /// 已经建好、等着被连接的第一个实例。bind 时建它，
        /// 这样"start 返回"就等于"端点已经可以连"。
        ready: Option<Conn>,
    }

    impl Listen {
        pub fn bind(endpoint: &str) -> io::Result<Self> {
            let name = wide(endpoint);
            let first = create_instance(&name)?;
            Ok(Self {
                name,
                ready: Some(first),
            })
        }

        pub fn accept(&mut self) -> io::Result<Conn> {
            let mut conn = match self.ready.take() {
                Some(c) => c,
                None => create_instance(&self.name)?,
            };

            let ok = unsafe { ConnectNamedPipe(conn.raw(), null_mut()) };
            if ok == 0 {
                let e = unsafe { GetLastError() };
                // 客户端在 CreateNamedPipe 与 ConnectNamedPipe 之间抢先连上了，
                // 此时内核返回这个错误码，但连接是好的
                if e != ERROR_PIPE_CONNECTED {
                    return Err(io::Error::from_raw_os_error(e as i32));
                }
            }
            // 服务端不设读超时会怎样：一个连上来就不发数据的客户端
            // 会把唯一一条服务线程钉死。给个宽松的上限兜住。
            conn.deadline = Some(Instant::now() + CONN_IDLE);
            Ok(conn)
        }
    }

    fn create_instance(name: &[u16]) -> io::Result<Conn> {
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                // PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT 都是 0，
                // 这里只剩一个真正的选项：拒绝远程客户端
                PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUF,
                BUF,
                0,
                null(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        let owned = unsafe { OwnedHandle::from_raw_handle(h as _) };
        Ok(Conn {
            h: owned,
            deadline: None,
        })
    }

    pub fn connect(endpoint: &str, timeout: Duration) -> io::Result<Conn> {
        let name = wide(endpoint);
        let deadline = Instant::now() + timeout;
        loop {
            let h = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    null(),
                    OPEN_EXISTING,
                    0,
                    null_mut(),
                )
            };
            if h != INVALID_HANDLE_VALUE {
                let owned = unsafe { OwnedHandle::from_raw_handle(h as _) };
                return Ok(Conn {
                    h: owned,
                    deadline: Some(deadline),
                });
            }
            let e = unsafe { GetLastError() };
            // 服务器实例全被占用：等它空出来再试。这是并发的正常情况，
            // 不是错误 —— 第二个 `hub search` 就会撞上。
            if e == ERROR_PIPE_BUSY {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "daemon 忙着处理别的请求",
                    ));
                }
                unsafe { WaitNamedPipeW(name.as_ptr(), 200) };
                continue;
            }
            return Err(io::Error::from_raw_os_error(e as i32));
        }
    }
}

// ============================================================
// Unix：域套接字
// ============================================================

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::Duration;

    pub struct Conn {
        inner: UnixStream,
    }

    impl Read for Conn {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl Write for Conn {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    pub struct Listen {
        listener: UnixListener,
    }

    impl Listen {
        pub fn bind(endpoint: &str) -> io::Result<Self> {
            let path = std::path::PathBuf::from(endpoint);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let listener = match UnixListener::bind(&path) {
                Ok(l) => l,
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                    // 上一个进程崩了，socket 文件留在原地。连一下：连得上说明
                    // 真有个活的 daemon（那就该报错），连不上就是残留，删掉重来。
                    if UnixStream::connect(&path).is_ok() {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "已有进程占着这个端点",
                        ));
                    }
                    let _ = std::fs::remove_file(&path);
                    UnixListener::bind(&path)?
                }
                Err(e) => return Err(e),
            };
            Ok(Self { listener })
        }

        pub fn accept(&mut self) -> io::Result<Conn> {
            let (stream, _) = self.listener.accept()?;
            stream.set_read_timeout(Some(CONN_IDLE))?;
            stream.set_write_timeout(Some(CONN_IDLE))?;
            Ok(Conn { inner: stream })
        }
    }

    pub fn connect(endpoint: &str, timeout: Duration) -> io::Result<Conn> {
        let stream = UnixStream::connect(endpoint)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(Conn { inner: stream })
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_roundtrip() {
        for s in [
            "普通文本",
            "带\t制表符的标题",
            "多行\n文本\r\n",
            r"反斜杠 \ 和 \\ 两个",
            r"末尾反斜杠\",
            "",
        ] {
            assert_eq!(unesc(&esc(s)), s, "往返失败：{s:?}");
        }
    }

    #[test]
    fn escape_removes_delimiters() {
        // 这一条才是转义存在的理由：没它，标题里的 \t 会把字段数搞乱
        let s = esc("a\tb\nc");
        assert!(!s.contains('\t'));
        assert!(!s.contains('\n'));
    }

    #[test]
    fn unknown_escape_is_preserved() {
        assert_eq!(unesc(r"a\qb"), r"a\qb");
    }

    #[test]
    fn frame_roundtrip() {
        let mut buf: Vec<u8> = Vec::new();
        for payload in [&b""[..], &b"a"[..], &vec![b'x'; 100_000][..]] {
            write_frame(&mut buf, payload).unwrap();
        }
        let mut cur = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap(), b"");
        assert_eq!(read_frame(&mut cur).unwrap(), b"a");
        assert_eq!(read_frame(&mut cur).unwrap().len(), 100_000);
        assert!(read_frame(&mut cur).is_err(), "读完应报错");
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&((MAX_FRAME as u32 + 1).to_le_bytes()));
        let mut cur = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cur).is_err());
    }

    #[test]
    fn search_reply_roundtrip_keeps_fields() {
        let hits = vec![
            Hit {
                doc_id: 42,
                uri: "files/C:/笔记/带\t制表符.md".into(),
                source: SourceId::Files,
                title: "第一行\n第二行".into(),
                score: 0.005,
                mtime: 1_700_000_000,
            },
            Hit {
                doc_id: 7,
                uri: "terminal://bash/abc".into(),
                source: SourceId::Terminal,
                title: "cargo test".into(),
                score: 12.5,
                mtime: 1_600_000_000,
            },
        ];
        let wire = encode_search_reply(&hits, 1234);
        let back = parse_search_reply(&wire).unwrap();
        assert_eq!(back.num_docs, 1234);
        assert_eq!(back.hits.len(), 2);
        assert_eq!(back.hits[0].uri, hits[0].uri);
        assert_eq!(back.hits[0].title, hits[0].title);
        assert_eq!(back.hits[0].doc_id, 42);
        assert!((back.hits[0].score - 0.005).abs() < 1e-6);
        assert_eq!(back.hits[1].source, SourceId::Terminal);
    }

    #[test]
    fn truncated_reply_is_an_error() {
        // 声明 3 条却只给了 1 条 —— 不能静默当成 1 条返回，
        // 那会让用户以为"就这些"，实际上是传输被截断了
        let wire = "ok\t3\t10\n1\t0.5\t100\tfiles\tfiles/a\tA\n";
        assert!(parse_search_reply(wire).is_err());
    }

    #[test]
    fn error_reply_surfaces_message() {
        let e = parse_search_reply("err\t查询为空").unwrap_err();
        assert_eq!(e, "查询为空");
    }

    #[test]
    fn endpoint_is_stable_and_dir_specific() {
        let t = tempfile::TempDir::new().unwrap();
        let a = endpoint(t.path());
        let b = endpoint(t.path());
        assert_eq!(a, b, "同一目录必须算出同一个端点");

        let t2 = tempfile::TempDir::new().unwrap();
        assert_ne!(a, endpoint(t2.path()), "不同目录不能串台");

        // 相对路径与绝对路径必须归一
        let sub = t.path().join("x");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(endpoint(&sub), endpoint(&std::path::absolute(&sub).unwrap()));
    }

    #[test]
    fn client_without_server_fails_fast() {
        let t = tempfile::TempDir::new().unwrap();
        let started = std::time::Instant::now();
        let err = Client::connect_timeout(t.path(), std::time::Duration::from_millis(500));
        assert!(err.is_err(), "没有 daemon 时不该连上");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "连不上要快点失败，不能拖住 CLI"
        );
    }

    // ---------- 端到端：真的起一个服务，真的连上去问 ----------

    fn fixture(dir: &Path) -> Arc<HubIndex> {
        use hub_store::Indexer;
        let mut ix = Indexer::open(dir).unwrap();
        ix.add(
            SourceId::Files,
            "files/a.md",
            "内存与零拷贝".into(),
            "mmap 和 zero copy 的笔记".into(),
            1_700_000_000,
        )
        .unwrap();
        ix.add(
            SourceId::Terminal,
            "terminal://bash/1",
            "cargo test".into(),
            "cargo test --all --release".into(),
            1_700_000_100,
        )
        .unwrap();
        ix.flush().unwrap();
        ix.index_handle()
    }

    #[test]
    fn server_answers_ping_and_search() {
        let t = tempfile::TempDir::new().unwrap();
        let index = fixture(&t.path().join("data"));
        let data = t.path().join("data");

        let mut srv = Server::start(&data, index, 1_700_000_000, Log::silent()).unwrap();

        {
            let mut c = Client::connect(&data).unwrap();
            let pong = c.ping().unwrap();
            assert_eq!(pong.proto, PROTO);
            assert_eq!(pong.num_docs, 2);
            assert_eq!(pong.pid, std::process::id());
            assert_eq!(pong.started_at, 1_700_000_000);

            let resp = c.call(&encode_search(5, "zero copy")).unwrap();
            let r = parse_search_reply(&resp).unwrap();
            assert_eq!(r.hits.len(), 1, "应命中一篇：{r:?}");
            assert_eq!(r.hits[0].uri, "files/a.md");
            assert_eq!(r.num_docs, 2);

            // 同一条连接可以继续问
            let resp = c.call(&encode_search(5, "cargo test")).unwrap();
            assert_eq!(parse_search_reply(&resp).unwrap().hits.len(), 1);
        }

        srv.stop();
    }

    #[test]
    fn server_survives_garbage_and_unknown_verbs() {
        let t = tempfile::TempDir::new().unwrap();
        let data = t.path().join("data");
        let index = fixture(&data);
        let mut srv = Server::start(&data, index, 0, Log::silent()).unwrap();

        {
            let mut c = Client::connect(&data).unwrap();
            let r = c.call("bogus").unwrap();
            assert!(r.starts_with("err\t"), "{r}");

            let r = c.call("search\tnan\tx").unwrap();
            assert!(r.starts_with("err\t"), "{r}");

            let r = c.call("search\t5\t").unwrap();
            assert!(r.starts_with("err\t"), "空查询应报错而不是返回全部：{r}");

            // 出过错之后连接仍然可用 —— 一条畸形请求不该把服务打死
            assert_eq!(c.ping().unwrap().proto, PROTO);
        }
        srv.stop();
    }

    #[test]
    fn query_with_tab_survives_the_wire() {
        let t = tempfile::TempDir::new().unwrap();
        let data = t.path().join("data");
        let index = fixture(&data);
        let mut srv = Server::start(&data, index, 0, Log::silent()).unwrap();

        {
            let mut c = Client::connect(&data).unwrap();
            // 查询里带制表符和引号，拼装层不能把它拆成两个字段
            let resp = c.call(&encode_search(5, "zero\tcopy\"")).unwrap();
            assert!(resp.starts_with("ok\t"), "{resp}");
        }
        srv.stop();
    }

    #[test]
    fn stop_releases_the_endpoint() {
        let t = tempfile::TempDir::new().unwrap();
        let data = t.path().join("data");
        let index = fixture(&data);
        let mut srv = Server::start(&data, index, 0, Log::silent()).unwrap();
        assert!(Client::connect(&data).is_ok());
        srv.stop();
        // 停了之后不该还能连上（Windows 上管道随句柄消失，Unix 上 socket 文件被删）
        let after = Client::connect_timeout(&data, std::time::Duration::from_millis(300));
        assert!(after.is_err(), "停止后端点应不可达");
    }}
