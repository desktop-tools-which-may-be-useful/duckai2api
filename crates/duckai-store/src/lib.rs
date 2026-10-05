//! SQLite 持久层：密钥 / 管理口令 / 出口与设置的「配置类」数据（ARCHITECTURE：配置入库，不存运行态）。
//!
//! - 密钥：明文只在创建/引导时出现一次，库里存 SHA-256 摘要（key_hash 唯一索引）+ 展示前缀。
//! - 管理口令：argon2id（PHC 串，自含 salt；salt 列冗余保留便于轮换）。
//! - 设置：`settings(key, value)` KV，首次启动用 env 引导，之后库为准（env 只做 bootstrap）。
//! - 出口：`egress` 表（C2 引入），`url IS NULL` = 显式 direct 行。
//! - 文件权限：非 `:memory:` 的库文件强制 0600（密钥在内，不给同机其他用户读）。
//! - 并发：`Mutex<Connection>`（rusqlite 连接非 Sync）；临界区都是毫秒级单行读写。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, Salt};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use duckai_types::{ApiKeyInfo, EgressPolicy};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("打开数据库 {path} 失败：{source}")]
    Open {
        path: String,
        source: rusqlite::Error,
    },
    #[error("数据库错误：{0}")]
    Sql(#[from] rusqlite::Error),
    #[error("口令处理失败：{0}")]
    Password(String),
}

/// 密钥与配置的持久化存储（一个进程一个实例，内部 Mutex 串行化）。
pub struct Store {
    conn: Mutex<Connection>,
}

/// 建表语句（幂等；新增列一律 `IF NOT EXISTS` 迁移式追加）。
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS api_keys (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    label      TEXT    NOT NULL DEFAULT '',
    key_hash   TEXT    NOT NULL UNIQUE,
    prefix     TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);
