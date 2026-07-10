use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    pub password: String,
    #[serde(default = "default_true")]
    pub ssl: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_port() -> u16 {
    993
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 缓存的未读邮件总数上限（0 表示不限制）；在已启用账户间平均分配
    #[serde(default = "default_cache_limit")]
    pub cache_limit: usize,
    /// 邮件正文磁盘缓存目录
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
    /// IMAP 读/写超时（秒）。稳态 fetch（取头部/正文/标记已读）超过此值即报错重连，
    /// 避免半死的 TCP（NAT/代理静默丢连）把账户线程或取信请求永久卡住。
    /// 不影响 IDLE 的长等待（IDLE 有独立的 poll_interval_secs 超时）。最小 5 秒。
    #[serde(default = "default_imap_timeout_secs")]
    pub imap_timeout_secs: u64,
    /// IDLE 轮询间隔（秒）。每挂 IDLE 至多这么久就主动 DONE 并重新拉取一遍，然后再进
    /// IDLE。它同时起两个作用：(1) 兜底——某些服务商（如 QQ 邮箱）IDLE 根本不推送
    /// EXISTS，或经代理/NAT 的长连接会被静默回收/黑洞化，此时新邮件只能靠这次周期性重
    /// 拉发现；(2) 保活——周期性的小流量让被代理/NAT 判定为空闲的连接不至于被中途掐断。
    /// 因此新邮件的最坏延迟 ≈ 本值。取值区间 [15, 1740]（上限遵循 RFC 2177 的 29 分钟）。
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// 正文磁盘缓存的最多文件数（0 表示不限制）。超过后按修改时间淘汰最旧的，
    /// 防止缓存目录无上限增长。
    #[serde(default = "default_body_cache_limit")]
    pub body_cache_limit: usize,
    pub accounts: Vec<Account>,
}

fn default_cache_limit() -> usize {
    50
}

fn default_imap_timeout_secs() -> u64 {
    60
}

fn default_poll_interval_secs() -> u64 {
    45
}

fn default_body_cache_limit() -> usize {
    500
}

pub fn default_cache_dir() -> String {
    dirs::cache_dir()
        .map(|p| p.join("dms-email-client"))
        .unwrap_or_else(|| std::env::temp_dir().join("dms-email-client-cache"))
        .to_string_lossy()
        .into_owned()
}

impl Config {
    pub fn get_path() -> PathBuf {
        let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        path.push("dms-email-client");
        path.push("config.toml");
        path
    }

    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let path = Self::get_path();
        if !path.exists() {
            // 创建默认配置文件（如果不存在）
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let default_config = Config {
                cache_limit: default_cache_limit(),
                cache_dir: default_cache_dir(),
                imap_timeout_secs: default_imap_timeout_secs(),
                poll_interval_secs: default_poll_interval_secs(),
                body_cache_limit: default_body_cache_limit(),
                accounts: vec![Account {
                    name: "Example QQ".to_string(),
                    host: "imap.qq.com".to_string(),
                    port: 993,
                    username: "your_email@qq.com".to_string(),
                    password: "your_auth_code".to_string(),
                    ssl: true,
                    // 示例账户默认禁用，避免守护进程用占位凭据反复连接报错
                    enabled: false,
                }],
            };
            let toml_str = toml::to_string_pretty(&default_config)?;
            fs::write(&path, toml_str)?;
            // 消息写入 stderr，保持 stdout 纯净（`config show` 的输出需为合法 JSON）
            eprintln!("已创建默认配置文件: {:?}", path);
            eprintln!("请编辑配置文件或在设置面板中添加账户。");
            return Ok(default_config);
        }

        let content = fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// 将 Config 序列化为 TOML 并写入配置文件
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::get_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let toml_str = toml::to_string_pretty(self)?;
        fs::write(&path, toml_str)?;
        Ok(())
    }

    /// 从 JSON 字符串反序列化为 Config
    pub fn from_json(json_str: &str) -> Result<Config, Box<dyn std::error::Error>> {
        let config: Config = serde_json::from_str(json_str)?;
        Ok(config)
    }

    /// 将 Config 序列化为 JSON 字符串
    pub fn to_json(&self) -> Result<String, Box<dyn std::error::Error>> {
        let json = serde_json::to_string_pretty(self)?;
        Ok(json)
    }
}
