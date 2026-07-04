use crate::config::{Account, Config};
use chrono::{Local, TimeZone};
use mail_parser::Address;
use native_tls::TlsConnector;
use notify_rust::Notification;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::Duration;

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

/// IMAP over TLS 会话类型别名
type ImapSession = imap::Session<native_tls::TlsStream<std::net::TcpStream>>;

/// 状态变更广播：单例的「版本号 + 条件变量」。任何修改 `DaemonState` 的地方都调用
/// [`notify_state_changed`]，`watch` 长连接会被立即唤醒并推送最新状态，从而做到
/// 「新邮件到达 / 标记已读」零延迟反映到前端，无需前端轮询。
struct Notifier {
    version: Mutex<u64>,
    cv: Condvar,
}

static NOTIFIER: OnceLock<Notifier> = OnceLock::new();

fn notifier() -> &'static Notifier {
    NOTIFIER.get_or_init(|| Notifier {
        version: Mutex::new(0),
        cv: Condvar::new(),
    })
}

/// 通知所有 `watch` 订阅者：状态已变化，请重新推送。
fn notify_state_changed() {
    let n = notifier();
    let mut v = n.version.lock().unwrap_or_else(|e| e.into_inner());
    *v = v.wrapping_add(1);
    n.cv.notify_all();
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailInfo {
    pub account: String,
    /// 邮件所在的 IMAP 文件夹（如 "INBOX"、"[Gmail]/Spam"），用于分类与按需取正文
    #[serde(default = "default_folder")]
    pub folder: String,
    pub uid: u32,
    pub from: String,
    pub subject: String,
    pub date: String,
    /// 是否已读（\Seen）。前端据此决定是否显示未读红点；已读邮件仍保留在列表中。
    #[serde(default)]
    pub seen: bool,
}

fn default_folder() -> String {
    "INBOX".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountError {
    pub account: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonState {
    pub unread_mails: Vec<MailInfo>,
    pub last_update: String,
    /// 各账户当前的连接/登录错误，供前端显示
    #[serde(default)]
    pub errors: Vec<AccountError>,
}

/// 记录某账户的错误（同名先移除再插入，避免重复）
fn set_account_error(state: &Arc<RwLock<DaemonState>>, account: &str, message: String) {
    {
        let mut w = state.write().unwrap_or_else(|e| e.into_inner());
        w.errors.retain(|e| e.account != account);
        w.errors.push(AccountError {
            account: account.to_string(),
            message,
        });
    }
    notify_state_changed();
}

/// 清除某账户的错误（连接/登录成功后调用）
fn clear_account_error(state: &Arc<RwLock<DaemonState>>, account: &str) {
    let changed = {
        let mut w = state.write().unwrap_or_else(|e| e.into_inner());
        let before = w.errors.len();
        w.errors.retain(|e| e.account != account);
        w.errors.len() != before
    };
    if changed {
        notify_state_changed();
    }
}

pub fn run_daemon(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    // 单例保护：若已有守护进程在监听 socket（多屏/多栏会实例化多份插件），本实例退出
    if already_running() {
        println!("已有守护进程在运行，本实例退出。");
        return Ok(());
    }

    // 每账户缓存上限 = 总上限 / 已启用账户数（0 表示不限制）
    let enabled_count = config.accounts.iter().filter(|a| a.enabled).count();
    let per_account_limit = if config.cache_limit == 0 || enabled_count == 0 {
        config.cache_limit
    } else {
        std::cmp::max(1, config.cache_limit / enabled_count)
    };
    let cache_dir = config.cache_dir.clone();
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
            run_account_loop(account, state_clone, per_account_limit);
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
                thread::spawn(move || handle_client(stream, &state, &accounts, &cache_dir));
            }
            Err(e) => {
                eprintln!("Socket connection error: {:?}", e);
            }
        }
    }

    Ok(())
}

