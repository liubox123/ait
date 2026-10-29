use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use chrono::Offset;
use serde::{Deserialize, Serialize};

use crate::git::Signature;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct IdentityFile {
    name: String,
    email: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct CredentialFile {
    servers: std::collections::BTreeMap<String, ServerAuth>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ServerAuth {
    user: String,
    token: String,
}

pub fn ait_home() -> PathBuf {
    if let Ok(path) = std::env::var("AIT_HOME") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ait")
}

pub fn load_identity() -> (String, String) {
    if let (Ok(name), Ok(email)) = (
        std::env::var("AIT_AUTHOR_NAME"),
        std::env::var("AIT_AUTHOR_EMAIL"),
    ) {
        return (name, email);
    }
    if let (Ok(name), Ok(email)) = (
        std::env::var("GIT_AUTHOR_NAME"),
        std::env::var("GIT_AUTHOR_EMAIL"),
    ) {
        return (name, email);
    }
    saved_identity()
}

/// 只读 `identity.toml`，不受 `AIT_AUTHOR_*` / `GIT_AUTHOR_*` 影响，供 `ait config` 改其中一项时保留另一项。
pub fn saved_identity() -> (String, String) {
    let path = ait_home().join("identity.toml");
    if let Ok(text) = fs::read_to_string(path) {
        if let Ok(file) = toml::from_str::<IdentityFile>(&text) {
            return (file.name, file.email);
        }
    }
    ("Ait".into(), "ait@local".into())
}

pub fn save_identity(name: &str, email: &str) -> anyhow::Result<()> {
    let home = ait_home();
    fs::create_dir_all(&home)?;
    let file = IdentityFile {
        name: name.to_string(),
        email: email.to_string(),
    };
    fs::write(home.join("identity.toml"), toml::to_string_pretty(&file)?)?;
    Ok(())
}

pub fn signature_now(name: &str, email: &str) -> Signature {
    let now = chrono::Local::now();
    let offset = now.offset().fix().local_minus_utc();
    let sign = if offset >= 0 { '+' } else { '-' };
    let abs = offset.abs();
    Signature {
        name: name.to_string(),
        email: email.to_string(),
        time: now.timestamp(),
        tz: format!("{sign}{:02}{:02}", abs / 3600, (abs % 3600) / 60),
    }
}

pub fn format_time(timestamp: i64, tz: &str) -> String {
    let offset = tz_seconds(tz);
    let local = chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp + offset, 0)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).expect("epoch"));
    format!("{} {tz}", local.format("%Y-%m-%d %H:%M:%S"))
}

/// `+0800` 这样的时区偏移换算成秒。
fn tz_seconds(tz: &str) -> i64 {
    let (sign, digits) = match tz.as_bytes().first() {
        Some(b'-') => (-1, &tz[1..]),
        Some(b'+') => (1, &tz[1..]),
        _ => (1, tz),
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    let hours: i64 = digits[..2].parse().unwrap_or(0);
    let minutes: i64 = digits[2..].parse().unwrap_or(0);
    sign * (hours * 3600 + minutes * 60)
}

#[derive(Clone, Debug)]
pub struct ServerCred {
    pub user: String,
    pub token: String,
}

pub fn credential_origins() -> Vec<String> {
    read_creds()
        .map(|file| file.servers.into_keys().collect())
        .unwrap_or_default()
}

pub fn load_credential(origin: &str) -> Option<ServerCred> {
    let file = read_creds().ok()?;
    file.servers.get(origin).map(|item| ServerCred {
        user: item.user.clone(),
        token: item.token.clone(),
    })
}

pub fn save_credential(origin: &str, user: &str, token: &str) -> anyhow::Result<()> {
    let home = ait_home();
    fs::create_dir_all(&home)?;
    let mut file = read_creds().unwrap_or_default();
    file.servers.insert(
        origin.to_string(),
        ServerAuth {
            user: user.to_string(),
            token: token.to_string(),
        },
    );
    fs::write(
        home.join("credentials.json"),
        serde_json::to_vec_pretty(&file)?,
    )?;
    Ok(())
}

fn read_creds() -> anyhow::Result<CredentialFile> {
    let text = fs::read_to_string(ait_home().join("credentials.json"))?;
    Ok(serde_json::from_str(&text)?)
}

/// `http://127.0.0.1:7878/r/demo` -> (`http://127.0.0.1:7878`, `demo`)
pub fn split_remote(url: &str) -> anyhow::Result<(String, String)> {
    let url = url.trim().trim_end_matches('/');
    let (origin, repo) = url
        .rsplit_once("/r/")
        .with_context(|| format!("远端地址需要形如 http://主机:端口/r/仓库名，实际是 {url}"))?;
    if !valid_repo_name(repo) {
        bail!("仓库名无效: {repo}");
    }
    if !(origin.starts_with("http://") || origin.starts_with("https://")) {
        bail!("远端地址需要以 http:// 或 https:// 开头");
    }
    Ok((origin.to_string(), repo.to_string()))
}

pub fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && !name.starts_with('.')
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

pub fn valid_user_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("系统随机数不可用");
    hex::encode(bytes)
}

