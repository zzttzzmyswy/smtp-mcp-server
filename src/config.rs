use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub auth: Auth,
    pub smtp: Smtp,
    #[serde(default)]
    pub security: Security,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    #[serde(default = "default_addr")]
    pub addr: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_addr() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8202
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    #[serde(alias = "key")]
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Smtp {
    pub host: String,
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    /// ssl (implicit TLS, 465) | starttls (587) | none
    #[serde(default = "default_tls")]
    pub tls: String,
    pub from: String,
    pub user: String,
    /// SMTP 授权码；支持 ${ENV_VAR} 占位符，缺省时回退读取 SMTP_PASS 环境变量
    pub pass: Option<String>,
}

fn default_smtp_port() -> u16 {
    465
}

fn default_tls() -> String {
    "ssl".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    /// 收件人白名单；空 = 不限
    #[serde(default)]
    pub receiver_whitelist: Vec<String>,
    /// 单个附件最大字节数（默认 10MB）
    #[serde(default = "default_max_attachment_bytes")]
    pub max_attachment_bytes: usize,
    /// 单次请求所有附件总字节上限（默认 10MB）
    #[serde(default = "default_max_attachment_bytes")]
    pub max_total_attachment_bytes: usize,
    /// 单个 JSON 请求体上限（遵循 MCP 后段，宽松于附件上限以容纳 base64 开销）
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    /// 附件扩展名白名单（小写，不带点）；空 = 不限
    #[serde(default)]
    pub allowed_attachment_extensions: Vec<String>,
}

impl Default for Security {
    fn default() -> Self {
        Security {
            receiver_whitelist: Vec::new(),
            max_attachment_bytes: default_max_attachment_bytes(),
            max_total_attachment_bytes: default_max_attachment_bytes(),
            max_request_bytes: default_max_request_bytes(),
            allowed_attachment_extensions: Vec::new(),
        }
    }
}

fn default_max_attachment_bytes() -> usize {
    10 * 1024 * 1024
}

fn default_max_request_bytes() -> usize {
    14 * 1024 * 1024
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: String,
    pub key: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("读取配置文件 {} 失败: {}", path.display(), e))?;
        Self::parse(&raw).map_err(|e| format!("解析配置文件 {} 失败: {}", path.display(), e))
    }

    pub fn parse(raw: &str) -> Result<Config, String> {
        let mut cfg: Config = toml::from_str(raw).map_err(|e| format!("TOML 解析失败: {}", e))?;

        if cfg.auth.keys.is_empty() {
            return Err("auth.keys 不能为空，至少配置一个访问密钥".into());
        }
        if cfg.auth.keys.iter().any(|k| k.trim().is_empty()) {
            return Err("auth.keys 存在空密钥".into());
        }
        if cfg.smtp.host.trim().is_empty() || cfg.smtp.user.trim().is_empty() {
            return Err("smtp.host / smtp.user 不能为空".into());
        }
        match cfg.smtp.tls.as_str() {
            "ssl" | "starttls" | "none" => {}
            other => {
                return Err(format!(
                    "smtp.tls 取值非法: '{}'，支持 ssl|starttls|none",
                    other
                ))
            }
        }
        // 解析 SMTP 授权码：支持 ${ENV} 占位符；未填写时回退 SMTP_PASS
        cfg.smtp.pass = Some(cfg.resolve_secret(cfg.smtp.pass.as_deref(), "SMTP_PASS")?);

        Ok(cfg)
    }

    /// 解析 ${ENV_VAR} 占位符；无占位符时原样返回；返回 None 时回退到环境变量
    fn resolve_secret(&self, value: Option<&str>, fallback_env: &str) -> Result<String, String> {
        if let Some(v) = value {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return self.read_env(fallback_env);
            }
            if let Some(rest) = trimmed.strip_prefix("${") {
                if let Some(name) = rest.strip_suffix('}') {
                    return self.read_env(name);
                }
            }
            return Ok(trimmed.to_string());
        }
        self.read_env(fallback_env)
    }

    fn read_env(&self, name: &str) -> Result<String, String> {
        std::env::var(name)
            .map_err(|_| format!("缺少 SMTP 授权码：请设置环境变量 {} 或 smtp.pass", name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> &'static str {
        r#"
[server]
addr = "0.0.0.0"
port = 8202

[auth]
keys = ["key1", "key2"]

[smtp]
host = "smtp.126.com"
port = 465
tls = "ssl"
from = "test@126.com"
user = "test@126.com"
pass = "placeholder"
"#
    }

    #[test]
    fn parse_valid_config() {
        let cfg = Config::parse(base_config()).unwrap();
        assert_eq!(cfg.server.addr, "0.0.0.0");
        assert_eq!(cfg.server.port, 8202);
        assert_eq!(cfg.auth.keys, vec!["key1", "key2"]);
        assert_eq!(cfg.smtp.host, "smtp.126.com");
        assert_eq!(cfg.smtp.pass.as_deref(), Some("placeholder"));
        assert_eq!(cfg.security.max_attachment_bytes, 10 * 1024 * 1024);
    }

    #[test]
    fn parse_missing_keys_rejected() {
        let raw = base_config().replace("keys = [\"key1\", \"key2\"]", "keys = []");
        let err = Config::parse(&raw).unwrap_err();
        assert!(err.contains("auth.keys"), "err: {}", err);
    }

    #[test]
    fn parse_bad_tls_mode_rejected() {
        let raw = base_config().replace("tls = \"ssl\"", "tls = \"weird\"");
        let err = Config::parse(&raw).unwrap_err();
        assert!(err.contains("smtp.tls"), "err: {}", err);
    }

    #[test]
    fn parse_unknown_field_rejected() {
        let raw = format!("{}\n[security]\nbogus = 1\n", base_config());
        assert!(Config::parse(&raw).is_err());
    }

    #[test]
    fn env_interpolation() {
        std::env::set_var("TEST_SMTP_PASS", "secret-env");
        let raw = r#"
[server]
[auth]
keys = ["k"]
[smtp]
host = "smtp.126.com"
from = "u@126.com"
user = "u@126.com"
pass = "${TEST_SMTP_PASS}"
"#;
        let cfg = Config::parse(raw).unwrap();
        assert_eq!(cfg.smtp.pass.as_deref(), Some("secret-env"));
        std::env::remove_var("TEST_SMTP_PASS");
    }

    #[test]
    fn smtp_pass_empty_falls_back_to_env() {
        std::env::set_var("SMTP_PASS", "env-pass");
        let raw = r#"
[server]
[auth]
keys = ["k"]
[smtp]
host = "smtp.126.com"
from = "u@126.com"
user = "u@126.com"
"#;
        let cfg = Config::parse(raw).unwrap();
        assert_eq!(cfg.smtp.pass.as_deref(), Some("env-pass"));
        std::env::remove_var("SMTP_PASS");
    }

    #[test]
    fn security_defaults_empty_whitelist() {
        let cfg = Config::parse(base_config()).unwrap();
        assert!(cfg.security.receiver_whitelist.is_empty());
        assert!(cfg.security.allowed_attachment_extensions.is_empty());
    }
}
