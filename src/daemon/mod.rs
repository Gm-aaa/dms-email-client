//! 守护进程编排：启动各账户同步线程与 config 监视器，监听 IPC socket，把每个连接的
//! [`Request`] 分发给对应处理器。
//!
//! 子模块职责：
//! - [`state`]：共享状态模型 + watch 广播（版本号 + 条件变量）
//! - [`imap_sync`]：IMAP 建连、每账户检查循环 + IDLE、文件夹探测
//! - [`commands`]：body / read / read_all / translate 命令处理器

mod commands;
mod imap_sync;
mod state;

use crate::config::{Account, Config};
use crate::ipc::{socket_path, Request};
use crate::sysmem::tune_allocator;
use state::{notifier, DaemonState};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

pub fn run_daemon(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    // 单例保护：若已有守护进程在监听 socket（多屏/多栏会实例化多份插件），本实例退出
    if already_running() {
        println!("已有守护进程在运行，本实例退出。");
        return Ok(());
    }

    // 限制 glibc malloc arena 数量，降低多线程下的 RSS 过度预留
    tune_allocator();

    // 每账户缓存上限 = 总上限 / 已启用账户数（0 表示不限制）
    let enabled_count = config.accounts.iter().filter(|a| a.enabled).count();
    let per_account_limit = if config.cache_limit == 0 || enabled_count == 0 {
        config.cache_limit
    } else {
        std::cmp::max(1, config.cache_limit / enabled_count)
    };
    let cache_dir = config.cache_dir.clone();
    let body_cache_limit = config.body_cache_limit;
    // IMAP 稳态读/写超时（最小 5 秒，防误配成 0 把连接秒断）
    let io_timeout = Duration::from_secs(config.imap_timeout_secs.max(5));
    // IDLE 轮询间隔（兼作弱 IDLE 服务商的兜底 + 代理长连接保活）。
    // 下限 15 秒防过度频繁拉取，上限 29 分钟遵循 RFC 2177 的 IDLE 重发建议。
    let idle_poll = Duration::from_secs(config.poll_interval_secs.clamp(15, 29 * 60));
    // 保留账户列表供按需操作（取正文 / 标记已读）使用
    let accounts: Arc<Vec<Account>> = Arc::new(config.accounts.clone());
    let state = Arc::new(RwLock::new(DaemonState::default()));

    // 1. 为每个已启用的账户启动 IMAP 检查线程
    for account in config.accounts {
        if !account.enabled {
            println!("[{}] 账户已禁用，跳过。", account.name);
            continue;
        }
        let state_clone = Arc::clone(&state);
        thread::spawn(move || {
            imap_sync::run_account_loop(account, state_clone, per_account_limit, io_timeout, idle_poll);
        });
    }

    // 2. 监视配置文件：图形设置保存后文件变更，守护进程退出，由插件前端自动重启以加载新配置
    spawn_config_watcher();

    // 3. 启动 Unix Socket 监听器（IPC）
    // 启动时清理旧 socket 文件
    let _ = fs::remove_file(socket_path());
    let listener = UnixListener::bind(socket_path())?;
    println!("Daemon IPC socket listening on {}", socket_path());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // 每连接一线程：`watch` 是长连接（会一直阻塞等待状态变更再推送），
                // 若仍在 accept 循环里同步处理会堵死后续的 body/read/status 请求。
                let state = Arc::clone(&state);
                let accounts = Arc::clone(&accounts);
                let cache_dir = cache_dir.clone();
                thread::spawn(move || {
                    handle_client(
                        stream,
                        &state,
                        &accounts,
                        &cache_dir,
                        body_cache_limit,
                        io_timeout,
                    )
                });
            }
            Err(e) => {
                eprintln!("Socket connection error: {:?}", e);
            }
        }
    }

    Ok(())
}

