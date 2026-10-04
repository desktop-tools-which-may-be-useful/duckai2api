//! 配置装配：`.env` 文件 + 环境变量 → [`ServerConfig`]。
//!
//! `.env.example` 的 14 个键全部有消费点（见各字段文档）。优先级：
//! 真实环境变量 > `.env` 文件 > 默认值。解析与校验是纯函数（[`ServerConfig::from_lookup`]），
//! 测试注入查表闭包即可，不改进程环境。

use std::collections::HashMap;
use std::net::IpAddr;

use duckai_upstream::{UpstreamMode, parse_proxy_config};

/// 非回环绑定必须带 API key 的失败提示（fail-fast，禁止裸奔上线）。
const KEYLESS_BIND_MSG: &str = "启动拒绝：非回环绑定必须设置 DUCKAI_API_KEY（否则 /v1 无鉴权裸奔）";

#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    /// `DUCKAI_BASE`（默认 https://duck.ai，尾斜杠已修剪）。
    pub base: String,
    /// `DUCKAI_UPSTREAM`：auto | http | browser（默认 auto = 纯 HTTP 主路径）。
    pub upstream_mode: String,
    /// `DUCKAI_VQD_OVERRIDE`（空 = 不注入）。
    pub vqd_override: Option<String>,
    /// `DUCKAI_MODEL`（缺省 model 的回落值）。
    pub default_model: String,
    /// `DUCKAI_NEW_CHAT`：true 时会话不粘。
    pub new_chat: bool,
    /// `DUCKAI_PROXIES` + `DUCKAI_PROXY` 合并去重校验后的最终列表（空 = 直连）。
    pub proxies: Vec<String>,
    /// `DUCKAI_API_KEY`（空 = 未启用；非回环绑定时触发 fail-fast）。
    pub api_key: Option<String>,
    /// `DUCKAI_BIND`（默认 127.0.0.1）。
    pub bind: String,
    /// `PORT`（默认 8080）。
    pub port: u16,
    /// `DUCKAI_MAX_CONCURRENCY`（默认 8，≥1）。
    pub max_concurrency: usize,
    /// `DUCKAI_ADMIN_PASSWORD`（空 = WebUI 回环只读、写 403）。
    pub admin_password: Option<String>,
    /// `DUCKAI_CHROME_PATH`（browser 模式可执行路径）。
    pub chrome_path: Option<String>,
    /// `RUST_LOG`（默认 info；main 交给 EnvFilter）。
    pub log_filter: String,
}

impl ServerConfig {
    /// 从进程环境 + 工作目录 `.env` 装配（`.env` 缺失/损坏行被忽略）。
    pub fn from_env() -> Result<Self, String> {
        let dot = load_dotenv(".env");
        let get = move |k: &str| -> Option<String> {
            std::env::var(k)
                .ok()
                .or_else(|| dot.get(k).cloned())
                .filter(|v| !v.trim().is_empty())
        };
        Self::from_lookup(&get)
    }

    /// 注入查表的解析入口（测试与 from_env 共用）；空白值按未设置处理。
    pub fn from_lookup<F: Fn(&str) -> Option<String>>(get_in: &F) -> Result<Self, String> {
        let get = |k: &str| get_in(k).filter(|v| !v.trim().is_empty());
        let base = get("DUCKAI_BASE")
            .unwrap_or_else(|| "https://duck.ai".to_string())
            .trim()
            .trim_end_matches('/')
            .to_string();

        let upstream_mode = get("DUCKAI_UPSTREAM").unwrap_or_else(|| "auto".to_string());
        UpstreamMode::parse(&upstream_mode)?; // fail-fast：未知模式不启动

        let default_model = get("DUCKAI_MODEL").unwrap_or_else(|| "gpt-5.6-luna".to_string());

        let new_chat_raw = get("DUCKAI_NEW_CHAT");
        let new_chat = match new_chat_raw.as_deref() {
            None => false,
            Some(v) => match v.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                other => return Err(format!("DUCKAI_NEW_CHAT 需为 true/false，收到 {other:?}")),
            },
        };

        let proxies = parse_proxy_config(
            get("DUCKAI_PROXIES").as_deref(),
            get("DUCKAI_PROXY").as_deref(),
        )
        .map_err(|e| format!("代理配置无效：{e}"))?;

        let api_key = get("DUCKAI_API_KEY");

        let bind = get("DUCKAI_BIND").unwrap_or_else(|| "127.0.0.1".to_string());
        let port = match get("PORT") {
            Some(raw) => raw
                .parse::<u16>()
                .map_err(|_| format!("PORT 需为 1~65535 的端口号，收到 {raw:?}"))?,
            None => 8080,
        };

