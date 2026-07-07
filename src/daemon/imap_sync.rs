//! IMAP 同步引擎：建连/登录、每账户的检查循环 + IDLE、文件夹探测、增量头部缓存。
//!
//! 每个已启用账户跑一个 [`run_account_loop`] 线程：连接 → 探测收件箱/垃圾箱 → 循环
//! （拉取头部、更新状态、发通知、IDLE 等待）。断线由外层循环 10 秒重连。

use super::state::{
    clear_account_error, notify_state_changed, set_account_error, DaemonState, MailInfo,
};
use crate::config::Account;
use crate::sysmem::release_free_memory;
use chrono::{Local, TimeZone};
use mail_parser::Address;
use native_tls::TlsConnector;
use notify_rust::Notification;
use std::collections::{HashMap, HashSet};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

/// IMAP over TLS 会话类型别名
pub type ImapSession = imap::Session<native_tls::TlsStream<std::net::TcpStream>>;

/// 建立一个 IMAP over TLS 会话并登录。
///
/// 注意：`imap::connect` 内部用 `TcpStream::connect` 且不设任何超时，登录前读取
/// 服务器 greeting 也是无超时阻塞。当守护进程在网络/代理尚未就绪时启动（例如登录
/// 会话刚把插件拉起、TUN 代理还没接管），TCP 可能连上代理但 greeting 永远不到，
/// 线程就永久卡在 `read_greeting()`：既不返回 Ok（状态永远为空），也不返回 Err
/// （`run_account_loop` 的 10 秒重连循环不触发），前端因此一直空白且无报错。
/// 所以这里手动建连，为「TCP 连接 + TLS 握手 + greeting + 登录」设置超时。
///
/// 登录成功后**保留** `io_timeout` 作为稳态读/写超时（不再清成 None）：这样后续的
/// fetch（取头部/正文/标记已读）在半死 TCP 下会超时报错而非永久阻塞。IDLE 的长等待
/// 由调用方在 idle 前后自行管理超时（见 [`connect_and_idle`]）。
///
/// 返回会话 + 一个 dup 出来的 [`TcpStream`] 控制句柄：SO_RCVTIMEO 在 dup 的 fd 间共享，
/// 调用方用它在「fetch 段」与「IDLE 段」之间切换读超时，无需触碰 TLS 会话内部。
pub fn connect_session(
    account: &Account,
    io_timeout: Duration,
) -> Result<(ImapSession, TcpStream), Box<dyn std::error::Error>> {
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
    // 登录阶段用较宽松的固定超时（慢网络下别把握手/登录误杀），登录成功后再切到 io_timeout
    const LOGIN_IO_TIMEOUT: Duration = Duration::from_secs(30);

    let tls = TlsConnector::builder().build()?;

    // 解析地址并带超时建立 TCP 连接，避免半就绪的代理/网络下永久阻塞
    let addr = (account.host.as_str(), account.port)
        .to_socket_addrs()?
        .next()
        .ok_or("无法解析 IMAP 服务器地址")?;
    let tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    tcp.set_read_timeout(Some(LOGIN_IO_TIMEOUT))?;
    tcp.set_write_timeout(Some(LOGIN_IO_TIMEOUT))?;
    // 同一 socket 的副本：登录后用它切换稳态超时（SO_RCVTIMEO 在 dup 的 fd 间共享）
    let tcp_ctl = tcp.try_clone()?;

    // TLS 握手 + 读取 greeting + 登录，均受上面的读写超时约束
    let tls_stream = tls.connect(&account.host, tcp)?;
    let mut client = imap::Client::new(tls_stream);
    client.read_greeting()?;
    let session = client
        .login(&account.username, &account.password)
        .map_err(|e| e.0)?;

    // 登录成功：切到稳态 io_timeout，保护后续所有 fetch 不被半死 TCP 永久阻塞
    tcp_ctl.set_read_timeout(Some(io_timeout))?;
    tcp_ctl.set_write_timeout(Some(io_timeout))?;

    Ok((session, tcp_ctl))
}

