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
    pub accounts: Vec<Account>,
}

fn default_cache_limit() -> usize {
    50
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
