//! 进程间通信：Unix socket 路径、请求协议（编码/解析）、以及 CLI 客户端的收发。
//!
//! 线协议是一行文本、字段以 `\t` 分隔。守护进程与客户端共用 [`Request`] 的
//! `encode`/`parse`，避免同一命令形状在「客户端拼、守护进程拆」两处各写一遍。

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

/// IPC socket 路径：优先 `$XDG_RUNTIME_DIR`（每用户私有、随登录会话清理），
/// 回退到系统临时目录并带上用户名，避免多用户在共享目录下冲突。
pub fn socket_path() -> String {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let dir = dir.trim_end_matches('/');
        if !dir.is_empty() {
            return format!("{}/dms-email-client.sock", dir);
        }
    }
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "user".to_string());
    std::env::temp_dir()
        .join(format!("dms-email-client-{}.sock", user))
        .to_string_lossy()
        .into_owned()
}

/// 客户端 → 守护进程的请求。每个变体对应一行线协议。
pub enum Request {
    /// 查询当前状态（一次性返回状态 JSON）
    Status,
    /// 订阅状态变更：守护进程每次状态变化即推送一行 JSON，直到连接关闭
    Watch,
    /// 请求守护进程退出（reload/shutdown 行为一致：应答后退出，前端会重启）
    Reload,
    Shutdown,
    /// 取某封邮件正文
    Body { account: String, folder: String, uid: String },
    /// 标记某封邮件已读
    Read { account: String, folder: String, uid: String },
    /// 批量标记已读（account 为空 = 所有账户）
    ReadAll { account: String },
    /// 翻译某封邮件正文
    Translate {
        account: String,
        folder: String,
        uid: String,
        src: String,
        tgt: String,
        engine: String,
        deeplx_url: String,
    },
}

impl Request {
    /// 编码为线协议一行（不含结尾换行）。
    pub fn encode(&self) -> String {
        match self {
            Request::Status => "status".to_string(),
            Request::Watch => "watch".to_string(),
            Request::Reload => "reload".to_string(),
            Request::Shutdown => "shutdown".to_string(),
            Request::Body { account, folder, uid } => {
                format!("body\t{account}\t{folder}\t{uid}")
            }
            Request::Read { account, folder, uid } => {
                format!("read\t{account}\t{folder}\t{uid}")
            }
            Request::ReadAll { account } => format!("read_all\t{account}"),
            Request::Translate { account, folder, uid, src, tgt, engine, deeplx_url } => {
                format!("translate\t{account}\t{folder}\t{uid}\t{src}\t{tgt}\t{engine}\t{deeplx_url}")
            }
        }
    }

    /// 从守护进程收到的一行解析出请求。无法识别的一律当作 [`Request::Status`]
    /// （与旧行为一致——未知命令返回当前状态）。
    pub fn parse(line: &str) -> Request {
        let line = line.trim_end_matches(['\n', '\r']);
        let parts: Vec<&str> = line.split('\t').collect();
        let s = |v: &&str| v.to_string();
        match parts.as_slice() {
            ["watch"] => Request::Watch,
            ["reload"] => Request::Reload,
            ["shutdown"] => Request::Shutdown,
            ["body", a, f, u] => Request::Body { account: s(a), folder: s(f), uid: s(u) },
            ["read", a, f, u] => Request::Read { account: s(a), folder: s(f), uid: s(u) },
            ["read_all", a] => Request::ReadAll { account: s(a) },
            ["translate", a, f, u, src, tgt, engine, deeplx] => Request::Translate {
                account: s(a),
                folder: s(f),
                uid: s(u),
                src: s(src),
                tgt: s(tgt),
                engine: s(engine),
                deeplx_url: s(deeplx),
            },
            // 兼容旧前端（未带引擎/URL 参数）：默认本地 nllb
            ["translate", a, f, u, src, tgt] => Request::Translate {
                account: s(a),
                folder: s(f),
                uid: s(u),
                src: s(src),
                tgt: s(tgt),
                engine: "nllb".to_string(),
                deeplx_url: String::new(),
            },
            _ => Request::Status,
        }
    }
}

/// 连接守护进程 socket，发送一行请求并把其响应打印到 stdout（一次性读到 EOF）。
pub fn send(req: &Request) {
    match UnixStream::connect(socket_path()) {
        Ok(mut stream) => {
            let _ = stream.write_all(req.encode().as_bytes());
            let _ = stream.write_all(b"\n");
            let _ = stream.flush();
            let mut response = String::new();
            if let Err(e) = stream.read_to_string(&mut response) {
                print_error_json(&format!("Read error: {}", e));
                return;
            }
            // 直接输出守护进程返回的 JSON
            println!("{}", response);
        }
        Err(_) => {
            print_error_json("Daemon not running");
        }
    }
}

/// 连接守护进程 socket，发送订阅请求，然后把服务端持续推送的每一行**流式**转发到
/// stdout（逐块读取并 flush，不等 EOF）。守护进程关闭连接或出错时返回，进程随之退出，
/// 前端据此重连。前端用 SplitParser 逐行消费，实现零延迟的状态更新。
pub fn stream(req: &Request) {
    match UnixStream::connect(socket_path()) {
        Ok(mut stream) => {
            let _ = stream.write_all(req.encode().as_bytes());
            let _ = stream.write_all(b"\n");
            let _ = stream.flush();
            let mut buf = [0u8; 4096];
            let stdout = std::io::stdout();
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break, // 守护进程关闭了连接
                    Ok(n) => {
                        let mut lock = stdout.lock();
                        if lock.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let _ = lock.flush();
                    }
                    Err(_) => break,
                }
            }
        }
        Err(_) => {
            print_error_json("Daemon not running");
        }
    }
}

fn print_error_json(err_msg: &str) {
    let err_json = serde_json::json!({
        "error": err_msg,
        "unread_mails": [],
        "last_update": ""
    });
    println!("{}", err_json);
}
