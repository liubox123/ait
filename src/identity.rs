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
    let utc = chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).expect("epoch"));
    format!("{} {tz}", utc.format("%Y-%m-%d %H:%M:%S"))
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

pub fn password_hash(salt: &str, password: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b":");
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize())
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