CREATE TABLE IF NOT EXISTS admin_password (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    salt       TEXT    NOT NULL,
    hash       TEXT    NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS egress (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    url              TEXT,
    enabled          INTEGER NOT NULL DEFAULT 1,
    cooldown_enabled INTEGER NOT NULL DEFAULT 1,
    ban_secs         INTEGER NOT NULL DEFAULT 300,
    ban_cap_secs     INTEGER NOT NULL DEFAULT 86400,
    rate_limit_secs  INTEGER NOT NULL DEFAULT 5,
    sort             INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS egress_url_uk ON egress(url) WHERE url IS NOT NULL;
"#;

/// 出口行（配置类，不存封禁运行态）。`url == None` = 显式 direct 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressRow {
    pub id: i64,
    pub url: Option<String>,
    pub enabled: bool,
    pub cooldown_enabled: bool,
    pub ban_secs: u64,
    pub ban_cap_secs: u64,
    pub rate_limit_secs: u64,
    pub sort: i64,
}

impl EgressRow {
    /// 并行映射到出口池策略。
    pub fn policy(&self) -> EgressPolicy {
        EgressPolicy {
            enabled: self.enabled,
            cooldown_enabled: self.cooldown_enabled,
            ban_secs: self.ban_secs,
            ban_cap_secs: self.ban_cap_secs,
            rate_limit_secs: self.rate_limit_secs,
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// API key 的存储摘要（SHA-256 → base64url；key 是高熵随机值，摘要足够，且可直接查索引）。
fn key_digest(plaintext: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(plaintext.as_bytes()))
}

impl Store {
    /// 打开（或创建）库。`path == ":memory:"` 为进程内测试库；其余会创建父目录并 chmod 0600。
    pub fn open(path: &str) -> Result<Self, StoreError> {
        if path == ":memory:" {
            return Self::open_in_memory();
        }
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let conn = Connection::open(path).map_err(|source| StoreError::Open {
            path: path.to_string(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Self::init(conn)
    }

    /// 进程内存库（测试）。
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------- API 密钥

    /// 库中是否存在至少一把未吊销密钥（鉴权开关 = 此值）。
    pub fn has_active_key(&self) -> bool {
        self.lock()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM api_keys WHERE revoked_at IS NULL)",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false)
    }

    /// 是否存在任何密钥行（含已吊销；引导导入以「表为空」为准）。
    pub fn has_any_key(&self) -> bool {
        self.lock()
            .query_row("SELECT EXISTS(SELECT 1 FROM api_keys)", [], |r| {
                r.get::<_, bool>(0)
            })
            .unwrap_or(false)
    }

    /// 引导导入：仅当表为空时写入第一把 key。返回是否真的导入。
    pub fn import_default_key(&self, plaintext: &str, label: &str) -> Result<bool, StoreError> {
        let plaintext = plaintext.trim();
        if plaintext.is_empty() || self.has_any_key() {
            return Ok(false);
        }
        self.insert_key(plaintext, label)?;
        Ok(true)
    }

    /// 生成并写入一把新密钥；明文只在此返回一次（调用方负责展示后不落日志）。
    pub fn create_key(&self, label: &str) -> Result<String, StoreError> {
        let raw = format!("sk-{}", URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()));
        self.insert_key(&raw, label)?;
        Ok(raw)
    }

    fn insert_key(&self, plaintext: &str, label: &str) -> Result<(), StoreError> {
        let prefix: String = plaintext.chars().take(12).collect();
        self.lock().execute(
            "INSERT INTO api_keys (label, key_hash, prefix, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![label, key_digest(plaintext), prefix, now_secs()],
        )?;
        Ok(())
    }

    /// 校验客户端提交的 key（只查活跃行；摘要等值，无需常数时间比较——攻击者面对的是哈希索引）。
    pub fn validate_key(&self, presented: &str) -> bool {
        self.lock()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM api_keys WHERE key_hash = ?1 AND revoked_at IS NULL)",
                params![key_digest(presented.trim())],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false)
    }

    /// 全部密钥元数据（新建在前）。
    pub fn list_keys(&self) -> Result<Vec<ApiKeyInfo>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, label, prefix, created_at, revoked_at FROM api_keys ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ApiKeyInfo {
                id: r.get(0)?,
                label: r.get(1)?,
                prefix: r.get(2)?,
                created_at: r.get(3)?,
                revoked_at: r.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// 吊销一把活跃密钥；返回 false = 不存在或已吊销。
    pub fn revoke_key(&self, id: i64) -> Result<bool, StoreError> {
        let n = self.lock().execute(
            "UPDATE api_keys SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
            params![now_secs(), id],
        )?;
        Ok(n > 0)
    }

    // ------------------------------------------------------------- 管理口令

    /// 库内是否已有自定义口令（有 = env 默认口令被遮蔽）。
    pub fn has_password(&self) -> bool {
        self.lock()
            .query_row("SELECT EXISTS(SELECT 1 FROM admin_password)", [], |r| {
                r.get::<_, bool>(0)
            })
            .unwrap_or(false)
    }

    /// 写入/覆盖自定义口令（argon2id；salt 随机 16 字节，PHC 串自含盐）。
    pub fn set_password(&self, given: &str) -> Result<(), StoreError> {
        if given.trim().is_empty() {
            return Err(StoreError::Password("口令不能为空".to_string()));
        }
        let salt_raw: [u8; 16] = rand::random();
        let salt_b64 = STANDARD_NO_PAD.encode(salt_raw);
        let salt =
            Salt::from_b64(salt_b64.as_str()).map_err(|e| StoreError::Password(e.to_string()))?;
        let hash = Argon2::default()
            .hash_password(given.as_bytes(), salt)
            .map_err(|e| StoreError::Password(e.to_string()))?
            .to_string();
        self.lock().execute(
            "INSERT INTO admin_password (id, salt, hash, updated_at) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET salt = excluded.salt, hash = excluded.hash, updated_at = excluded.updated_at",
            params![salt_b64, hash, now_secs()],
        )?;
        Ok(())
    }

    /// argon2id 校验库内口令；无行返回 false（调用方回落默认口令）。
    pub fn verify_password(&self, given: &str) -> bool {
        let hash: Option<String> = self
            .lock()
            .query_row("SELECT hash FROM admin_password WHERE id = 1", [], |r| {
                r.get(0)
            })
            .ok();
        let Some(hash) = hash else {
            return false;
        };
        let Ok(phc) = PasswordHash::new(&hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(given.as_bytes(), &phc)
            .is_ok()
    }

    // ------------------------------------------------------------- 设置 KV

    /// 读一项设置（不存在返回 None）。
    pub fn get_setting(&self, key: &str) -> Option<String> {
        self.lock()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .ok()
    }

    /// 写/覆盖一项设置（upsert）。
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.lock().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// 全量设置表。
    pub fn settings(&self) -> Result<HashMap<String, String>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (k, v) = row?;
            out.insert(k, v);
        }
        Ok(out)
    }

    // ------------------------------------------------------------- 出口（per-egress 策略）

    /// 出口列表（`sort, id` 即出口池槽位顺序）。
    pub fn egresses(&self) -> Result<Vec<EgressRow>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, url, enabled, cooldown_enabled, ban_secs, ban_cap_secs,
                    rate_limit_secs, sort
             FROM egress ORDER BY sort, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(EgressRow {
                id: r.get(0)?,
                url: r.get(1)?,
                enabled: r.get(2)?,
                cooldown_enabled: r.get(3)?,
                ban_secs: r.get::<_, i64>(4)?.max(0) as u64,
                ban_cap_secs: r.get::<_, i64>(5)?.max(0) as u64,
                rate_limit_secs: r.get::<_, i64>(6)?.max(0) as u64,
                sort: r.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 首启引导：`egress` 表为空时按 env 条目播种（`None` = 直连；空列表播种单一直连行）。
    /// 返回 `true` 表示本次播种；已有行时不动（库为准）。
    pub fn seed_egresses(&self, entries: &[Option<String>]) -> Result<bool, StoreError> {
        let d = EgressPolicy::default();
        let mut conn = self.lock();
        let existing: i64 = conn.query_row("SELECT COUNT(*) FROM egress", [], |r| r.get(0))?;
        if existing > 0 {
            return Ok(false);
        }
        let tx = conn.transaction()?;
        let rows: Vec<Option<String>> = if entries.is_empty() {
            vec![None]
        } else {
            entries.to_vec()
        };
        for (i, url) in rows.iter().enumerate() {
            tx.execute(
                "INSERT INTO egress (url, enabled, cooldown_enabled, ban_secs, ban_cap_secs,
                                     rate_limit_secs, sort)
                 VALUES (?1, 1, 1, ?2, ?3, ?4, ?5)",
                params![
                    url,
                    d.ban_secs as i64,
                    d.ban_cap_secs as i64,
                    d.rate_limit_secs as i64,
                    i as i64
                ],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// 按条目定位行 id（`None` = direct 行）。
    fn egress_id(&self, url: Option<&str>) -> Result<Option<i64>, StoreError> {
        let conn = self.lock();
        let id = match url {
            Some(u) => conn
                .query_row("SELECT id FROM egress WHERE url = ?1", params![u], |r| {
                    r.get(0)
                })
                .optional()?,
            None => conn
                .query_row("SELECT id FROM egress WHERE url IS NULL", [], |r| r.get(0))
                .optional()?,
        };
        Ok(id)
    }

    /// 新增出口行（已存在返回既有 id；direct 行全表唯一）。
    pub fn upsert_egress(&self, url: Option<&str>) -> Result<i64, StoreError> {
        if let Some(id) = self.egress_id(url)? {
            return Ok(id);
        }
        let d = EgressPolicy::default();
        let conn = self.lock();
        let sort: i64 =
            conn.query_row("SELECT COALESCE(MAX(sort), -1) + 1 FROM egress", [], |r| {
                r.get(0)
            })?;
        conn.execute(
            "INSERT INTO egress (url, enabled, cooldown_enabled, ban_secs, ban_cap_secs,
                                 rate_limit_secs, sort)
             VALUES (?1, 1, 1, ?2, ?3, ?4, ?5)",
            params![
                url,
                d.ban_secs as i64,
                d.ban_cap_secs as i64,
                d.rate_limit_secs as i64,
                sort
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 删除出口行（行不存在返回 false）。
    pub fn delete_egress(&self, url: Option<&str>) -> Result<bool, StoreError> {
        let n = match url {
            Some(u) => self
                .lock()
                .execute("DELETE FROM egress WHERE url = ?1", params![u])?,
            None => self
                .lock()
                .execute("DELETE FROM egress WHERE url IS NULL", [])?,
        };
        Ok(n > 0)
    }

    /// 更新单出口策略四元组（行不存在返回 false）。
    pub fn set_egress_policy(
        &self,
        url: Option<&str>,
        cooldown_enabled: bool,
        ban_secs: u64,
        ban_cap_secs: u64,
        rate_limit_secs: u64,
    ) -> Result<bool, StoreError> {
        let n = match url {
            Some(u) => self.lock().execute(
                "UPDATE egress SET cooldown_enabled = ?1, ban_secs = ?2, ban_cap_secs = ?3,
                                   rate_limit_secs = ?4
                 WHERE url = ?5",
                params![
                    cooldown_enabled,
                    ban_secs as i64,
                    ban_cap_secs as i64,
                    rate_limit_secs as i64,
                    u
                ],
            )?,
            None => self.lock().execute(
                "UPDATE egress SET cooldown_enabled = ?1, ban_secs = ?2, ban_cap_secs = ?3,
                                   rate_limit_secs = ?4
                 WHERE url IS NULL",
                params![
                    cooldown_enabled,
                    ban_secs as i64,
                    ban_cap_secs as i64,
                    rate_limit_secs as i64
                ],
            )?,
        };
        Ok(n > 0)
    }

    /// 启用/停用单出口（行不存在返回 false）。
    pub fn set_egress_enabled(&self, url: Option<&str>, enabled: bool) -> Result<bool, StoreError> {
        let n = match url {
            Some(u) => self.lock().execute(
                "UPDATE egress SET enabled = ?1 WHERE url = ?2",
                params![enabled, u],
            )?,
            None => self.lock().execute(
                "UPDATE egress SET enabled = ?1 WHERE url IS NULL",
                params![enabled],
            )?,
        };
        Ok(n > 0)
    }
}

impl duckai_types::ApiKeyAuth for Store {
    fn enabled(&self) -> bool {
        self.has_active_key()
    }

    fn validate(&self, presented: &str) -> bool {
        self.validate_key(presented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duckai_types::ApiKeyAuth as _;

    fn mem() -> Store {
        Store::open_in_memory().expect("内存库")
    }

    /// 临时文件库路径（测试名 + 纳秒时间戳，避免并行互踩）。
    fn tmp_path(name: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("时钟")
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "duckai-store-{name}-{}-{nanos}.db",
                std::process::id()
            ))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn key_lifecycle_create_validate_revoke() {
        let s = mem();
        assert!(!s.has_active_key());
        let raw = s.create_key("测试键").expect("生成");
        assert!(raw.starts_with("sk-") && raw.len() > 20, "格式：{raw}");
        assert!(s.has_active_key());
        assert!(s.validate_key(&raw), "本尊可验");
        assert!(!s.validate_key("sk-wrong"), "假 key 拒绝");
        assert!(!s.validate_key(""), "空 key 拒绝");

        let list = s.list_keys().expect("列表");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].label, "测试键");
        assert!(list[0].active());
        assert!(list[0].prefix.starts_with("sk-"));

        assert!(s.revoke_key(list[0].id).expect("吊销"), "首次吊销成功");
        assert!(!s.revoke_key(list[0].id).expect("重复调用"), "幂等=false");
        assert!(!s.validate_key(&raw), "吊销后失效");
        assert!(!s.has_active_key());
        assert!(s.has_any_key(), "吊销不删行（保留审计）");
    }

    #[test]
    fn import_default_key_only_when_empty() {
        let s = mem();
        assert!(
            s.import_default_key("sk-default", "引导").expect("导入"),
            "空表可导入"
        );
        assert!(s.validate_key("sk-default"));
        assert!(
            !s.import_default_key("sk-second", "再导").expect("二次调用"),
            "非空表拒绝导入（库为准）"
        );
        assert!(!s.validate_key("sk-second"));
        // 空白串不导入
        let s2 = mem();
        assert!(!s2.import_default_key("  ", "x").expect("空白拒绝"));
        assert!(!s2.has_any_key());
    }

    #[test]
    fn password_hash_roundtrip() {
        let s = mem();
        assert!(!s.has_password());
        assert!(!s.verify_password("whatever"), "无行时恒 false");
        s.set_password("correct horse").expect("设置");
        assert!(s.has_password());
        assert!(s.verify_password("correct horse"));
        assert!(!s.verify_password("wrong"), "错口令拒绝");
        s.set_password("new-secret").expect("覆盖");
        assert!(s.verify_password("new-secret"), "新口令生效");
        assert!(!s.verify_password("correct horse"), "旧口令失效");
        assert!(s.set_password("   ").is_err(), "空白口令拒绝");
    }

    #[test]
    fn settings_upsert_and_dump() {
        let s = mem();
        assert_eq!(s.get_setting("default_model"), None);
        s.set_setting("default_model", "gpt-5.6-luna").expect("写");
        s.set_setting("default_model", "gpt-6").expect("覆盖");
        assert_eq!(s.get_setting("default_model").as_deref(), Some("gpt-6"));
        s.set_setting("base", "https://duck.ai").expect("写");
        let all = s.settings().expect("dump");
        assert_eq!(all.len(), 2);
        assert_eq!(all.get("base").map(String::as_str), Some("https://duck.ai"));
    }

    #[test]
    fn file_store_creates_dir_and_sets_0600() {
        let path = tmp_path("perm");
        let dir = std::path::Path::new(&path)
            .parent()
            .expect("有父目录")
            .to_path_buf();
        let s = Store::open(&path).expect("打开文件库");
        assert!(dir.exists(), "父目录自动创建");
        drop(s);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("库文件存在")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "密钥库仅属主可读写，得到 {mode:o}");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn auth_trait_impl_matches_store() {
        let s = mem();
        assert!(!s.enabled());
        let raw = s.create_key("l").expect("k");
        assert!(s.enabled());
        assert!(s.validate(&raw));
        assert!(!s.validate("nope"));
    }

    #[test]
    fn reopen_persists_data() {
        let path = tmp_path("reopen");
        {
            let s = Store::open(&path).expect("首开");
            s.create_key("持久").expect("建 key");
            s.set_setting("k", "v").expect("写设置");
        }
        let s = Store::open(&path).expect("重开");
        assert!(s.has_active_key(), "key 跨进程持久");
        assert_eq!(s.get_setting("k").as_deref(), Some("v"));
        let _ = std::fs::remove_file(&path);
    }

    // ---- egress 表 ----

    #[test]
    fn egress_seed_is_first_boot_only_and_defaults_policy() {
        let s = mem();
        let entries = vec![Some("socks5://b:1080".to_string()), None];
        assert!(s.seed_egresses(&entries).expect("播种"), "首空播种");
        assert!(
            !s.seed_egresses(&[Some("http://x:1".to_string())])
                .expect("重播"),
            "已有行不再播种（库为准）"
        );
        let rows = s.egresses().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].url.as_deref(), Some("socks5://b:1080"));
        assert_eq!(rows[1].url, None, "None = 显式 direct 行");
        let d = EgressPolicy::default();
        assert_eq!(rows[1].policy(), d, "播种默认 = §6.1 基线");
        assert!(rows[0].enabled && rows[0].cooldown_enabled);
    }

    #[test]
    fn egress_seed_empty_list_grows_single_direct_row() {
        let s = mem();
        assert!(s.seed_egresses(&[]).expect("播"));
        let rows = s.egresses().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url, None);
    }

    #[test]
    fn egress_upsert_is_idempotent_and_direct_single_row() {
        let s = mem();
        let a1 = s.upsert_egress(Some("http://a:1")).unwrap();
        let a2 = s.upsert_egress(Some("http://a:1")).unwrap();
        assert_eq!(a1, a2, "同一 URL 复用行");
        let d1 = s.upsert_egress(None).unwrap();
        let d2 = s.upsert_egress(None).unwrap();
        assert_eq!(d1, d2, "direct 行全表唯一（NULL 也要挡）");
        assert_eq!(s.egresses().unwrap().len(), 2);
        assert!(
            s.upsert_egress(Some("http://a:1")).unwrap()
                < s.upsert_egress(Some("http://z:9")).unwrap(),
            "sort 递增"
        );
    }

    #[test]
    fn egress_policy_and_toggle_persist_across_reopen() {
        let path = tmp_path("egress-reopen");
        {
            let s = Store::open(&path).expect("首开");
            s.upsert_egress(None).expect("direct 行");
            assert!(
                s.set_egress_policy(None, false, 60, 3600, 30)
                    .expect("改策略")
            );
            assert!(s.set_egress_enabled(None, false).expect("停用"));
            assert!(
                !s.set_egress_policy(Some("http://gone:1"), true, 1, 1, 1)
                    .unwrap(),
                "不存在的行返回 false"
            );
        }
        let s = Store::open(&path).expect("重开");
        let rows = s.egresses().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].policy(),
            EgressPolicy {
                enabled: false,
                cooldown_enabled: false,
                ban_secs: 60,
                ban_cap_secs: 3600,
                rate_limit_secs: 30,
            },
            "出口策略是配置，跨重启持久"
        );
        assert!(s.delete_egress(None).expect("删 direct"));
        assert!(!s.delete_egress(None).expect("再删 = 不存在"));
        assert!(s.egresses().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