/// 处理单个客户端连接：读取一行命令、解析为 [`Request`]、分发给对应处理器。
fn handle_client(
    mut stream: UnixStream,
    state: &Arc<RwLock<DaemonState>>,
    accounts: &[Account],
    cache_dir: &str,
    body_cache_limit: usize,
    io_timeout: Duration,
) {
    let mut line = String::new();
    if let Ok(clone) = stream.try_clone() {
        let mut reader = BufReader::new(clone);
        let _ = reader.read_line(&mut line);
    }

    match Request::parse(&line) {
        Request::Watch => {
            // 长连接订阅：立即推送当前状态，之后每次状态变更即推送一行 JSON。
            handle_watch(stream, state);
        }
        Request::Reload | Request::Shutdown => {
            let _ = stream.write_all(b"{\"ok\":true}");
            let _ = stream.flush();
            let _ = fs::remove_file(socket_path());
            println!("收到关停指令，守护进程退出。");
            std::process::exit(0);
        }
        Request::Body { account, folder, uid } => {
            let resp = commands::fetch_body(
                accounts, cache_dir, body_cache_limit, io_timeout, &account, &folder, &uid,
            );
            let _ = stream.write_all(resp.as_bytes());
        }
        Request::Translate { account, folder, uid, src, tgt, engine, deeplx_url } => {
            let resp = commands::fetch_translation(
                accounts, io_timeout, &account, &folder, &uid, &src, &tgt, &engine, &deeplx_url,
            );
            let _ = stream.write_all(resp.as_bytes());
        }
        Request::Read { account, folder, uid } => {
            let resp = commands::mark_read(accounts, state, io_timeout, &account, &folder, &uid);
            let _ = stream.write_all(resp.as_bytes());
        }
        Request::ReadAll { account } => {
            let resp = commands::mark_read_all(accounts, state, io_timeout, &account);
            let _ = stream.write_all(resp.as_bytes());
        }
        Request::Status => {
            let data = {
                // RwLock poisoning 恢复：如果 lock 被 poison，恢复而非 panic
                let r_state = state.read().unwrap_or_else(|e| e.into_inner());
                serde_json::to_string(&*r_state).unwrap_or_default()
            };
            let _ = stream.write_all(data.as_bytes());
        }
    }
}

/// `watch` 长连接处理：先推当前状态，然后阻塞等待，状态一变即推送最新状态
/// （一行 JSON + `\n`）。带 60 秒心跳：即便无变更也周期性重推，既作兜底，又能借
/// 写失败探测到已断开的前端，避免线程永久滞留。coalescing：多次快速变更只需一次重推。
fn handle_watch(mut stream: UnixStream, state: &Arc<RwLock<DaemonState>>) {
    let n = notifier();
    // 进入时的版本号；若在快照/发送期间发生变更，下面的 wait 会立即返回，不漏更新。
    let mut last = *n.version.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        // 推送当前状态快照（读锁尽快释放，避免阻塞写侧）
        let json = {
            let r = state.read().unwrap_or_else(|e| e.into_inner());
            serde_json::to_string(&*r).unwrap_or_default()
        };
        if stream.write_all(json.as_bytes()).is_err()
            || stream.write_all(b"\n").is_err()
            || stream.flush().is_err()
        {
            break; // 前端已断开，退出线程
        }

        // 阻塞直到状态变更（版本号推进）或 60 秒心跳超时
        let guard = n.version.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _timeout) = n
            .cv
            .wait_timeout_while(guard, Duration::from_secs(60), |v| *v == last)
            .unwrap_or_else(|e| e.into_inner());
        last = *guard;
    }
}

/// 尝试连接现有 socket 并请求状态；若得到有效响应说明已有守护进程在运行。
///
/// 探测带读/写超时：若 socket 上有个**卡死**的旧进程在监听但不应答（或正处于
/// remove_file→exit 的关停窗口），无超时的 `read_to_string` 会永久阻塞，导致新实例
/// 卡在启动、永不 bind。健康守护进程毫秒级即应答，故 3 秒超时不会误伤正常实例；
/// 一旦超时则视为“没有健康实例在跑”，继续往下接管 socket（对卡死进程正是所需的恢复）。
fn already_running() -> bool {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
    if let Ok(mut s) = UnixStream::connect(socket_path()) {
        let _ = s.set_read_timeout(Some(PROBE_TIMEOUT));
        let _ = s.set_write_timeout(Some(PROBE_TIMEOUT));
        let _ = s.write_all(b"status\n");
        let _ = s.flush();
        let mut buf = String::new();
        if s.read_to_string(&mut buf).is_ok() && buf.contains('{') {
            return true;
        }
    }
    false
}

/// 后台线程：用 inotify 监视配置文件，发生变更则退出进程（前端会重启以加载新配置）。
///
/// 监视的是配置文件所在**目录**而非文件本身：图形设置保存时通常是「写临时文件再
/// rename 覆盖」，直接盯着原文件的 inode 会在 rename 后失效；盯目录能可靠捕获对该
/// 文件名的创建 / 修改 / 移入。相比旧的每 3 秒轮询 mtime，inotify 空闲时零唤醒。
fn spawn_config_watcher() {
    use notify::{RecursiveMode, Watcher};

    let path = Config::get_path();
    let dir = path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("配置监视器初始化失败（配置热重载不可用）：{:?}", e);
                return;
            }
        };
        if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
            eprintln!("配置监视器无法监视 {:?}：{:?}", dir, e);
            return;
        }

        for res in rx {
            let event = match res {
                Ok(ev) => ev,
                Err(_) => continue,
            };
            // 仅当事件涉及配置文件本身时才触发重载
            if event.paths.iter().any(|p| p == &path) {
                println!("检测到配置文件变更，守护进程退出以重新加载新配置。");
                let _ = fs::remove_file(socket_path());
                std::process::exit(0);
            }
        }
    });
}
