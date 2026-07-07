//! 守护进程共享状态：未读邮件快照 + 各账户错误，以及「状态变更广播」。
//!
//! 任何修改 [`DaemonState`] 的地方都调用 [`notify_state_changed`]，`watch` 长连接会
//! 被立即唤醒并推送最新状态，从而做到「新邮件到达 / 标记已读」零延迟反映到前端，
//! 无需前端轮询。

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};

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
pub fn set_account_error(state: &Arc<RwLock<DaemonState>>, account: &str, message: String) {
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
pub fn clear_account_error(state: &Arc<RwLock<DaemonState>>, account: &str) {
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

/// 状态变更广播：单例的「版本号 + 条件变量」。任何修改 [`DaemonState`] 的地方都调用
/// [`notify_state_changed`]，`watch` 长连接会被立即唤醒并推送最新状态。
pub struct Notifier {
    pub version: Mutex<u64>,
    pub cv: Condvar,
}

static NOTIFIER: OnceLock<Notifier> = OnceLock::new();

pub fn notifier() -> &'static Notifier {
    NOTIFIER.get_or_init(|| Notifier {
        version: Mutex::new(0),
        cv: Condvar::new(),
    })
}

/// 通知所有 `watch` 订阅者：状态已变化，请重新推送。
pub fn notify_state_changed() {
    let n = notifier();
    let mut v = n.version.lock().unwrap_or_else(|e| e.into_inner());
    *v = v.wrapping_add(1);
    n.cv.notify_all();
}