        let max_concurrency = match get("DUCKAI_MAX_CONCURRENCY") {
            Some(raw) => {
                let n: usize = raw
                    .parse()
                    .map_err(|_| format!("DUCKAI_MAX_CONCURRENCY 需为正整数，收到 {raw:?}"))?;
                if n == 0 {
                    return Err("DUCKAI_MAX_CONCURRENCY 必须 ≥1".to_string());
                }
                n
            }
            None => 8,
        };

        let admin_password = get("DUCKAI_ADMIN_PASSWORD");
        let chrome_path = get("DUCKAI_CHROME_PATH");
        let log_filter = get("RUST_LOG").unwrap_or_else(|| "info".to_string());

        // fail-fast：非回环绑定 + 无 API key → 拒绝启动。
        if api_key.is_none() && !is_loopback(&bind) {
            return Err(format!("{KEYLESS_BIND_MSG}（bind={bind}）"));
        }

        Ok(Self {
            base,
            upstream_mode,
            vqd_override: get("DUCKAI_VQD_OVERRIDE"),
            default_model,
            new_chat,
            proxies,
            api_key,
            bind,
            port,
            max_concurrency,
            admin_password,
            chrome_path,
            log_filter,
        })
    }

    /// 上游工厂配置（模式在此最终解析；factory 不读环境）。
    pub fn factory(&self) -> Result<duckai_upstream::FactoryConfig, String> {
        Ok(duckai_upstream::FactoryConfig {
            base: self.base.clone(),
            mode: UpstreamMode::parse(&self.upstream_mode)?,
            vqd_override: self.vqd_override.clone(),
            new_chat: self.new_chat,
            proxies: self.proxies.clone(),
            chrome_path: self.chrome_path.clone(),
        })
    }

    /// 监听地址（IPv6 自动加方括号）。
    pub fn addr(&self) -> String {
        if self.bind.contains(':') && !self.bind.starts_with('[') {
            format!("[{}]:{}", self.bind, self.port)
        } else {
            format!("{}:{}", self.bind, self.port)
        }
    }

    /// 绑定地址是否回环（决定 API key 是否必填、WebUI 只读豁免）。
    pub fn loopback(&self) -> bool {
        is_loopback(&self.bind)
    }
}

fn is_loopback(bind: &str) -> bool {
    let b = bind.trim();
    if b.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match b.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false, // 主机名/通配地址一律按非回环从严处理
    }
}