/// 连接账户、只读打开文件夹、按 UID 取原始邮件并解析。供 fetch_body 与 translate 复用。
pub fn fetch_raw_message(
    account: &Account,
    folder: &str,
    uid: &str,
    io_timeout: Duration,
) -> Result<mail_parser::Message<'static>, Box<dyn std::error::Error>> {
    // io_timeout 已由 connect_session 设为稳态读/写超时，下面的 examine/uid_fetch 均受其保护
    let (mut session, _ctl) = connect_session(account, io_timeout)?;
    session.examine(folder)?;
    let fetches = session.uid_fetch(uid, "BODY.PEEK[]")?;
    let fetch = fetches.iter().next().ok_or("未找到该邮件")?;
    let raw = fetch.body().ok_or("邮件无正文数据")?.to_vec();
    let _ = session.logout();
    let msg = mail_parser::MessageParser::default()
        .parse(&raw)
        .ok_or("邮件解析失败")?;
    // 解析借用 raw；转为 owned 以便跨函数返回
    Ok(msg.into_owned())
}

pub fn format_address(addr: &Address) -> String {
    match addr {
        Address::List(list) => list
            .iter()
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
            .join(", "),
        Address::Group(groups) => groups
            .iter()
            .map(|g| {
                let group_name = g.name.as_ref().map(|n| n.as_ref()).unwrap_or("Group");
                let members = g
                    .addresses
                    .iter()
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
            .join(", "),
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

pub fn run_account_loop(
    account: Account,
    state: Arc<RwLock<DaemonState>>,
    per_account_limit: usize,
    io_timeout: Duration,
) {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let retry_delay = Duration::from_secs(10);
    loop {
        println!("[{}] 正在连接 {}:{}...", account.name, account.host, account.port);
        // 当前仅支持 TLS (port 993)，ssl 字段保留但不影响行为。
        // catch_unwind 兜底：一旦 connect_and_idle 内部 panic（而非返回 Err），
        // 也把该账户线程救回来重连，而不是让线程静默死掉、此后永不刷新。
        let outcome =
            catch_unwind(AssertUnwindSafe(|| connect_and_idle(&account, &state, per_account_limit, io_timeout)));
        match outcome {
            Ok(Ok(())) => {
                println!("[{}] 连接正常关闭，10 秒后重连...", account.name);
                thread::sleep(retry_delay);
            }
            Ok(Err(e)) => {
                let msg = e.to_string();
                eprintln!("[{}] 邮件检查出错: {}，10 秒后重试...", account.name, msg);
                set_account_error(&state, &account.name, msg);
                thread::sleep(retry_delay);
            }
            Err(_) => {
                let msg = "内部错误(panic)，已自动重启该账户".to_string();
                eprintln!("[{}] {}", account.name, msg);
                set_account_error(&state, &account.name, msg);
                thread::sleep(retry_delay);
            }
        }
    }
}

fn connect_and_idle(
    account: &Account,
    state: &Arc<RwLock<DaemonState>>,
    per_account_limit: usize,
    io_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut imap_session, tcp_ctl) = connect_session(account, io_timeout)?;

    // 连接并登录成功，清除该账户之前的错误状态
    clear_account_error(state, &account.name);

    let folders = detect_folders(&mut imap_session);
    println!("[{}] 监视文件夹: {:?}", account.name, folders);

    // 去重通知用：键为 "folder\u{1}uid"
    let mut known: HashSet<String> = HashSet::new();
    let mut is_first_sync = true;

    // 增量同步缓存：按 (folder, uid) 缓存已解析的**不可变**头部字段 (from, subject,
    // date)。每轮只对新 UID 下载+解析头部；已知 UID 只取 FLAGS 更新已读状态。
    // UIDVALIDITY 变化则清掉该文件夹缓存（UID 语义已失效，必须重取）。
    let mut hdr_cache: HashMap<(String, u32), (String, String, String)> = HashMap::new();
    let mut uidvalidity: HashMap<String, u32> = HashMap::new();

    loop {
        // 每轮 fetch 前把读超时复位到 io_timeout：上一轮的 IDLE(wait_with_timeout) 会把
        // socket 读超时改成 5 分钟，这里复位以确保 examine/uid_fetch 受 io_timeout 保护。
        tcp_ctl.set_read_timeout(Some(io_timeout))?;

        let mut account_mails: Vec<MailInfo> = Vec::new();
        let mut current: HashSet<String> = HashSet::new();

        for folder in &folders {
            // 只读打开该文件夹（examine 不会标记已读），并读取 UIDVALIDITY
            let mailbox = match imap_session.examine(folder) {
                Ok(mb) => mb,
                Err(_) => continue,
            };
            // UIDVALIDITY 变化 → 该文件夹的 UID 缓存全部失效，必须重取头部
            if let Some(uv) = mailbox.uid_validity {
                if uidvalidity.get(folder) != Some(&uv) {
                    hdr_cache.retain(|(f, _), _| f != folder);
                    uidvalidity.insert(folder.clone(), uv);
                }
            }

            // 取该文件夹最近的若干封邮件（已读 + 未读都要），按 UID 从大到小取前 per_account_limit 封
            let all_uids = match imap_session.uid_search("ALL") {
                Ok(set) => set,
                Err(_) => continue,
            };
            if all_uids.is_empty() {
                // 文件夹已空 → 清掉其缓存
                hdr_cache.retain(|(f, _), _| f != folder);
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

            // 淘汰：窗口滑动 / 已删除 —— 移除不在当前 top-N 里的该文件夹缓存项
            let keep: HashSet<u32> = uid_vec.iter().copied().collect();
            hdr_cache.retain(|(f, u), _| f != folder || keep.contains(u));

            // 本轮各 UID 的已读状态（uid -> seen）
            let mut seen_map: HashMap<u32, bool> = HashMap::new();

            // 1) 新 UID（缓存未命中）：一次取 (FLAGS RFC822.HEADER)，解析头部入缓存
            let new_uids: Vec<u32> = uid_vec
                .iter()
                .copied()
                .filter(|u| !hdr_cache.contains_key(&(folder.clone(), *u)))
                .collect();
            if !new_uids.is_empty() {
                let set = new_uids
                    .iter()
                    .map(|u| u.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let fetches = imap_session.uid_fetch(set, "(FLAGS RFC822.HEADER)")?;
                for fetch in fetches.iter() {
                    let uid = match fetch.uid {
                        Some(u) => u,
                        None => continue,
                    };
                    let seen = fetch
                        .flags()
                        .iter()
                        .any(|f| matches!(f, imap::types::Flag::Seen));
                    seen_map.insert(uid, seen);
                    let header_bytes = match fetch.header() {
                        Some(h) => h,
                        None => continue,
                    };
                    let parsed = match mail_parser::MessageParser::default().parse(header_bytes) {
                        Some(p) => p,
                        None => continue,
                    };
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
                    hdr_cache.insert((folder.clone(), uid), (from, subject, date));
                }
            }

            // 2) 已知 UID：只取 FLAGS 更新已读状态（省去重复下载/解析头部）
            let known_uids: Vec<u32> = uid_vec
                .iter()
                .copied()
                .filter(|u| !seen_map.contains_key(u))
                .collect();
            if !known_uids.is_empty() {
                let set = known_uids
                    .iter()
                    .map(|u| u.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let fetches = imap_session.uid_fetch(set, "FLAGS")?;
                for fetch in fetches.iter() {
                    if let Some(uid) = fetch.uid {
                        let seen = fetch
                            .flags()
                            .iter()
                            .any(|f| matches!(f, imap::types::Flag::Seen));
                        seen_map.insert(uid, seen);
                    }
                }
            }

            // 3) 用缓存头部 + 本轮 FLAGS 组装 MailInfo（按 uid_vec 顺序；任一缺失则跳过）
            for uid in &uid_vec {
                let (from, subject, date) = match hdr_cache.get(&(folder.clone(), *uid)) {
                    Some(v) => v.clone(),
                    None => continue, // 头部取回失败（例如取回途中被删）
                };
                let seen = match seen_map.get(uid) {
                    Some(s) => *s,
                    None => continue, // FLAGS 未取到
                };

                let key = format!("{}\u{1}{}", folder, uid);
                // known/current 仅跟踪“未读”键，用于新邮件通知去重
                if !seen {
                    current.insert(key.clone());
                }

                account_mails.push(MailInfo {
                    account: account.name.clone(),
                    folder: folder.clone(),
                    uid: *uid,
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
        // 本轮同步可能下载/解析了一批头部，尖峰后把空闲堆页还给 OS
        release_free_memory();

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