/// 新口令使用 Argon2id，参数按 OWASP 的最低配置：19 MiB、2 次、并行度 1。
/// 盐和参数写在 PHC 字符串里。更早的 PBKDF2 和单次 SHA-256 仍能登录，成功后升级。
pub fn password_hash(salt: &str, password: &str) -> String {
    use argon2::{
        password_hash::{PasswordHasher, SaltString},
        Argon2,
    };
    let raw = hex::decode(salt).unwrap_or_else(|_| salt.as_bytes().to_vec());
    let salt = SaltString::encode_b64(&raw).expect("盐的长度不适合 Argon2");
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("Argon2 计算失败")
        .to_string()
}

pub fn password_is_current(stored: &str) -> bool {
    stored.starts_with("$argon2id$")
}

pub fn password_matches(stored: &str, salt: &str, password: &str) -> bool {
    if stored.starts_with("$argon2") {
        return argon2_matches(stored, password);
    }
    if let Some(rest) = stored.strip_prefix("pbkdf2-sha256$") {
        return pbkdf2_matches(rest, salt, password);
    }
    password_eq(stored, &legacy_password_hash(salt, password))
}

fn argon2_matches(stored: &str, password: &str) -> bool {
    use argon2::{
        password_hash::{PasswordHash, PasswordVerifier},
        Argon2,
    };
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    if !argon2_within_limit(&parsed) {
        return false;
    }
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

fn argon2_within_limit(parsed: &argon2::password_hash::PasswordHash<'_>) -> bool {
    let Some(memory) = argon2_param(parsed, "m") else {
        return false;
    };
    let Some(time) = argon2_param(parsed, "t") else {
        return false;
    };
    let Some(parallelism) = argon2_param(parsed, "p") else {
        return false;
    };
    (8..=131_072).contains(&memory) && (1..=16).contains(&time) && (1..=4).contains(&parallelism)
}

fn argon2_param(parsed: &argon2::password_hash::PasswordHash<'_>, name: &str) -> Option<u32> {
    parsed
        .params
        .get(name)
        .and_then(|value| value.decimal().ok())
}

pub fn token_id(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"ait-session-v1\0");
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

fn legacy_password_hash(salt: &str, password: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b":");
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize())
}

fn pbkdf2_matches(rest: &str, salt: &str, password: &str) -> bool {
    let mut parts = rest.split('$');
    let Some(iterations) = parts.next().and_then(|text| text.parse::<u32>().ok()) else {
        return false;
    };
    let Some(expected) = parts.next() else {
        return false;
    };
    if iterations == 0 || iterations > 2_000_000 || parts.next().is_some() {
        return false;
    }
    let actual = hex::encode(pbkdf2_sha256(
        password.as_bytes(),
        salt.as_bytes(),
        iterations,
    ));
    password_eq(&actual, expected)
}

fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut block = Vec::with_capacity(salt.len() + 4);
    block.extend_from_slice(salt);
    block.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &block);
    let mut acc = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (left, right) in acc.iter_mut().zip(u) {
            *left ^= right;
        }
    }
    acc
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        ipad[index] ^= key_block[index];
        opad[index] ^= key_block[index];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

pub fn password_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

pub fn read_password(prompt: &str) -> anyhow::Result<String> {
    if let Ok(password) = std::env::var("AIT_PASSWORD") {
        if !password.is_empty() {
            return Ok(password);
        }
    }
    eprint!("{prompt}");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim().to_string();
    if line.is_empty() {
        bail!("密码不能为空");
    }
    Ok(line)
}

pub fn manifest_path(git_dir: &Path) -> PathBuf {
    git_dir.join("ait").join("manifest.json")
}

pub fn save_manifest(git_dir: &Path, manifest: &crate::perm::Manifest) -> anyhow::Result<()> {
    let path = manifest_path(git_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(manifest)?)?;
    Ok(())
}

pub fn load_manifest(git_dir: &Path) -> anyhow::Result<Option<crate::perm::Manifest>> {
    let path = manifest_path(git_dir);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&text)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_is_argon2id_and_accepts_older_hashes() {
        let stored = password_hash(&hex::encode(b"0123456789abcdef"), "secret");
        assert!(stored.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(password_is_current(&stored));
        assert!(password_matches(&stored, "ignored", "secret"));
        assert!(!password_matches(&stored, "ignored", "other"));
        let published = "$argon2id$v=19$m=19456,t=2,p=1$YmFkIHNhbHQh$DqHGwv6NQV0VcaJi7jeF1E8IpfMXmXcpq4r2kKyqpXk";
        assert!(password_matches(published, "", "password"));
        let legacy = legacy_password_hash("salt", "secret");
        assert!(password_matches(&legacy, "salt", "secret"));
        assert!(!password_matches(&legacy, "salt", "other"));
        let pbkdf2 = format!(
            "pbkdf2-sha256$1${}",
            hex::encode(pbkdf2_sha256(b"secret", b"salt", 1))
        );
        assert!(password_matches(&pbkdf2, "salt", "secret"));
        assert!(!password_is_current(&pbkdf2));
    }

    #[test]
    fn pbkdf2_matches_rfc6070() {
        let once = pbkdf2_sha256(b"password", b"salt", 1);
        assert_eq!(
            hex::encode(once),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        let twice = pbkdf2_sha256(b"password", b"salt", 2);
        assert_eq!(
            hex::encode(twice),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
    }

    #[test]
    fn token_id_hides_the_token() {
        let token = random_token();
        let id = token_id(&token);
        assert_ne!(id, token);
        assert_eq!(id, token_id(&token));
    }
}
