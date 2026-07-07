//! 客户端命令处理器：按需取正文、翻译、标记已读 / 批量已读。
//!
//! 每个处理器都是「查账户 → 连接 IMAP 执行操作 → 返回一行 JSON」。取正文/翻译共用
//! [`super::imap_sync::fetch_raw_message`]，翻译模型与译文缓存是进程级懒加载单例。

use super::imap_sync::{connect_session, fetch_raw_message, format_address};
use super::state::{notify_state_changed, DaemonState, MailInfo};
use crate::config::Account;
use crate::mailhtml::{body_to_html, extract_body};
use crate::translate;
use chrono::{Local, TimeZone};
use std::fs;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

/// 翻译功能的共享单例：懒加载的模型管理器（含空闲卸载）。
fn model_manager() -> &'static Arc<translate::ModelManager> {
    static MM: OnceLock<Arc<translate::ModelManager>> = OnceLock::new();
    MM.get_or_init(|| {
        let mm = Arc::new(translate::ModelManager::new());
        mm.start_idle_unloader();
        mm
    })
}

/// 译文内存缓存单例。
fn trans_cache() -> &'static translate::TransCache {
    static TC: OnceLock<translate::TransCache> = OnceLock::new();
    TC.get_or_init(|| translate::TransCache::new(200))
}

fn json_err(msg: &str) -> String {
    serde_json::json!({ "ok": false, "error": msg }).to_string()
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
        // v3：正文提取加入占位符→HTML 回退 + 实体解码，换文件名让旧缓存(含只存了
        // 占位符 "Plain text version not available" 的那些)自然失效重取
        .join(format!("{}.v3.json", uid))
}

/// 按需获取某封邮件的正文（只读打开，不会标记已读）；优先读磁盘缓存
pub fn fetch_body(
    accounts: &[Account],
    cache_dir: &str,
    body_cache_limit: usize,
    io_timeout: Duration,
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
        let msg = fetch_raw_message(account, folder, uid, io_timeout)?;

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
            // 3. 淘汰过旧缓存，防止缓存目录无上限增长（0 = 不限制）
            if body_cache_limit > 0 {
                prune_body_cache(cache_dir, body_cache_limit);
            }
            json
        }
        Err(e) => json_err(&e.to_string()),
    }
}

/// 把 `cache_dir` 下的正文缓存文件数裁到 `limit` 以内：递归收集所有 `.json` 文件，
/// 若超限则按修改时间从旧到新删除，直到不超过上限。best-effort，任何 IO 错误忽略。
fn prune_body_cache(cache_dir: &str, limit: usize) {
    fn collect(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, std::time::SystemTime)>) {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, out);
            } else if path.extension().is_some_and(|e| e == "json") {
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                out.push((path, mtime));
            }
        }
    }

    let mut files: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    collect(std::path::Path::new(cache_dir), &mut files);
    if files.len() <= limit {
        return;
    }
    // 按修改时间升序（最旧在前），删掉超出上限的最旧那批
    files.sort_by_key(|(_, mtime)| *mtime);
    let remove_count = files.len() - limit;
    for (path, _) in files.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
}

/// 取信→提取纯文本→(auto 检测源语言)→查缓存→翻译散文(保留 URL)→body_to_html→缓存。
#[allow(clippy::too_many_arguments)]
pub fn fetch_translation(
    accounts: &[Account],
    io_timeout: Duration,
    account_name: &str,
    folder: &str,
    uid: &str,
    src_req: &str,
    tgt: &str,
    engine: &str,
    deeplx_url: &str,
) -> String {
    let account = match accounts.iter().find(|a| a.name == account_name) {
        Some(a) => a,
        None => return json_err("账户不存在"),
    };
    // 缓存键含 engine + 请求值 src_req，命中检查在联网取信**之前**完成——同一封邮件、
    // 同样的引擎/语言，译文缓存命中即直接返回，省掉每次约 2s 的 IMAP 原文重取。
    let key: translate::TransKey = (
        account_name.to_string(),
        folder.to_string(),
        uid.parse::<u32>().unwrap_or(0),
        engine.to_string(),
        src_req.to_string(),
        tgt.to_string(),
    );
    if let Some(html) = trans_cache().get(&key) {
        return serde_json::json!({ "ok": true, "body": html }).to_string();
    }
    let run = || -> Result<String, Box<dyn std::error::Error>> {
        let msg = fetch_raw_message(account, folder, uid, io_timeout)?;
        let plain = extract_body(&msg);
        // 前置判断：正文为空/纯空白时无需翻译，直接返回。否则本地 NLLB 引擎会为“空”
        // 触发首次约 600MB 的模型下载、在线引擎会发无谓请求。
        if plain.trim().is_empty() {
            let html = body_to_html(&plain);
            trans_cache().put(key.clone(), html.clone());
            return Ok(serde_json::json!({ "ok": true, "body": html }).to_string());
        }
        // 按引擎翻译正文（返回纯文本；URL/验证码在下面 body_to_html 里重新 linkify）。
        let translated_plain = match engine {
            "google" => translate::translate_google(&plain, tgt)?,
            "deeplx" => translate::translate_deeplx(deeplx_url, &plain, tgt)?,
            // 默认本地 NLLB（离线）
            _ => {
                let src = translate::resolve_source_lang(src_req, &plain);
                // 源语言==目标语言（如中文邮件译中文）→ 直接返回原文，省下无谓且慢的推理
                if src == tgt {
                    let html = body_to_html(&plain);
                    trans_cache().put(key.clone(), html.clone());
                    return Ok(serde_json::json!({ "ok": true, "body": html }).to_string());
                }
                model_manager()
                    .with_translator(|t| translate::translate_prose(t, &plain, &src, tgt))
                    .map_err(|e| e.to_string())??
            }
        };
        let html = body_to_html(&translated_plain);
        trans_cache().put(key.clone(), html.clone());
        Ok(serde_json::json!({ "ok": true, "body": html }).to_string())
    };
    run().unwrap_or_else(|e| json_err(&e.to_string()))
}

/// 标记某封邮件为已读（设置 \Seen 标志），并从守护进程状态中立即移除该邮件
pub fn mark_read(
    accounts: &[Account],
    state: &Arc<RwLock<DaemonState>>,
    io_timeout: Duration,
    account_name: &str,
    folder: &str,
    uid: &str,
) -> String {
    let account = match accounts.iter().find(|a| a.name == account_name) {
        Some(a) => a,
        None => return json_err("账户不存在"),
    };

    let run = || -> Result<String, Box<dyn std::error::Error>> {
        let (mut session, _ctl) = connect_session(account, io_timeout)?;
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
pub fn mark_read_all(
    accounts: &[Account],
    state: &Arc<RwLock<DaemonState>>,
    io_timeout: Duration,
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
            let (mut session, _ctl) = connect_session(account, io_timeout)?;
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