/// 处理单个客户端连接：先读取一行命令（字段以 \t 分隔），再据此响应。
/// - `status`（或任意未知命令）：返回当前状态 JSON
/// - `reload` / `shutdown`：响应后退出进程
/// - `body\t<account>\t<folder>\t<uid>`：返回该邮件正文
/// - `read\t<account>\t<folder>\t<uid>`：标记该邮件为已读
/// - `read_all\t<account>`：标记该账户（account 为空则所有账户）当前全部未读为已读
fn handle_client(
    mut stream: UnixStream,
    state: &Arc<RwLock<DaemonState>>,
    accounts: &[Account],
    cache_dir: &str,
) {
    let mut line = String::new();
    if let Ok(clone) = stream.try_clone() {
        let mut reader = BufReader::new(clone);
        let _ = reader.read_line(&mut line);
    }
    let line = line.trim_end_matches(['\n', '\r']);
    let parts: Vec<&str> = line.split('\t').collect();

    match parts.as_slice() {
        ["watch"] => {
            // 长连接订阅：立即推送当前状态，之后每次状态变更即推送一行 JSON。
            handle_watch(stream, state);
        }
        ["reload"] | ["shutdown"] => {
            let _ = stream.write_all(b"{\"ok\":true}");
            let _ = stream.flush();
            let _ = fs::remove_file(socket_path());
            println!("收到 {} 指令，守护进程退出。", parts[0]);
            std::process::exit(0);
        }
        ["body", account, folder, uid] => {
            let resp = fetch_body(accounts, cache_dir, account, folder, uid);
            let _ = stream.write_all(resp.as_bytes());
        }
        ["read", account, folder, uid] => {
            let resp = mark_read(accounts, state, account, folder, uid);
            let _ = stream.write_all(resp.as_bytes());
        }
        ["read_all", account_filter] => {
            let resp = mark_read_all(accounts, state, account_filter);
            let _ = stream.write_all(resp.as_bytes());
        }
        _ => {
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

fn json_err(msg: &str) -> String {
    serde_json::json!({ "ok": false, "error": msg }).to_string()
}

/// 建立一个 IMAP over TLS 会话并登录。
///
/// 注意：`imap::connect` 内部用 `TcpStream::connect` 且不设任何超时，登录前读取
/// 服务器 greeting 也是无超时阻塞。当守护进程在网络/代理尚未就绪时启动（例如登录
/// 会话刚把插件拉起、TUN 代理还没接管），TCP 可能连上代理但 greeting 永远不到，
/// 线程就永久卡在 `read_greeting()`：既不返回 Ok（状态永远为空），也不返回 Err
/// （`run_account_loop` 的 10 秒重连循环不触发），前端因此一直空白且无报错。
/// 所以这里手动建连，为「TCP 连接 + TLS 握手 + greeting + 登录」设置超时；成功后
/// 清除超时，交还给正常的 fetch / IDLE（IDLE 会自行设置并复位读超时）。
fn connect_session(account: &Account) -> Result<ImapSession, Box<dyn std::error::Error>> {
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
    const IO_TIMEOUT: Duration = Duration::from_secs(30);

    let tls = TlsConnector::builder().build()?;

    // 解析地址并带超时建立 TCP 连接，避免半就绪的代理/网络下永久阻塞
    let addr = (account.host.as_str(), account.port)
        .to_socket_addrs()?
        .next()
        .ok_or("无法解析 IMAP 服务器地址")?;
    let tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    tcp.set_read_timeout(Some(IO_TIMEOUT))?;
    tcp.set_write_timeout(Some(IO_TIMEOUT))?;
    // 同一 socket 的副本：握手/登录完成后用它清除超时（SO_RCVTIMEO 在 dup 的 fd 间共享）
    let tcp_ctl = tcp.try_clone()?;

    // TLS 握手 + 读取 greeting + 登录，均受上面的读写超时约束
    let tls_stream = tls.connect(&account.host, tcp)?;
    let mut client = imap::Client::new(tls_stream);
    client.read_greeting()?;
    let session = client
        .login(&account.username, &account.password)
        .map_err(|e| e.0)?;

    // 连接已建立并登录成功：清除超时，恢复正常阻塞语义
    let _ = tcp_ctl.set_read_timeout(None);
    let _ = tcp_ctl.set_write_timeout(None);

    Ok(session)
}

/// 把账户名/文件夹名转为安全的文件名片段
fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

/// 正文磁盘缓存路径：<cache_dir>/<account>/<folder>/<uid>.json
fn body_cache_path(cache_dir: &str, account: &str, folder: &str, uid: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(cache_dir)
        .join(sanitize_component(account))
        .join(sanitize_component(folder))
        // v2：正文改为 HTML 缓存，换文件名以让旧的纯文本缓存自然失效
        .join(format!("{}.v2.json", uid))
}

/// 按需获取某封邮件的正文（只读打开，不会标记已读）；优先读磁盘缓存
fn fetch_body(
    accounts: &[Account],
    cache_dir: &str,
    account_name: &str,
    folder: &str,
    uid: &str,
) -> String {
    // 1. 命中磁盘缓存则直接返回
    let cache_path = body_cache_path(cache_dir, account_name, folder, uid);
    if let Ok(cached) = fs::read_to_string(&cache_path) {
        if !cached.is_empty() {
            return cached;
        }
    }

    let account = match accounts.iter().find(|a| a.name == account_name) {
        Some(a) => a,
        None => return json_err("账户不存在"),
    };

    let run = || -> Result<String, Box<dyn std::error::Error>> {
        let mut session = connect_session(account)?;
        session.examine(folder)?; // 只读，避免标记已读
        let fetches = session.uid_fetch(uid, "BODY.PEEK[]")?;
        let fetch = fetches.iter().next().ok_or("未找到该邮件")?;
        let raw = fetch.body().ok_or("邮件无正文数据")?;
        let msg = mail_parser::MessageParser::default()
            .parse(raw)
            .ok_or("邮件解析失败")?;

        // 正文先取纯文本，再转成可渲染的 HTML：URL → 可点击「🔗 域名 + ⧉ 复制」，
        // 4–8 位验证码 → 可点击复制。前端 TextEdit(RichText) 直接显示。
        let body = body_to_html(&extract_body(&msg));
        let from = msg.from().map(format_address).unwrap_or_default();
        let subject = msg.subject().unwrap_or("(无主题)").to_string();
        let date = msg
            .date()
            .and_then(|d| Local.timestamp_opt(d.to_timestamp(), 0).single())
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();

        let _ = session.logout();
        Ok(serde_json::json!({
            "ok": true,
            "from": from,
            "subject": subject,
            "date": date,
            "folder": folder,
            "body": body,
        })
        .to_string())
    };

    match run() {
        Ok(json) => {
            // 2. 写入磁盘缓存（失败不影响返回）
            if let Some(parent) = cache_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&cache_path, &json);
            json
        }
        Err(e) => json_err(&e.to_string()),
    }
}

/// 标记某封邮件为已读（设置 \Seen 标志），并从守护进程状态中立即移除该邮件
fn mark_read(
    accounts: &[Account],
    state: &Arc<RwLock<DaemonState>>,
    account_name: &str,
    folder: &str,
    uid: &str,
) -> String {
    let account = match accounts.iter().find(|a| a.name == account_name) {
        Some(a) => a,
        None => return json_err("账户不存在"),
    };

    let run = || -> Result<String, Box<dyn std::error::Error>> {
        let mut session = connect_session(account)?;
        session.select(folder)?; // 读写方式打开
        session.uid_store(uid, "+FLAGS (\\Seen)")?;
        let _ = session.logout();
        Ok(serde_json::json!({ "ok": true }).to_string())
    };

    let result = run();
    if result.is_ok() {
        // 标记成功后立即把该邮件置为已读（保留在列表，仅去掉未读红点）
        if let Ok(uid_num) = uid.parse::<u32>() {
            let mut w = state.write().unwrap_or_else(|e| e.into_inner());
            for m in w.unread_mails.iter_mut() {
                if m.account == account_name && m.folder == folder && m.uid == uid_num {
                    m.seen = true;
                }
            }
        }
        // 已读状态变化 → 立即推送
        notify_state_changed();
    }

    result.unwrap_or_else(|e| json_err(&e.to_string()))
}

/// 批量标记已读：把当前状态中（account_filter 为空则不限账户）的所有未读邮件
/// 设为 \Seen。按 (账户, 文件夹) 分组，每个账户只建一个连接、每个文件夹一次
/// uid_store。成功的账户立即从状态移除其邮件。
fn mark_read_all(
    accounts: &[Account],
    state: &Arc<RwLock<DaemonState>>,
    account_filter: &str,
) -> String {
    // 先快照匹配的邮件（拿完即释放读锁，再去联网）
    let targets: Vec<MailInfo> = {
        let r = state.read().unwrap_or_else(|e| e.into_inner());
        r.unread_mails
            .iter()
            .filter(|m| !m.seen && (account_filter.is_empty() || m.account == account_filter))
            .cloned()
            .collect()
    };
    if targets.is_empty() {
        return serde_json::json!({ "ok": true, "marked": 0 }).to_string();
    }

    use std::collections::BTreeMap;
    let mut by_account: BTreeMap<String, BTreeMap<String, Vec<u32>>> = BTreeMap::new();
    for m in &targets {
        by_account
            .entry(m.account.clone())
            .or_default()
            .entry(m.folder.clone())
            .or_default()
            .push(m.uid);
    }

    let mut marked = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for (acc_name, by_folder) in &by_account {
        let account = match accounts.iter().find(|a| &a.name == acc_name) {
            Some(a) => a,
            None => continue,
        };
        let run = || -> Result<usize, Box<dyn std::error::Error>> {
            let mut session = connect_session(account)?;
            let mut n = 0;
            for (folder, uids) in by_folder {
                session.select(folder)?;
                let set = uids
                    .iter()
                    .map(|u| u.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                session.uid_store(set, "+FLAGS (\\Seen)")?;
                n += uids.len();
            }
            let _ = session.logout();
            Ok(n)
        };
        match run() {
            Ok(n) => {
                marked += n;
                // 把该账户已处理的邮件置为已读（保留在列表，仅去红点）
                {
                    let mut w = state.write().unwrap_or_else(|e| e.into_inner());
                    for m in w.unread_mails.iter_mut() {
                        if &m.account == acc_name
                            && by_folder.get(&m.folder).is_some_and(|v| v.contains(&m.uid))
                        {
                            m.seen = true;
                        }
                    }
                }
                // 已读状态变化 → 立即推送
                notify_state_changed();
            }
            Err(e) => errors.push(format!("{}: {}", acc_name, e)),
        }
    }

    if errors.is_empty() {
        serde_json::json!({ "ok": true, "marked": marked }).to_string()
    } else {
        serde_json::json!({ "ok": false, "marked": marked, "error": errors.join("; ") }).to_string()
    }
}

/// 从已解析邮件中提取可读正文：优先纯文本，其次把 HTML 粗略转为文本
fn extract_body(msg: &mail_parser::Message) -> String {
    if let Some(t) = msg.body_text(0) {
        let s = t.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    if let Some(h) = msg.body_html(0) {
        return html_to_text(&h);
    }
    String::new()
}

/// 极简 HTML → 纯文本：保留段落/换行，去掉标签，折叠空白
fn html_to_text(html: &str) -> String {
    let mut s = html.to_string();
    for tag in ["<br>", "<br/>", "<br />", "</p>", "</div>", "</tr>", "</li>"] {
        s = s.replace(tag, "\n");
    }
    let mut out = String::new();
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    // 折叠多余空行/空白，但保留换行结构
    out.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 转义 HTML 特殊字符
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 处理非链接文本片段：转义 HTML，把 4–8 位独立数字串识别为验证码（可点击复制），
/// 末尾把换行转成 <br>。
fn process_text_segment(seg: &str) -> String {
    static DIGIT_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = DIGIT_RE.get_or_init(|| regex::Regex::new(r"[0-9]+").unwrap());

    let mut out = String::new();
    let mut last = 0;
    for m in re.find_iter(seg) {
        let len = m.end() - m.start();
        if (4..=8).contains(&len) {
            out.push_str(&html_escape(&seg[last..m.start()]));
            let code = m.as_str();
            // 验证码：点击复制（href="copy:..."）
            out.push_str(&format!(
                "<a href=\"copy:{code}\">{code} ⧉</a>",
                code = code
            ));
            last = m.end();
        }
    }
    out.push_str(&html_escape(&seg[last..]));
    out.replace('\n', "<br>")
}

/// 把纯文本正文转成可渲染的 HTML：URL → 「🔗 域名」(点击打开) +「⧉」(复制完整链接)，
/// 其余文本经 process_text_segment 处理。单遍扫描，绝不残留占位符。
fn body_to_html(plain: &str) -> String {
    static URL_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let url_re = URL_RE
        .get_or_init(|| regex::Regex::new(r#"<?(https?://[^\s<>"']+)>?"#).unwrap());

    let mut out = String::new();
    let mut last = 0;
    for caps in url_re.captures_iter(plain) {
        let whole = caps.get(0).unwrap();
        let u = caps.get(1).unwrap().as_str();
        out.push_str(&process_text_segment(&plain[last..whole.start()]));

        // 可读域名：去掉协议头与路径
        let dom = u
            .strip_prefix("https://")
            .or_else(|| u.strip_prefix("http://"))
            .unwrap_or(u);
        let dom = dom.split('/').next().unwrap_or(dom);
        // href 属性里需转义 & 和 "
        let attr = u.replace('&', "&amp;").replace('"', "&quot;");

        out.push_str(&format!(
            "<a href=\"{attr}\">🔗 {dom}</a><a href=\"copy:{attr}\"> ⧉</a>",
            attr = attr,
            dom = html_escape(dom)
        ));
        last = whole.end();
    }
    out.push_str(&process_text_segment(&plain[last..]));
    out
}

/// 尝试连接现有 socket 并请求状态；若得到有效响应说明已有守护进程在运行。
fn already_running() -> bool {
    if let Ok(mut s) = UnixStream::connect(socket_path()) {
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

fn run_account_loop(account: Account, state: Arc<RwLock<DaemonState>>, per_account_limit: usize) {
    let retry_delay = Duration::from_secs(10);
    loop {
        println!("[{}] 正在连接 {}:{}...", account.name, account.host, account.port);
        // 当前仅支持 TLS (port 993)，ssl 字段保留但不影响行为
        match connect_and_idle(&account, &state, per_account_limit) {
            Ok(_) => {
                println!("[{}] 连接正常关闭，10 秒后重连...", account.name);
                thread::sleep(retry_delay);
            }
            Err(e) => {
                let msg = e.to_string();
                eprintln!("[{}] 邮件检查出错: {}，10 秒后重试...", account.name, msg);
                set_account_error(&state, &account.name, msg);
                thread::sleep(retry_delay);
            }
        }
    }
}

fn format_address(addr: &Address) -> String {
    match addr {
        Address::List(list) => {
            list.iter()
                .map(|mb| {
                    let name = mb.name.as_ref().map(|n| n.as_ref()).unwrap_or("");
                    let email = mb.address.as_ref().map(|a| a.as_ref()).unwrap_or("");
                    if !name.is_empty() {
                        format!("{} <{}>", name, email)
                    } else {
                        email.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
        Address::Group(groups) => {
            groups.iter()
                .map(|g| {
                    let group_name = g.name.as_ref().map(|n| n.as_ref()).unwrap_or("Group");
                    let members = g.addresses.iter()
                        .map(|mb| {
                            let name = mb.name.as_ref().map(|n| n.as_ref()).unwrap_or("");
                            let email = mb.address.as_ref().map(|a| a.as_ref()).unwrap_or("");
                            if !name.is_empty() {
                                format!("{} <{}>", name, email)
                            } else {
                                email.to_string()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{}: [{}]", group_name, members)
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

/// 常见垃圾邮件文件夹候选名（不同服务商命名不同）
const SPAM_FOLDER_CANDIDATES: &[&str] = &[
    "[Gmail]/Spam",
    "[Google Mail]/Spam",
    "Junk",
    "Junk E-mail",
    "Junk Email",
    "Spam",
    "垃圾邮件",
    "Bulk Mail",
];

/// 探测该账户需要监视的文件夹：始终包含 INBOX，再加上垃圾邮件文件夹。
/// 优先通过 LIST 的 \Junk 特殊用途属性识别（兼容 Gmail 等各语言/各服务商命名），
/// 失败时回退到按常见名称探测。
fn detect_folders(session: &mut ImapSession) -> Vec<String> {
    let mut folders = vec!["INBOX".to_string()];

    // 方式一：LIST 全部文件夹，挑出带 \Junk 属性的
    if let Ok(names) = session.list(Some(""), Some("*")) {
        for n in names.iter() {
            let is_junk = n.attributes().iter().any(|a| match a {
                imap::types::NameAttribute::Custom(s) => s.as_ref().eq_ignore_ascii_case("\\Junk"),
                _ => false,
            });
            if is_junk && !folders.iter().any(|f| f == n.name()) {
                folders.push(n.name().to_string());
            }
        }
    }

    // 方式二（回退）：未识别到时按常见名称探测
    if folders.len() == 1 {
        for cand in SPAM_FOLDER_CANDIDATES {
            if session.examine(cand).is_ok() {
                folders.push((*cand).to_string());
                break;
            }
        }
    }

    folders
}

/// 文件夹中文标签（用于桌面通知）
fn folder_label(folder: &str) -> &'static str {
    if folder == "INBOX" {
        "收件箱"
    } else {
        "垃圾邮件"
    }
}

fn connect_and_idle(
    account: &Account,
    state: &Arc<RwLock<DaemonState>>,
    per_account_limit: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut imap_session = connect_session(account)?;

    // 连接并登录成功，清除该账户之前的错误状态
    clear_account_error(state, &account.name);

    let folders = detect_folders(&mut imap_session);
    println!("[{}] 监视文件夹: {:?}", account.name, folders);

    // 去重通知用：键为 "folder\u{1}uid"
    let mut known: HashSet<String> = HashSet::new();
    let mut is_first_sync = true;

    loop {
        let mut account_mails: Vec<MailInfo> = Vec::new();
        let mut current: HashSet<String> = HashSet::new();

        for folder in &folders {
            // 只读打开该文件夹（examine 不会标记已读）
            if imap_session.examine(folder).is_err() {
                continue;
            }
            // 取该文件夹最近的若干封邮件（已读 + 未读都要），按 UID 从大到小取前 per_account_limit 封
            let all_uids = match imap_session.uid_search("ALL") {
                Ok(set) => set,
                Err(_) => continue,
            };
            if all_uids.is_empty() {
                continue;
            }
            let mut uid_vec: Vec<u32> = all_uids.into_iter().collect();
            uid_vec.sort_unstable_by(|a, b| b.cmp(a));
            let take = if per_account_limit > 0 {
                std::cmp::min(per_account_limit, uid_vec.len())
            } else {
                uid_vec.len()
            };
            uid_vec.truncate(take);
            let uid_set = uid_vec
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(",");
            // FLAGS 取已读状态；RFC822.HEADER 取头部（examine 只读，不会标记已读）
            let fetches = imap_session.uid_fetch(uid_set, "(FLAGS RFC822.HEADER)")?;

            for fetch in fetches.iter() {
                let uid = match fetch.uid {
                    Some(u) => u,
                    None => continue,
                };
                let header_bytes = match fetch.header() {
                    Some(h) => h,
                    None => continue,
                };
                let parsed = match mail_parser::MessageParser::default().parse(header_bytes) {
                    Some(p) => p,
                    None => continue,
                };

                let seen = fetch
                    .flags()
                    .iter()
                    .any(|f| matches!(f, imap::types::Flag::Seen));
                let subject = parsed.subject().unwrap_or("No Subject").to_string();
                let from = parsed
                    .from()
                    .map(format_address)
                    .unwrap_or_else(|| "Unknown Sender".to_string());
                // 将邮件时间从发件人时区转换为本地时区再显示
                let date = parsed
                    .date()
                    .and_then(|d| Local.timestamp_opt(d.to_timestamp(), 0).single())
                    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| "Unknown Date".to_string());

                let key = format!("{}\u{1}{}", folder, uid);
                // known/current 仅跟踪“未读”键，用于新邮件通知去重
                if !seen {
                    current.insert(key.clone());
                }

                account_mails.push(MailInfo {
                    account: account.name.clone(),
                    folder: folder.clone(),
                    uid,
                    from: from.clone(),
                    subject: subject.clone(),
                    date,
                    seen,
                });

                // 仅对未读、且首次同步之后新出现的邮件发送桌面通知
                if !seen && !is_first_sync && !known.contains(&key) {
                    if let Err(e) = Notification::new()
                        .summary(&format!("新邮件 - {} · {}", account.name, folder_label(folder)))
                        .body(&format!("发件人: {}\n主题: {}", from, subject))
                        .timeout(10000)
                        .show()
                    {
                        eprintln!("发送桌面通知失败: {:?}", e);
                    }
                }
            }
        }

        // 该账户邮件按日期倒序，并应用「每账户上限」（0 表示不限制）
        account_mails.sort_by(|a, b| b.date.cmp(&a.date));
        if per_account_limit > 0 && account_mails.len() > per_account_limit {
            account_mails.truncate(per_account_limit);
        }

        // 更新全局状态
        {
            // RwLock poisoning 恢复
            let mut w_state = state.write().unwrap_or_else(|e| e.into_inner());
            w_state.unread_mails.retain(|m| m.account != account.name);
            w_state.unread_mails.extend(account_mails);
            // 按日期倒序
            w_state.unread_mails.sort_by(|a, b| b.date.cmp(&a.date));
            w_state.last_update = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        }
        // 状态已更新（可能有新邮件）→ 立即推送给 watch 订阅者，实现零延迟。
        notify_state_changed();

        known = current;
        is_first_sync = false;

        // 在 INBOX 上 IDLE；超时设短（5 分钟），以便周期性复查垃圾邮件等其他文件夹
        imap_session.examine("INBOX")?;
        println!("[{}] 进入 IDLE 等待新邮件...", account.name);
        let handle = imap_session.idle()?;
        handle.wait_with_timeout(Duration::from_secs(5 * 60))?;
        println!("[{}] IDLE 唤醒，正在检查更新...", account.name);
    }
}