/// 极简 `.env` 读取：`KEY=VALUE`，支持空行/`#` 注释/成对引号；不覆盖已有环境变量。
/// 返回键值表（文件中出现过的键），最终优先级由 [`ServerConfig::from_lookup`] 决定。
pub fn load_dotenv(path: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue; // 非法行忽略（dotenv 惯例：不因坏行崩溃）
        };
        let k = k.trim().trim_start_matches("export ").trim();
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let mut v = v.trim();
        if v.starts_with('"') || v.starts_with('\'') {
            let q = v.as_bytes()[0] as char;
            let end = v[1..].find(q).map(|i| i + 1);
            v = match end {
                Some(i) => &v[1..i],
                None => v, // 未闭合引号：原样保留（后续按普通值消费）
            };
        } else if let Some(hash) = v.find(" #") {
            // 未加引号的行内注释
            v = v[..hash].trim_end();
        }
        out.insert(k.to_string(), v.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(map: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: HashMap<String, String> = map
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| owned.get(k).cloned()
    }

    #[test]
    fn defaults_are_loopback_and_safe() {
        let cfg = ServerConfig::from_lookup(&lookup(&[])).expect("默认可启动");
        assert_eq!(cfg.base, "https://duck.ai");
        assert_eq!(cfg.upstream_mode, "auto");
        assert_eq!(cfg.default_model, "gpt-5.6-luna");
        assert!(!cfg.new_chat);
        assert!(cfg.proxies.is_empty(), "默认直连");
        assert_eq!(cfg.api_key, None);
        assert_eq!(cfg.bind, "127.0.0.1");
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.max_concurrency, 8);
        assert_eq!(cfg.admin_password, None);
        assert_eq!(cfg.chrome_path, None);
        assert_eq!(cfg.log_filter, "info");
        assert!(cfg.loopback());
        assert_eq!(cfg.addr(), "127.0.0.1:8080");
    }

    #[test]
    fn fail_fast_non_loopback_without_key() {
        let err = ServerConfig::from_lookup(&lookup(&[("DUCKAI_BIND", "0.0.0.0")]))
            .expect_err("非回环无 key 必须拒绝");
        assert!(err.contains("DUCKAI_API_KEY"), "错误要指名缺的键：{err}");
        // 带上 key 即放行
        let ok = ServerConfig::from_lookup(&lookup(&[
            ("DUCKAI_BIND", "0.0.0.0"),
            ("DUCKAI_API_KEY", "sk-live"),
        ]))
        .expect("带 key 可启动");
        assert!(!ok.loopback());
        assert_eq!(ok.api_key.as_deref(), Some("sk-live"));
    }

    #[test]
    fn localhost_counts_as_loopback_but_unknown_host_does_not() {
        assert!(
            ServerConfig::from_lookup(&lookup(&[("DUCKAI_BIND", "localhost")])).is_ok(),
            "localhost 视为回环"
        );
        assert!(
            ServerConfig::from_lookup(&lookup(&[("DUCKAI_BIND", "myhost")])).is_err(),
            "无法判定的主机名从严要求 key"
        );
        assert!(ServerConfig::from_lookup(&lookup(&[("DUCKAI_BIND", "::1")])).is_ok());
    }

    #[test]
    fn port_and_concurrency_validation() {
        assert!(
            ServerConfig::from_lookup(&lookup(&[("PORT", "abc")])).is_err(),
            "坏 PORT 拒绝"
        );
        assert!(
            ServerConfig::from_lookup(&lookup(&[("PORT", "99999")])).is_err(),
            "越界 PORT 拒绝"
        );
        assert!(
            ServerConfig::from_lookup(&lookup(&[("DUCKAI_MAX_CONCURRENCY", "0")])).is_err(),
            "0 并发拒绝"
        );
        let cfg = ServerConfig::from_lookup(&lookup(&[
            ("PORT", "9000"),
            ("DUCKAI_MAX_CONCURRENCY", "4"),
        ]))
        .expect("合法数值");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.max_concurrency, 4);
    }

    #[test]
    fn proxies_merge_and_validate() {
        let cfg = ServerConfig::from_lookup(&lookup(&[
            ("DUCKAI_PROXIES", "http://user:pw@a:1, socks5://b:1080"),
            ("DUCKAI_PROXY", "http://user:pw@a:1, https://c:443"),
        ]))
        .expect("合并成功");
        assert_eq!(
            cfg.proxies.len(),
            3,
            "逐字去重（凭据不同算不同代理）：{:?}",
            cfg.proxies
        );
        assert!(cfg.proxies.contains(&"https://c:443".to_string()));
        let bad = ServerConfig::from_lookup(&lookup(&[("DUCKAI_PROXIES", "ftp://x")]));
        assert!(bad.is_err(), "未知协议拒绝");
    }

    #[test]
    fn empty_values_treated_as_unset() {
        // .env.example 默认就是这些空值行：空 = 未设置（不是 Some("")）。
        let cfg = ServerConfig::from_lookup(&lookup(&[
            ("DUCKAI_API_KEY", ""),
            ("DUCKAI_VQD_OVERRIDE", ""),
            ("DUCKAI_ADMIN_PASSWORD", ""),
            ("DUCKAI_CHROME_PATH", ""),
        ]))
        .expect("空值不报错");
        assert_eq!(cfg.api_key, None);
        assert_eq!(cfg.vqd_override, None);
        assert_eq!(cfg.admin_password, None);
        assert_eq!(cfg.chrome_path, None);
    }

    #[test]
    fn new_chat_and_mode_validation() {
        let cfg = ServerConfig::from_lookup(&lookup(&[("DUCKAI_NEW_CHAT", "TRUE")])).unwrap();
        assert!(cfg.new_chat);
        assert!(
            ServerConfig::from_lookup(&lookup(&[("DUCKAI_NEW_CHAT", "maybe")])).is_err(),
            "非布尔拒绝"
        );
        assert!(
            ServerConfig::from_lookup(&lookup(&[("DUCKAI_UPSTREAM", "chrome")])).is_err(),
            "未知模式拒绝"
        );
        let f = ServerConfig::from_lookup(&lookup(&[("DUCKAI_UPSTREAM", "browser")]))
            .unwrap()
            .factory()
            .unwrap();
        assert_eq!(f.mode, duckai_upstream::UpstreamMode::Browser);
    }

    #[test]
    fn base_trailing_slash_trimmed_and_addr_ipv6() {
        let cfg = ServerConfig::from_lookup(&lookup(&[
            ("DUCKAI_BASE", "https://duck.ai/"),
            ("DUCKAI_BIND", "::1"),
            ("PORT", "8443"),
        ]))
        .unwrap();
        assert_eq!(cfg.base, "https://duck.ai");
        assert_eq!(cfg.addr(), "[::1]:8443");
    }

    #[test]
    fn dotenv_parses_quotes_and_comments() {
        let dir = std::env::temp_dir().join(format!("duckai_dotenv_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(
            &path,
            "# 注释\nexport FOO=bar\nQUOTED=\"hello world\" # 尾注释\nBAD LINE\nEMPTY=\n",
        )
        .unwrap();
        let map = load_dotenv(path.to_str().unwrap());
        assert_eq!(map.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(map.get("QUOTED").map(String::as_str), Some("hello world"));
        assert_eq!(map.get("EMPTY").map(String::as_str), Some(""));
        assert!(!map.contains_key("BAD LINE"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
