use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::collab::{
    self, AiConfig, AiConfigView, AiReview, CheckGate, CheckRun, CheckSpec, Claim, DiffStat,
    Event,
};
use crate::git::{valid_branch, Object, Oid, Repository};
use crate::identity::{
    password_hash, password_is_current, password_matches, random_token, token_id, valid_repo_name,
    valid_user_name,
};
use crate::perm::{self, Access, Duty, Manifest, Member, Policy, Rule};
use crate::plan::{self, Assignment};

mod gitops;
pub use gitops::RefCommand;

#[derive(Clone, Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

fn git_err(err: anyhow::Error) -> ApiError {
    ApiError::bad(err.to_string())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    root: PathBuf,
    lock: Mutex<()>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UserRecord {
    name: String,
    salt: String,
    password_hash: String,
    admin: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoMeta {
    pub name: String,
    pub description: String,
    pub owner: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoSummary {
    pub name: String,
    pub description: String,
    pub owner: String,
    pub branch: String,
    pub head: Option<Oid>,
    pub subject: String,
    pub updated_at: i64,
    pub clone_url: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CommitInfo {
    pub oid: Oid,
    pub summary: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub time: i64,
    pub tz: String,
    pub parents: Vec<Oid>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TreeItem {
    pub name: String,
    pub path: String,
    pub mode: u32,
    pub oid: Oid,
    pub kind: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct BlobView {
    pub path: String,
    pub oid: Oid,
    pub mode: u32,
    pub size: usize,
    pub binary: bool,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Review {
    pub id: String,
    pub author: String,
    pub reference: String,
    pub base: Oid,
    pub head: Oid,
    pub status: String,
    pub message: String,
    pub blocked: Vec<String>,
    pub paths: Vec<String>,
    pub created_at: i64,
    pub decided_by: Option<String>,
    pub decision_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewView {
    #[serde(flatten)]
    pub review: Review,
    pub diff: String,
    pub can_approve: bool,
    #[serde(default)]
    pub checks: Vec<CheckGate>,
    #[serde(default)]
    pub checks_blocking: Vec<String>,
    #[serde(default)]
    pub ai: Option<AiReview>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub title: String,
    pub body: String,
    pub author: String,
    pub base_ref: String,
    pub head_ref: String,
    pub status: String,
    pub approvals: Vec<String>,
    pub created_at: i64,
    pub merged_by: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProposalView {
    #[serde(flatten)]
    pub proposal: Proposal,
    pub diff: String,
    pub blocked: Vec<String>,
    /// 作者无权修改的路径；非空时不能合并。
    #[serde(default)]
    pub denied: Vec<String>,
    pub paths: Vec<String>,
    pub can_approve: bool,
    pub can_merge: bool,
    pub can_close: bool,
    #[serde(default)]
    pub base_oid: Option<Oid>,
    #[serde(default)]
    pub head_oid: Option<Oid>,
    #[serde(default)]
    pub stat: DiffStat,
    /// 超出职责限制的原因；非空时即使在写权限内也需要别人通过。
    #[serde(default)]
    pub limit_reasons: Vec<String>,
    #[serde(default)]
    pub checks: Vec<CheckGate>,
    #[serde(default)]
    pub checks_blocking: Vec<String>,
    #[serde(default)]
    pub ai: Option<AiReview>,
    #[serde(default)]
    pub author_kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectBody {
    pub kind: String,
    #[serde(alias = "payload_b64")]
    pub payload_b64: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PushRequest {
    #[serde(alias = "ref")]
    pub reference: String,
    pub old_oid: Oid,
    pub new_oid: Oid,
    #[serde(default)]
    pub objects: Vec<ObjectBody>,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub pack_b64: Option<String>,
    #[serde(skip)]
    pub staged: Vec<(Oid, &'static str, Vec<u8>)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PushResponse {
    pub status: String,
    pub reference: String,
    pub oid: Oid,
    pub review_id: Option<String>,
    pub blocked: Vec<String>,
    pub message: String,
    pub manifest: Manifest,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PullRequest {
    #[serde(default)]
    pub reference: String,
    #[serde(default)]
    pub have: Vec<Oid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectOut {
    pub oid: Oid,
    pub kind: String,
    pub payload_b64: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PullResponse {
    pub reference: String,
    pub oid: Oid,
    pub objects: Vec<ObjectOut>,
    pub manifest: Manifest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefInfo {
    pub name: String,
    pub oid: Oid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefsResponse {
    pub refs: Vec<RefInfo>,
    pub manifest: Manifest,
    #[serde(default)]
    pub tags: Vec<RefInfo>,
    #[serde(default)]
    pub head: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanView {
    pub content: String,
    pub assignments: Vec<PlanAssignment>,
    pub policy: Option<Policy>,
    pub applied: bool,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanAssignment {
    pub user: String,
    pub duty: String,
    pub paths: Vec<String>,
}

impl Store {
    pub fn open(root: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(root.join("repos"))?;
        if !root.join("users.json").exists() {
            write_json(&root.join("users.json"), &Vec::<UserRecord>::new())?;
        }
        if !root.join("tokens.json").exists() {
            write_json(
                &root.join("tokens.json"),
                &BTreeMap::<String, String>::new(),
            )?;
        }
        Ok(Self {
            inner: Arc::new(StoreInner {
                root,
                lock: Mutex::new(()),
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.inner
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn register(&self, name: &str, password: &str) -> Result<AuthSession, ApiError> {
        if !valid_user_name(name) {
            return Err(ApiError::bad(
                "用户名只能包含字母、数字、点、下划线和短横线",
            ));
        }
        if password.is_empty() {
            return Err(ApiError::bad("密码不能为空"));
        }
        let salt = {
            let _guard = self.lock();
            let users = self.users()?;
            if !users.is_empty() && !registration_open() {
                return Err(ApiError::forbidden("已关闭公开注册"));
            }
            if users.iter().any(|user| user.name == name) {
                return Err(ApiError::conflict("用户已存在"));
            }
            random_token()
        };
        let password_hash = password_hash(&salt, password);
        let _guard = self.lock();
        let mut users = self.users()?;
        if !users.is_empty() && !registration_open() {
            return Err(ApiError::forbidden("已关闭公开注册"));
        }
        if users.iter().any(|user| user.name == name) {
            return Err(ApiError::conflict("用户已存在"));
        }
        let admin = users.is_empty();
        users.push(UserRecord {
            name: name.to_string(),
            salt,
            password_hash,
            admin,
        });
        self.save_users(&users)?;
        self.issue_token(name)
    }

    pub fn login(&self, name: &str, password: &str) -> Result<AuthSession, ApiError> {
        let snapshot = {
            let _guard = self.lock();
            let users = self.users()?;
            users
                .iter()
                .find(|user| user.name == name)
                .map(|user| (user.salt.clone(), user.password_hash.clone()))
        };
        let known = snapshot.is_some();
        let (salt, stored) = match snapshot {
            Some(found) => found,
            None => ("ait-unknown-user".to_string(), unknown_password_hash()),
        };
        let ok = password_matches(&stored, &salt, password);
        if !known || !ok {
            return Err(ApiError::unauthorized("用户名或密码错误"));
        }
        let upgraded = if password_is_current(&stored) {
            None
        } else {
            Some(password_hash(&salt, password))
        };
        let _guard = self.lock();
        let mut users = self.users()?;
        let Some(index) = users.iter().position(|user| user.name == name) else {
            return Err(ApiError::unauthorized("用户名或密码错误"));
        };
        if users[index].salt != salt {
            if !password_matches(&users[index].password_hash, &users[index].salt, password) {
                return Err(ApiError::unauthorized("用户名或密码错误"));
            }
        } else if let Some(password_hash) = upgraded {
            if !password_is_current(&users[index].password_hash) {
                users[index].password_hash = password_hash;
                self.save_users(&users)?;
            }
        }
        let name = users[index].name.clone();
        self.issue_token(&name)
    }

    pub fn logout(&self, token: &str) -> Result<(), ApiError> {
        let _guard = self.lock();
        let mut tokens = self.tokens()?;
        tokens.remove(&token_id(token));
        self.save_tokens(&tokens)?;
        self.forget_token_time(token)
    }

    pub fn authenticate(&self, token: &str) -> Result<AuthSession, ApiError> {
        let _guard = self.lock();
        let tokens = self.tokens()?;
        let Some(name) = tokens.get(&token_id(token)) else {
            return Err(ApiError::unauthorized("登录已失效"));
        };
        let name = name.clone();
        let name = &name;
        if self.token_expired(token)? {
            self.drop_token(token)?;
            return Err(ApiError::unauthorized("登录已过期，请重新登录"));
        }
        let users = self.users()?;
        let Some(user) = users.iter().find(|user| &user.name == name) else {
            return Err(ApiError::unauthorized("用户不存在"));
        };
        Ok(AuthSession {
            token: token.to_string(),
            user: user.name.clone(),
            admin: user.admin,
        })
    }

    pub fn user_names(&self) -> Result<Vec<String>, ApiError> {
        let _guard = self.lock();
        Ok(self.users()?.into_iter().map(|user| user.name).collect())
    }

    fn issue_token(&self, name: &str) -> Result<AuthSession, ApiError> {
        let users = self.users()?;
        let user = users
            .iter()
            .find(|user| user.name == name)
            .ok_or_else(|| ApiError::bad("用户不存在"))?;
        let mut tokens = self.tokens()?;
        let token = random_token();
        tokens.insert(token_id(&token), name.to_string());
        self.save_tokens(&tokens)?;
        self.note_token_issued(&token)?;
        Ok(AuthSession {
            token,
            user: name.to_string(),
            admin: user.admin,
        })
    }

    pub fn create_repo(
        &self,
        user: &str,
        admin: bool,
        name: &str,
        description: &str,
    ) -> Result<RepoMeta, ApiError> {
        let _ = admin;
        let _guard = self.lock();
        if !valid_repo_name(name) {
            return Err(ApiError::bad(
                "仓库名只能包含字母、数字、点、下划线和短横线，且不能以点开头",
            ));
        }
        let dir = self.repo_dir(name);
        if dir.exists() {
            return Err(ApiError::conflict("仓库已存在"));
        }
        fs::create_dir_all(&dir).map_err(|err| ApiError::bad(err.to_string()))?;
        Repository::init(&dir.join("git"), true).map_err(git_err)?;
        let meta = RepoMeta {
            name: name.to_string(),
            description: description.trim().to_string(),
            owner: user.to_string(),
            created_at: now_secs(),
        };
        write_json(&dir.join("meta.json"), &meta).map_err(io_err)?;
        write_json(&dir.join("policy.json"), &Policy::owner(user)).map_err(io_err)?;
        fs::write(dir.join("plan.md"), "").map_err(|err| ApiError::bad(err.to_string()))?;
        write_json(&dir.join("reviews.json"), &Vec::<Review>::new()).map_err(io_err)?;
        write_json(&dir.join("pulls.json"), &Vec::<Proposal>::new()).map_err(io_err)?;
        write_json(
            &dir.join("branch_owners.json"),
            &BTreeMap::<String, String>::new(),
        )
        .map_err(io_err)?;
        Ok(meta)
    }

    pub fn list_repos(&self, base_url: &str) -> Result<Vec<RepoSummary>, ApiError> {
        let _guard = self.lock();
        let mut out = Vec::new();
        let root = self.inner.root.join("repos");
        let entries = fs::read_dir(&root).map_err(|err| ApiError::bad(err.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|err| ApiError::bad(err.to_string()))?;
            if !entry.path().is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            if let Ok(summary) = self.summary_of(&name, base_url) {
                out.push(summary);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn repo_summary(&self, name: &str, base_url: &str) -> Result<RepoSummary, ApiError> {
        let _guard = self.lock();
        self.summary_of(name, base_url)
    }

    fn summary_of(&self, name: &str, base_url: &str) -> Result<RepoSummary, ApiError> {
        let meta = self.meta(name)?;
        let repo = self.open_git(name)?;
        let branch = repo
            .branch_name()
            .map_err(git_err)?
            .unwrap_or_else(|| "main".into());
        let head = repo.head_oid().map_err(git_err)?;
        let (subject, updated_at) = if let Some(oid) = head {
            let commit = repo.read_commit(oid).map_err(git_err)?;
            (commit.summary().to_string(), commit.committer.time)
        } else {
            (String::new(), meta.created_at)
        };
        Ok(RepoSummary {
            name: meta.name,
            description: meta.description,
            owner: meta.owner,
            branch,
            head,
            subject,
            updated_at,
            clone_url: format!("{}/r/{}", base_url.trim_end_matches('/'), name),
        })
    }

    pub fn tree(&self, name: &str, reference: &str, path: &str) -> Result<Vec<TreeItem>, ApiError> {
        let _guard = self.lock();
        let repo = self.open_git(name)?;
        let Some(oid) = self.resolve_optional(&repo, reference)? else {
            return Ok(Vec::new());
        };
        let entries = repo.list_dir(oid, path).map_err(git_err)?;
        Ok(entries
            .into_iter()
            .map(|entry| TreeItem {
                name: entry.name,
                path: entry.path,
                mode: entry.mode,
                oid: entry.oid,
                kind: entry.kind,
            })
            .collect())
    }

    pub fn blob(&self, name: &str, reference: &str, path: &str) -> Result<BlobView, ApiError> {
        let _guard = self.lock();
        let repo = self.open_git(name)?;
        let oid = self.resolve_required(&repo, reference)?;
        let (mode, file_oid, bytes) = repo.file_at(oid, path).map_err(git_err)?;
        let binary = bytes.iter().take(8192).any(|byte| *byte == 0);
        let text = if binary {
            String::new()
        } else {
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(200_000)
                .collect()
        };
        Ok(BlobView {
            path: path.to_string(),
            oid: file_oid,
            mode,
            size: bytes.len(),
            binary,
            text,
        })
    }

    pub fn commits(
        &self,
        name: &str,
        reference: &str,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, ApiError> {
        let _guard = self.lock();
        let repo = self.open_git(name)?;
        let Some(oid) = self.resolve_optional(&repo, reference)? else {
            return Ok(Vec::new());
        };
        let views = repo.log(oid, limit.clamp(1, 200)).map_err(git_err)?;
        Ok(views
            .into_iter()
            .map(|view| CommitInfo {
                oid: view.oid,
                summary: view.summary,
                message: view.message,
                author_name: view.author_name,
                author_email: view.author_email,
                time: view.time,
                tz: view.tz,
                parents: view.parents,
            })
            .collect())
    }

    pub fn diff(&self, name: &str, from: Option<Oid>, to: Oid) -> Result<String, ApiError> {
        let _guard = self.lock();
        let repo = self.open_git(name)?;
        let files = repo
            .diff_range(from.filter(|oid| !oid.is_zero()), to)
            .map_err(git_err)?;
        Ok(truncate_text(&crate::git::unified_diff(&files), 200_000))
    }

    pub fn branches(&self, name: &str) -> Result<Vec<RefInfo>, ApiError> {
        let _guard = self.lock();
        let repo = self.open_git(name)?;
        let mut out = Vec::new();
        for branch in repo.branches().map_err(git_err)? {
            out.push(RefInfo {
                name: branch.name,
                oid: branch.oid,
            });
        }
        Ok(out)
    }

    pub fn policy(&self, name: &str) -> Result<Policy, ApiError> {
        let _guard = self.lock();
        self.policy_of(name)
    }

    pub fn put_policy(
        &self,
        user: &str,
        admin: bool,
        name: &str,
        rules: Vec<Rule>,
        duties: Vec<Duty>,
        members: Option<Vec<Member>>,
    ) -> Result<Policy, ApiError> {
        let _guard = self.lock();
        let mut policy = self.policy_of(name)?;
        self.require_owner(&policy, user, admin)?;
        let candidate = Policy {
            owners: policy.owners.clone(),
            rules,
            duties,
            members: members.unwrap_or_else(|| policy.members.clone()),
        };
        if let Some(error) = policy_errors(&candidate).into_iter().next() {
            return Err(ApiError::bad(error));
        }
        policy = candidate;
        write_json(&self.repo_dir(name).join("policy.json"), &policy).map_err(io_err)?;
        self.record(name, user, "policy", "", "更新了目录职责与权限");
        Ok(policy)
    }

    /// 预览一份规划：每个目录谁能直接修改、谁被禁止，以及没人负责或多人重叠的地方。
    pub fn policy_preview(
        &self,
        name: &str,
        reference: &str,
        rules: Vec<Rule>,
        duties: Vec<Duty>,
        members: Vec<Member>,
    ) -> Result<PolicyPreview, ApiError> {
        let _guard = self.lock();
        let current = self.policy_of(name)?;
        let policy = Policy {
            owners: current.owners.clone(),
            rules,
            duties,
            members,
        };
        let errors = policy_errors(&policy);
        let repo = self.open_git(name)?;
        let files: Vec<String> = match self.resolve_optional(&repo, reference)? {
            Some(oid) => {
                let commit = repo.read_commit(oid).map_err(git_err)?;
                repo.tree_files(commit.tree)
                    .map_err(git_err)?
                    .into_keys()
                    .collect()
            }
            None => Vec::new(),
        };
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for file in &files {
            let mut dir = String::new();
            let parts: Vec<&str> = file.split('/').collect();
            for part in &parts[..parts.len().saturating_sub(1)] {
                if !dir.is_empty() {
                    dir.push('/');
                }
                dir.push_str(part);
                *counts.entry(dir.clone()).or_default() += 1;
            }
        }
        let mut users: Vec<String> = self.users()?.into_iter().map(|user| user.name).collect();
        let mut add_user = |name: &str| {
            if name != "*" && !users.iter().any(|have| have == name) {
                users.push(name.to_string());
            }
        };
        policy.rules.iter().for_each(|rule| add_user(&rule.user));
        policy.duties.iter().for_each(|duty| add_user(&duty.user));
        policy.members.iter().for_each(|member| add_user(&member.user));
        users.sort();
        let manifests: Vec<Manifest> = users
            .iter()
            .map(|user| {
                let admin = self.is_admin_user(user).unwrap_or(false);
                policy.manifest_for(user, admin)
            })
            .collect();
        let others: Vec<&Manifest> = manifests
            .iter()
            .filter(|manifest| !policy.owners.contains(&manifest.user))
            .collect();
        let mut dirs = Vec::new();
        let mut warnings = Vec::new();
        for (path, count) in counts.iter().take(3000) {
            let probe = format!("{path}/\u{1}");
            let writers: Vec<String> = others
                .iter()
                .filter(|manifest| manifest.decide(&probe) == Access::Write)
                .map(|manifest| manifest.user.clone())
                .collect();
            let denied: Vec<String> = others
                .iter()
                .filter(|manifest| manifest.decide(&probe) == Access::Deny)
                .map(|manifest| manifest.user.clone())
                .collect();
            let mut holders: Vec<String> = policy
                .duties
                .iter()
                .filter(|duty| {
                    duty.paths
                        .iter()
                        .any(|pattern| pattern != "**" && perm::path_match(pattern, &probe))
                })
                .map(|duty| duty.user.clone())
                .collect();
            holders.dedup();
            let parent = path.rsplit_once('/').map(|(parent, _)| parent);
            let parent_writers = parent.and_then(|parent| {
                dirs.iter()
                    .find(|dir: &&DirAccess| dir.path == parent)
                    .map(|dir| dir.writers.clone())
            });
            if writers.len() > 1 && parent_writers.as_ref() != Some(&writers) {
                warnings.push(format!("{} 都能直接修改 {path}/，注意分工", writers.join("、")));
            }
            if !path.contains('/') && writers.is_empty() {
                warnings.push(format!("{path}/ 没有分配给任何开发者，只有所有者能直接修改"));
            }
            dirs.push(DirAccess {
                path: path.clone(),
                files: *count,
                writers,
                denied,
                duties: holders,
            });
        }
        for rule in &policy.rules {
            if rule.pattern != "**"
                && !files.iter().any(|file| perm::path_match(&rule.pattern, file))
                && !counts.keys().any(|dir| perm::path_match(&rule.pattern, dir))
            {
                warnings.push(format!(
                    "{} 的规则 {} 目前没有命中任何文件",
                    rule.user, rule.pattern
                ));
            }
        }
        for duty in &policy.duties {
            if policy.owners.contains(&duty.user) {
                continue;
            }
            let manifest = manifests.iter().find(|manifest| manifest.user == duty.user);
            for pattern in &duty.paths {
                let probe = format!("{}/\u{1}", pattern.trim_end_matches("/**"));
                let covered = manifest.is_some_and(|m| {
                    m.write.contains(pattern) || m.decide(&probe) == Access::Write
                });
                if !covered {
                    warnings.push(format!(
                        "{} 的职责包含 {pattern}，但没有它的写权限",
                        duty.user
                    ));
                }
            }
        }
        for member in &policy.members {
            if member.kind == "agent" && member.supervisor.is_none() {
                warnings.push(format!("agent {} 没有指定负责人", member.user));
            }
        }
        warnings.truncate(60);
        Ok(PolicyPreview {
            dirs,
            manifests,
            users,
            warnings,
            errors,
        })
    }

    pub fn manifest_for(&self, name: &str, user: &str, admin: bool) -> Result<Manifest, ApiError> {
        let _guard = self.lock();
        Ok(self.policy_of(name)?.manifest_for(user, admin))
    }

    pub fn plan_text(&self, name: &str) -> Result<String, ApiError> {
        let _guard = self.lock();
        self.ensure_repo(name)?;
        Ok(fs::read_to_string(self.repo_dir(name).join("plan.md")).unwrap_or_default())
    }

    pub fn save_plan(
        &self,
        user: &str,
        admin: bool,
        name: &str,
        content: &str,
        apply: bool,
        assignments: Option<Vec<Assignment>>,
        source: &str,
    ) -> Result<PlanView, ApiError> {
        let _guard = self.lock();
        let policy = self.policy_of(name)?;
        self.require_owner(&policy, user, admin)?;
        fs::write(self.repo_dir(name).join("plan.md"), content)
            .map_err(|err| ApiError::bad(err.to_string()))?;
        let parsed = if let Some(items) = assignments {
            items
        } else if apply {
            if content.trim().is_empty() {
                Vec::new()
            } else {
                plan::parse_plan(content).map_err(|err| ApiError::bad(err.to_string()))?
            }
        } else {
            plan::parse_plan(content).unwrap_or_else(|_| Vec::new())
        };
        let mut preview = plan::policy_from_assignments(&policy.owners, &parsed);
        preview.members = policy.members.clone();
        let applied = if apply {
            write_json(&self.repo_dir(name).join("policy.json"), &preview).map_err(io_err)?;
            true
        } else {
            false
        };
        Ok(PlanView {
            content: content.to_string(),
            assignments: parsed
                .into_iter()
                .map(|item| PlanAssignment {
                    user: item.user,
                    duty: item.duty,
                    paths: item.paths,
                })
                .collect(),
            policy: Some(if applied { preview } else { policy }),
            applied,
            source: source.to_string(),
        })
    }

    pub fn refs(&self, name: &str, user: &str, admin: bool) -> Result<RefsResponse, ApiError> {
        let _guard = self.lock();
        let repo = self.open_git(name)?;
        let manifest = self.policy_of(name)?.manifest_for(user, admin);
        let refs = repo
            .list_refs("refs/heads/")
            .map_err(git_err)?
            .into_iter()
            .map(|(name, oid)| RefInfo { name, oid })
            .collect();
        let tags = repo
            .list_refs("refs/tags/")
            .map_err(git_err)?
            .into_iter()
            .map(|(name, oid)| RefInfo { name, oid })
            .collect();
        let head = self.head_branch(name)?;
        Ok(RefsResponse {
            refs,
            manifest,
            tags,
            head,
        })
    }

    pub fn pull(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        request: PullRequest,
    ) -> Result<PullResponse, ApiError> {
        let _guard = self.lock();
        let repo = self.open_git(name)?;
        let manifest = self.policy_of(name)?.manifest_for(user, admin);
        let reference = if request.reference.is_empty() {
            "refs/heads/main".to_string()
        } else {
            normalize_branch(&request.reference)?
        };
        let Some(oid) = repo.resolve_ref(&reference).map_err(git_err)? else {
            return Ok(PullResponse {
                reference,
                oid: Oid::ZERO,
                objects: Vec::new(),
                manifest,
            });
        };
        let objects = repo.objects_since(oid, &request.have).map_err(git_err)?;
        Ok(PullResponse {
            reference,
            oid,
            objects: objects
                .into_iter()
                .map(|object| ObjectOut {
                    oid: object.oid,
                    kind: object.kind,
                    payload_b64: STANDARD.encode(object.payload),
                })
                .collect(),
            manifest,
        })
    }

    pub fn push(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        request: PushRequest,
    ) -> Result<PushResponse, ApiError> {
        let _guard = self.lock();
        if request.reference.starts_with("refs/tags/") {
            return self.push_tag_locked(name, user, admin, &request);
        }
        if request.new_oid.is_zero() {
            return self.delete_branch_locked(name, user, admin, &request);
        }
        let reference = normalize_branch(&request.reference)?;
        let repo = self.open_git(name)?;
        let policy = self.policy_of(name)?;
        let owner = policy.is_owner(user, admin);
        let protected = self.protected_branch(name, &reference);
        let mut branch_owners = self.branch_owners(name)?;
        let creator = branch_owners.get(&reference).cloned();
        if !protected {
            if let Some(creator) = &creator {
                if creator != user && !owner {
                    return Err(ApiError::forbidden(format!(
                        "不能更新别人的分支 {}",
                        short_ref(&reference)
                    )));
                }
            }
        }
        let can_force = owner || (!protected && creator.as_deref() == Some(user));
        if request.force && !can_force {
            return Err(ApiError::forbidden("只有仓库所有者可以强制推送"));
        }
        let current = repo
            .resolve_ref(&reference)
            .map_err(git_err)?
            .unwrap_or(Oid::ZERO);
        if current != request.old_oid && !(request.force && can_force) {
            return Err(ApiError::conflict(format!(
                "远端 {reference} 已是 {current}，请先拉取"
            )));
        }
        self.stage_request(&repo, &request)?;
        ensure_complete(&repo, request.new_oid)?;
        if !current.is_zero() && current != request.new_oid && !(request.force && can_force) {
            let fast_forward = repo
                .is_ancestor(current, request.new_oid)
                .map_err(git_err)?;
            if !fast_forward {
                return Err(ApiError::conflict("这次推送不是快进，请先拉取并合并"));
            }
        }
        let manifest = policy.manifest_for(user, admin);
        let compare_from = if !protected && current.is_zero() {
            repo.resolve_ref("refs/heads/main")
                .map_err(git_err)?
                .filter(|oid| !oid.is_zero())
        } else if current.is_zero() {
            None
        } else {
            Some(current)
        };
        let changes = repo
            .changed_paths(compare_from, request.new_oid)
            .map_err(git_err)?;
        let mut denied = Vec::new();
        let mut blocked = Vec::new();
        let mut paths = Vec::new();
        for change in &changes {
            paths.push(change.path.clone());
            match manifest.decide(&change.path) {
                Access::Deny => denied.push(change.path.clone()),
                Access::Review => blocked.push(change.path.clone()),
                Access::Write => {}
            }
        }
        if !denied.is_empty() {
            return Err(ApiError::forbidden(format!(
                "这些路径禁止修改: {}",
                denied.join(", ")
            )));
        }
        repo.persist_staged().map_err(git_err)?;
        if !protected {
            repo.set_ref(&reference, request.new_oid).map_err(git_err)?;
            if creator.is_none() {
                branch_owners.insert(reference.clone(), user.to_string());
                self.save_branch_owners(name, &branch_owners)?;
            }
            self.note_branch_moved(name, &reference, request.new_oid)?;
            self.record(
                name,
                user,
                "push",
                short_ref(&reference),
                &format!("推送 {} 个文件到 {}", paths.len(), short_ref(&reference)),
            );
            return Ok(PushResponse {
                status: "updated".into(),
                reference,
                oid: request.new_oid,
                review_id: None,
                blocked,
                message: "已更新分支".into(),
                manifest,
            });
        }
        let mut reasons = Vec::new();
        if !blocked.is_empty() {
            reasons.push("修改了写权限之外的路径".to_string());
        }
        let stat = collab::diff_stat(
            &repo
                .diff_range(compare_from, request.new_oid)
                .map_err(git_err)?,
        );
        let limited = collab::limit_violations(&manifest, &stat);
        if !manifest.limits.required_checks.is_empty() {
            reasons.push(format!(
                "职责要求先通过校验: {}",
                manifest.limits.required_checks.join(", ")
            ));
        }
        if !limited.is_empty() || !manifest.limits.required_checks.is_empty() {
            reasons.extend(limited);
            blocked = paths.clone();
        }
        if !blocked.is_empty() {
            let review = self.supersede_and_add(
                name,
                Review {
                    id: format!("r-{}", &random_token()[..8]),
                    author: user.to_string(),
                    reference: reference.clone(),
                    base: current,
                    head: request.new_oid,
                    status: "pending".into(),
                    message: reasons.join("；"),
                    blocked: blocked.clone(),
                    paths,
                    created_at: now_secs(),
                    decided_by: None,
                    decision_reason: None,
                },
            )?;
            self.record(
                name,
                user,
                "review",
                &review.id,
                &format!("推送到 {} 进入评审: {}", short_ref(&reference), review.message),
            );
            return Ok(PushResponse {
                status: "review".into(),
                reference,
                oid: request.new_oid,
                review_id: Some(review.id.clone()),
                blocked,
                message: format!("{}，已提交评审 {}", reasons.join("；"), review.id),
                manifest,
            });
        }
        repo.set_ref(&reference, request.new_oid).map_err(git_err)?;
        self.record(
            name,
            user,
            "push",
            short_ref(&reference),
            &format!("推送 {} 个文件到 {}", paths.len(), short_ref(&reference)),
        );
        Ok(PushResponse {
            status: "updated".into(),
            reference,
            oid: request.new_oid,
            review_id: None,
            blocked,
            message: "已更新分支".into(),
            manifest,
        })
    }

    pub fn reviews(
        &self,
        name: &str,
        user: &str,
        admin: bool,
    ) -> Result<Vec<ReviewView>, ApiError> {
        let _guard = self.lock();
        let policy = self.policy_of(name)?;
        let manifest = policy.manifest_for(user, admin);
        let repo = self.open_git(name)?;
        let mut views = Vec::new();
        for review in self.review_list(name)? {
            let from = if review.base.is_zero() {
                None
            } else {
                Some(review.base)
            };
            let diff = match repo.diff_range(from, review.head) {
                Ok(files) => truncate_text(&crate::git::unified_diff(&files), 200_000),
                Err(err) => format!("无法生成差异: {err}"),
            };
            let author_manifest =
                policy.manifest_for(&review.author, self.is_admin_user(&review.author)?);
            let checks = self.gates_for(name, &repo, review.head, &author_manifest, &review.paths);
            let checks_blocking = collab::failing_required(&checks);
            let can_approve = review.status == "pending"
                && (review.author != user || admin)
                && can_approve(&manifest, &policy, user, admin, &review.blocked);
            let ai = self.latest_ai(name, &review.id);
            views.push(ReviewView {
                review,
                diff,
                can_approve,
                checks,
                checks_blocking,
                ai,
            });
        }
        views.sort_by(|a, b| b.review.created_at.cmp(&a.review.created_at));
        Ok(views)
    }

    pub fn approve(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        id: &str,
    ) -> Result<Review, ApiError> {
        let _guard = self.lock();
        self.decide(name, user, admin, id, true, "")
    }

    pub fn reject(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        id: &str,
        reason: &str,
    ) -> Result<Review, ApiError> {
        let _guard = self.lock();
        self.decide(name, user, admin, id, false, reason)
    }

    fn decide(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        id: &str,
        approve: bool,
        reason: &str,
    ) -> Result<Review, ApiError> {
        let policy = self.policy_of(name)?;
        let manifest = policy.manifest_for(user, admin);
        let mut reviews = self.review_list(name)?;
        let Some(review) = reviews.iter_mut().find(|review| review.id == id) else {
            return Err(ApiError::not_found("评审不存在"));
        };
        if review.status != "pending" {
            return Err(ApiError::conflict(format!("评审已是 {}", review.status)));
        }
        if !can_approve(&manifest, &policy, user, admin, &review.blocked) {
            return Err(ApiError::forbidden("你没有这些路径的写权限，不能评审"));
        }
        if approve && review.author == user && !admin {
            return Err(ApiError::forbidden("不能通过自己的推送"));
        }
        if approve {
            let repo = self.open_git(name)?;
            let author_manifest =
                policy.manifest_for(&review.author, self.is_admin_user(&review.author)?);
            let gates = self.gates_for(name, &repo, review.head, &author_manifest, &review.paths);
            let failing = collab::failing_required(&gates);
            if !failing.is_empty() {
                return Err(ApiError::conflict(format!(
                    "必过校验还没有通过: {}",
                    failing.join("，")
                )));
            }
            let current = repo
                .resolve_ref(&review.reference)
                .map_err(git_err)?
                .unwrap_or(Oid::ZERO);
            if current != review.base {
                return Err(ApiError::conflict(
                    "分支在评审期间又有新提交，请让作者重新推送",
                ));
            }
            ensure_complete(&repo, review.head)?;
            repo.set_ref(&review.reference, review.head)
                .map_err(git_err)?;
            review.status = "approved".into();
        } else {
            review.status = "rejected".into();
        }
        review.decided_by = Some(user.to_string());
        review.decision_reason = Some(reason.to_string());
        let saved = review.clone();
        self.save_reviews(name, &reviews)?;
        self.record(
            name,
            user,
            "review",
            &saved.id,
            &format!(
                "{}了 {} 的推送 {}",
                if approve { "通过" } else { "退回" },
                saved.author,
                saved.id
            ),
        );
        Ok(saved)
    }

    pub fn proposals(
        &self,
        name: &str,
        user: &str,
        admin: bool,
    ) -> Result<Vec<ProposalView>, ApiError> {
        let _guard = self.lock();
        self.proposal_views(name, user, admin)
    }

    pub fn create_proposal(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        title: &str,
        body: &str,
        base: &str,
        head: &str,
    ) -> Result<ProposalView, ApiError> {
        let _guard = self.lock();
        let title = title.trim();
        if title.is_empty() {
            return Err(ApiError::bad("标题不能为空"));
        }
        let base_ref = normalize_branch(base)?;
        let head_ref = normalize_branch(head)?;
        if base_ref == head_ref {
            return Err(ApiError::bad("来源分支和目标分支不能相同"));
        }
        let policy = self.policy_of(name)?;
        let owner = policy.is_owner(user, admin);
        let creators = self.branch_owners(name)?;
        if creators.get(&head_ref).map(String::as_str) != Some(user) && !owner {
            return Err(ApiError::forbidden("只能为自己推送的分支创建合并请求"));
        }
        let repo = self.open_git(name)?;
        let base_oid = repo
            .resolve_ref(&base_ref)
            .map_err(git_err)?
            .ok_or_else(|| ApiError::not_found("目标分支不存在"))?;
        let head_oid = repo
            .resolve_ref(&head_ref)
            .map_err(git_err)?
            .ok_or_else(|| ApiError::not_found("来源分支不存在"))?;
        let author_admin = admin || self.is_admin_user(user)?;
        let manifest = policy.manifest_for(user, author_admin);
        let landing = landing_changes(&repo, base_oid, head_oid, &manifest)?;
        if landing.paths.is_empty() {
            return Err(ApiError::bad("没有需要合并的更改"));
        }
        if !landing.denied.is_empty() {
            return Err(ApiError::forbidden(format!(
                "这些路径禁止修改: {}",
                landing.denied.join(", ")
            )));
        }
        let mut items = self.proposal_list(name)?;
        if let Some(existing) = items.iter().find(|item| {
            item.status == "open" && item.head_ref == head_ref && item.base_ref == base_ref
        }) {
            return Err(ApiError::conflict(format!(
                "这个分支已经有打开的合并请求 {}",
                existing.id
            )));
        }
        let proposal = Proposal {
            id: format!("pr-{}", &random_token()[..8]),
            title: title.to_string(),
            body: body.trim().to_string(),
            author: user.to_string(),
            base_ref,
            head_ref: head_ref.clone(),
            status: "open".into(),
            approvals: Vec::new(),
            created_at: now_secs(),
            merged_by: None,
        };
        let id = proposal.id.clone();
        items.push(proposal);
        self.save_proposals(name, &items)?;
        self.record(name, user, "pull", &id, &format!("创建合并请求「{title}」"));
        self.one_proposal(name, user, admin, &id)
    }

    pub fn approve_proposal(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        id: &str,
    ) -> Result<ProposalView, ApiError> {
        let _guard = self.lock();
        let mut items = self.proposal_list(name)?;
        let Some(proposal) = items.iter_mut().find(|item| item.id == id) else {
            return Err(ApiError::not_found("合并请求不存在"));
        };
        if proposal.status != "open" {
            return Err(ApiError::conflict(format!(
                "合并请求已是 {}",
                proposal.status
            )));
        }
        if proposal.author == user && !admin {
            return Err(ApiError::forbidden("不能通过自己的合并请求"));
        }
        let repo = self.open_git(name)?;
        let policy = self.policy_of(name)?;
        let landing = self.proposal_gate(&repo, name, &policy, proposal)?.landing;
        if landing.blocked.is_empty() {
            return Err(ApiError::bad("这些更改都在作者的写权限内，可以直接合并"));
        }
        if !landing.denied.is_empty() {
            return Err(ApiError::forbidden(format!(
                "这些路径禁止修改: {}",
                landing.denied.join(", ")
            )));
        }
        let manifest = policy.manifest_for(user, admin || self.is_admin_user(user)?);
        if !writes_all(&manifest, &landing.blocked) {
            return Err(ApiError::forbidden("你没有这些路径的写权限，不能通过"));
        }
        if !proposal.approvals.iter().any(|name| name == user) {
            proposal.approvals.push(user.to_string());
        }
        let title = proposal.title.clone();
        self.save_proposals(name, &items)?;
        self.record(name, user, "pull", id, &format!("通过了合并请求「{title}」"));
        self.one_proposal(name, user, admin, id)
    }

    pub fn close_proposal(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        id: &str,
    ) -> Result<ProposalView, ApiError> {
        let _guard = self.lock();
        let policy = self.policy_of(name)?;
        let mut items = self.proposal_list(name)?;
        let Some(proposal) = items.iter_mut().find(|item| item.id == id) else {
            return Err(ApiError::not_found("合并请求不存在"));
        };
        if proposal.status != "open" {
            return Err(ApiError::conflict(format!(
                "合并请求已是 {}",
                proposal.status
            )));
        }
        if proposal.author != user && !policy.is_owner(user, admin) {
            return Err(ApiError::forbidden("只有作者或仓库所有者可以关闭"));
        }
        proposal.status = "closed".into();
        let title = proposal.title.clone();
        self.save_proposals(name, &items)?;
        self.record(name, user, "pull", id, &format!("关闭了合并请求「{title}」"));
        self.one_proposal(name, user, admin, id)
    }

    pub fn merge_proposal(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        id: &str,
    ) -> Result<ProposalView, ApiError> {
        let _guard = self.lock();
        let mut items = self.proposal_list(name)?;
        let Some(index) = items.iter().position(|item| item.id == id) else {
            return Err(ApiError::not_found("合并请求不存在"));
        };
        if items[index].status != "open" {
            return Err(ApiError::conflict(format!(
                "合并请求已是 {}",
                items[index].status
            )));
        }
        let repo = self.open_git(name)?;
        let base_ref = items[index].base_ref.clone();
        let head_ref = items[index].head_ref.clone();
        let author = items[index].author.clone();
        let title = items[index].title.clone();
        let approvals = items[index].approvals.clone();
        let policy = self.policy_of(name)?;
        let gate = self.proposal_gate(&repo, name, &policy, &items[index])?;
        let (base_oid, head_oid) = (gate.base_oid, gate.head_oid);
        let landing = gate.landing;
        if !landing.denied.is_empty() {
            return Err(ApiError::forbidden(format!(
                "这些路径禁止修改: {}",
                landing.denied.join(", ")
            )));
        }
        if !gate.checks_blocking.is_empty() {
            return Err(ApiError::conflict(format!(
                "必过校验还没有通过: {}",
                gate.checks_blocking.join("，")
            )));
        }
        let blocked = landing.blocked.clone();
        let merger_admin = admin || self.is_admin_user(user)?;
        let merger_manifest = policy.manifest_for(user, merger_admin);
        let allowed = if blocked.is_empty() {
            user == author
                || policy.is_owner(user, merger_admin)
                || writes_all(&merger_manifest, &landing.paths)
        } else {
            let approved = approvals.iter().any(|approver| {
                let Ok(is_admin) = self.is_admin_user(approver) else {
                    return false;
                };
                writes_all(&policy.manifest_for(approver, is_admin), &blocked)
            });
            approved
                && (policy.is_owner(user, merger_admin)
                    || approvals.iter().any(|name| name == user))
        };
        if !allowed {
            return Err(ApiError::forbidden(
                "需要有写权限的人先通过，才能合并到目标分支",
            ));
        }
        let signature = crate::git::Signature {
            name: user.to_string(),
            email: format!("{user}@ait"),
            time: now_secs(),
            tz: "+0000".into(),
        };
        let merged = repo
            .integrate(
                base_oid,
                head_oid,
                &format!(
                    "合并 {} 到 {}\n\n{title}",
                    short_ref(&head_ref),
                    short_ref(&base_ref)
                ),
                &signature,
            )
            .map_err(git_err)?;
        let oid = match merged {
            crate::git::MergeOutcome::UpToDate => {
                return Err(ApiError::bad("没有需要合并的更改"));
            }
            crate::git::MergeOutcome::FastForward(oid) | crate::git::MergeOutcome::Merged(oid) => {
                oid
            }
            crate::git::MergeOutcome::Conflicts(paths) => {
                return Err(ApiError::conflict(format!(
                    "合并冲突: {}。请在来源分支上更新后再合并",
                    paths.join(", ")
                )));
            }
        };
        repo.set_ref(&base_ref, oid).map_err(git_err)?;
        items[index].status = "merged".into();
        items[index].merged_by = Some(user.to_string());
        self.save_proposals(name, &items)?;
        self.record(name, user, "pull", id, &format!("合并了「{title}」"));
        self.one_proposal(name, user, admin, id)
    }

    pub fn checks_view(&self, name: &str, reference: &str) -> Result<ChecksView, ApiError> {
        let _guard = self.lock();
        let repo = self.open_git(name)?;
        let Some(oid) = self.resolve_optional(&repo, reference)? else {
            return Ok(ChecksView {
                oid: None,
                specs: Vec::new(),
                config_error: None,
                gates: Vec::new(),
                runs: Vec::new(),
                runner: check_runner(),
            });
        };
        let (specs, config_error) = match checks_at(&repo, oid) {
            Ok(specs) => (specs, None),
            Err(err) => (Vec::new(), Some(err)),
        };
        let commit = repo.read_commit(oid).map_err(git_err)?;
        let paths: Vec<String> = repo
            .changed_paths(commit.parents.first().copied(), oid)
            .map_err(git_err)?
            .into_iter()
            .map(|change| change.path)
            .collect();
        let author = self
            .policy_of(name)?
            .manifest_for(&commit.author.name, false);
        let gates = self.gates_for(name, &repo, oid, &author, &paths);
        let mut runs: Vec<CheckRun> = self
            .check_runs(name)?
            .into_iter()
            .filter(|run| run.oid == oid)
            .collect();
        runs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        for run in &mut runs {
            run.log = truncate_text(&run.log, 4000);
        }
        Ok(ChecksView {
            oid: Some(oid),
            specs,
            config_error,
            gates,
            runs,
            runner: check_runner(),
        })
    }

    pub fn check_run(&self, name: &str, id: &str) -> Result<CheckRun, ApiError> {
        let _guard = self.lock();
        self.check_runs(name)?
            .into_iter()
            .find(|run| run.id == id)
            .ok_or_else(|| ApiError::not_found("校验记录不存在"))
    }

    /// 开发者、agent 或 CI 上报校验结果。只有所有者和 ci 账号的上报能满足必过校验。
    pub fn report_check(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        report: CheckReport,
    ) -> Result<CheckRun, ApiError> {
        let _guard = self.lock();
        if !collab::valid_check_name(&report.name) {
            return Err(ApiError::bad("校验名只能包含字母、数字、点、下划线和短横线"));
        }
        if !collab::CHECK_STATUSES.contains(&report.status.as_str()) {
            return Err(ApiError::bad(format!(
                "状态只能是 {}",
                collab::CHECK_STATUSES.join("、")
            )));
        }
        let repo = self.open_git(name)?;
        let oid = self.resolve_required(&repo, &report.reference)?;
        if let Ok(specs) = checks_at(&repo, oid) {
            if specs
                .iter()
                .any(|spec| spec.name == report.name && spec.runner == "server")
            {
                return Err(ApiError::forbidden("这项校验只能由服务端执行"));
            }
        }
        let policy = self.policy_of(name)?;
        let trusted = policy.is_owner(user, admin) || policy.kind_of(user) == "ci";
        let now = now_secs();
        let run = CheckRun {
            id: format!("c-{}", &random_token()[..10]),
            oid,
            name: report.name.clone(),
            status: report.status.clone(),
            summary: truncate_text(report.summary.trim(), 500),
            url: if report.url.starts_with("http://") || report.url.starts_with("https://") {
                report.url.clone()
            } else {
                String::new()
            },
            log: truncate_text(&report.log, 64 * 1024),
            reporter: user.to_string(),
            trusted,
            runner: "report".into(),
            created_at: now,
            finished_at: if matches!(report.status.as_str(), "pending" | "running") {
                None
            } else {
                Some(now)
            },
        };
        let mut runs = self.check_runs(name)?;
        runs.push(run.clone());
        self.save_check_runs(name, runs)?;
        self.record(
            name,
            user,
            "check",
            &oid.to_string()[..8],
            &format!("上报校验 {} = {}", run.name, run.status),
        );
        Ok(run)
    }

    /// 为服务端执行器登记待跑的校验，返回需要执行的任务。
    pub fn queue_server_checks(
        &self,
        name: &str,
        user: &str,
        reference: &str,
        only: Option<&str>,
    ) -> Result<Vec<(CheckRun, CheckSpec)>, ApiError> {
        let _guard = self.lock();
        if check_runner().is_none() {
            return Err(ApiError::bad(
                "服务端没有启用校验执行器（设置 AIT_CHECK_RUNNER=docker）",
            ));
        }
        let repo = self.open_git(name)?;
        let oid = self.resolve_required(&repo, reference)?;
        let specs = checks_at(&repo, oid).map_err(ApiError::bad)?;
        let now = now_secs();
        let mut runs = self.check_runs(name)?;
        let mut jobs = Vec::new();
        for spec in specs {
            if spec.runner == "report" || only.is_some_and(|only| only != spec.name) {
                continue;
            }
            let busy = collab::latest_runs(&runs, oid).iter().any(|run| {
                run.name == spec.name && matches!(run.status.as_str(), "pending" | "running")
            });
            if busy {
                continue;
            }
            let run = CheckRun {
                id: format!("c-{}", &random_token()[..10]),
                oid,
                name: spec.name.clone(),
                status: "pending".into(),
                summary: "等待服务端执行".into(),
                url: String::new(),
                log: String::new(),
                reporter: format!("ait（{user} 触发）"),
                trusted: true,
                runner: "server".into(),
                created_at: now,
                finished_at: None,
            };
            runs.push(run.clone());
            jobs.push((run, spec));
        }
        if only.is_some() && jobs.is_empty() {
            return Err(ApiError::bad("没有可以由服务端执行的同名校验，或它正在运行"));
        }
        self.save_check_runs(name, runs)?;
        Ok(jobs)
    }

    /// 在 Docker 沙箱里执行一项已登记的校验。耗时操作不持有仓库锁。
    pub fn execute_check(&self, name: &str, run: &CheckRun, spec: &CheckSpec) {
        let workdir = self
            .inner
            .root
            .join("tmp")
            .join(format!("{name}-{}", run.id));
        let prepared = {
            let _guard = self.lock();
            self.update_run(name, &run.id, "running", "正在执行", None);
            self.materialize(name, run.oid, &workdir)
        };
        let (status, log) = match prepared {
            Ok(()) => collab::run_in_docker(&workdir, spec, &format!("ait-check-{}", run.id)),
            Err(err) => ("error".into(), format!("准备工作目录失败: {}", err.message)),
        };
        let _ = fs::remove_dir_all(&workdir);
        let _guard = self.lock();
        let summary = match status.as_str() {
            "success" => "通过".to_string(),
            "failure" => "未通过".to_string(),
            _ => log.lines().last().unwrap_or("执行出错").to_string(),
        };
        self.update_run(name, &run.id, &status, &summary, Some(log));
        self.record(
            name,
            "ait",
            "check",
            &run.oid.to_string()[..8],
            &format!("服务端校验 {} = {status}", run.name),
        );
    }

    fn update_run(&self, name: &str, id: &str, status: &str, summary: &str, log: Option<String>) {
        let Ok(mut runs) = self.check_runs(name) else {
            return;
        };
        if let Some(run) = runs.iter_mut().find(|run| run.id == id) {
            run.status = status.to_string();
            run.summary = summary.to_string();
            if let Some(log) = log {
                run.log = log;
                run.finished_at = Some(now_secs());
            }
        }
        let _ = self.save_check_runs(name, runs);
    }

    fn materialize(&self, name: &str, oid: Oid, dir: &Path) -> Result<(), ApiError> {
        let repo = self.open_git(name)?;
        let commit = repo.read_commit(oid).map_err(git_err)?;
        let _ = fs::remove_dir_all(dir);
        fs::create_dir_all(dir).map_err(|err| ApiError::bad(err.to_string()))?;
        for (path, (mode, blob)) in repo.tree_files(commit.tree).map_err(git_err)? {
            if path.split('/').any(|seg| seg == ".." || seg == ".git") || path.starts_with('/') {
                continue;
            }
            let target = dir.join(&path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|err| ApiError::bad(err.to_string()))?;
            }
            if mode == 0o160000 || mode == 0o120000 {
                continue;
            }
            let bytes = repo.read_blob(blob).map_err(git_err)?;
            fs::write(&target, bytes).map_err(|err| ApiError::bad(err.to_string()))?;
            #[cfg(unix)]
            if mode == 0o100755 {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o755));
            }
        }
        Ok(())
    }

    fn check_runs(&self, name: &str) -> Result<Vec<CheckRun>, ApiError> {
        self.load_list(name, "checks.json")
    }

    fn save_check_runs(&self, name: &str, mut runs: Vec<CheckRun>) -> Result<(), ApiError> {
        if runs.len() > 3000 {
            runs.drain(..runs.len() - 3000);
        }
        write_json(&self.repo_dir(name).join("checks.json"), &runs).map_err(io_err)
    }

    pub fn ai_config(&self, name: &str) -> Result<AiConfigView, ApiError> {
        let _guard = self.lock();
        Ok(self.ai_config_of(name)?.view())
    }

    pub fn ai_auto(&self, name: &str) -> bool {
        let _guard = self.lock();
        self.ai_config_of(name)
            .map(|cfg| cfg.auto)
            .unwrap_or(false)
    }

    pub fn put_ai_config(
        &self,
        user: &str,
        admin: bool,
        name: &str,
        update: AiConfigUpdate,
    ) -> Result<AiConfigView, ApiError> {
        let _guard = self.lock();
        let policy = self.policy_of(name)?;
        if !policy.is_owner(user, admin) {
            return Err(ApiError::forbidden("只有仓库所有者可以配置 AI 审查者"));
        }
        let base_url = update.base_url.trim().to_string();
        if !base_url.is_empty()
            && !(base_url.starts_with("https://") || base_url.starts_with("http://"))
        {
            return Err(ApiError::bad("模型地址必须以 http:// 或 https:// 开头"));
        }
        let mut cfg = self.ai_config_of(name)?;
        cfg.enabled = update.enabled;
        cfg.auto = update.auto;
        cfg.base_url = base_url;
        cfg.model = update.model.trim().to_string();
        cfg.instructions = truncate_text(update.instructions.trim(), 4000);
        if let Some(key) = update.api_key {
            cfg.api_key = key.trim().to_string();
        }
        write_json(&self.repo_dir(name).join("ai.json"), &cfg).map_err(io_err)?;
        self.record(name, user, "ai", "", "更新了 AI 审查者配置");
        Ok(cfg.view())
    }

    pub fn ai_reviews(&self, name: &str, target: &str) -> Result<Vec<AiReview>, ApiError> {
        let _guard = self.lock();
        let mut items: Vec<AiReview> = self
            .load_list::<AiReview>(name, "ai_reviews.json")?
            .into_iter()
            .filter(|item| target.is_empty() || item.target == target)
            .collect();
        items.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(items)
    }

    /// 让 AI 审查者审查合并请求（`pr-…`）或推送评审（`r-…`）。模型调用期间不持有仓库锁。
    pub fn run_ai_review(
        &self,
        name: &str,
        user: &str,
        target: &str,
    ) -> Result<AiReview, ApiError> {
        let job = {
            let _guard = self.lock();
            self.ai_job(name, target)?
        };
        let input = collab::ReviewInput {
            title: &job.title,
            body: &job.body,
            author: &job.author,
            manifest: &job.manifest,
            policy: &job.policy,
            files: &job.files,
            claims: &job.claims,
            plan: &job.plan,
            now: now_secs(),
        };
        let (mut verdict, mut summary, mut findings) = collab::rule_review(&input);
        let mut reviewer = "ait 规则".to_string();
        let mut error = None;
        if job.cfg.enabled {
            if let Some(endpoint) = job.cfg.endpoint() {
                match collab::llm_review(&endpoint, &job.cfg.instructions, &input, &findings) {
                    Ok((llm_verdict, llm_summary, llm_findings)) => {
                        reviewer = format!("ait 规则 + {}", endpoint.model);
                        verdict = worse_verdict(&verdict, &llm_verdict);
                        if !llm_summary.trim().is_empty() {
                            summary = format!("{}。{summary}", llm_summary.trim());
                        }
                        findings.extend(llm_findings);
                    }
                    Err(err) => error = Some(format!("{err:#}")),
                }
            } else {
                error = Some("没有配置模型地址，只运行了内置规则".into());
            }
        }
        let review = AiReview {
            id: format!("ai-{}", &random_token()[..8]),
            target: target.to_string(),
            head: job.head,
            reviewer,
            verdict,
            summary,
            findings,
            requested_by: user.to_string(),
            created_at: now_secs(),
            error,
        };
        let _guard = self.lock();
        let mut items = self.load_list::<AiReview>(name, "ai_reviews.json")?;
        items.push(review.clone());
        if items.len() > 1000 {
            items.drain(..items.len() - 1000);
        }
        write_json(&self.repo_dir(name).join("ai_reviews.json"), &items).map_err(io_err)?;
        self.record(
            name,
            "ai",
            "ai",
            target,
            &format!("AI 审查 {target}: {}", verdict_label(&review.verdict)),
        );
        Ok(review)
    }

    fn ai_job(&self, name: &str, target: &str) -> Result<AiJob, ApiError> {
        let policy = self.policy_of(name)?;
        let repo = self.open_git(name)?;
        let cfg = self.ai_config_of(name)?;
        let plan = fs::read_to_string(self.repo_dir(name).join("plan.md")).unwrap_or_default();
        let claims = self.active_claims(name)?;
        if target.starts_with("pr-") {
            let proposal = self
                .proposal_list(name)?
                .into_iter()
                .find(|item| item.id == target)
                .ok_or_else(|| ApiError::not_found("合并请求不存在"))?;
            let gate = self.proposal_gate(&repo, name, &policy, &proposal)?;
            let manifest =
                policy.manifest_for(&proposal.author, self.is_admin_user(&proposal.author)?);
            return Ok(AiJob {
                title: proposal.title,
                body: proposal.body,
                author: proposal.author,
                manifest,
                policy,
                files: gate.files,
                claims,
                plan,
                cfg,
                head: gate.head_oid,
            });
        }
        let review = self
            .review_list(name)?
            .into_iter()
            .find(|item| item.id == target)
            .ok_or_else(|| ApiError::not_found("评审不存在"))?;
        let from = Some(review.base).filter(|oid| !oid.is_zero());
        let files = repo.diff_range(from, review.head).map_err(git_err)?;
        let manifest = policy.manifest_for(&review.author, self.is_admin_user(&review.author)?);
        Ok(AiJob {
            title: format!("推送到 {}", short_ref(&review.reference)),
            body: review.message,
            author: review.author,
            manifest,
            policy,
            files,
            claims,
            plan,
            cfg,
            head: review.head,
        })
    }

    fn ai_config_of(&self, name: &str) -> Result<AiConfig, ApiError> {
        self.ensure_repo(name)?;
        let path = self.repo_dir(name).join("ai.json");
        if !path.exists() {
            return Ok(AiConfig::default());
        }
        read_json(&path).map_err(io_err)
    }

    fn latest_ai(&self, name: &str, target: &str) -> Option<AiReview> {
        self.load_list::<AiReview>(name, "ai_reviews.json")
            .ok()?
            .into_iter()
            .filter(|item| item.target == target)
            .max_by_key(|item| item.created_at)
    }

    pub fn open_pull_for(&self, name: &str, reference: &str) -> Option<String> {
        let _guard = self.lock();
        self.proposal_list(name)
            .ok()?
            .into_iter()
            .find(|item| item.status == "open" && item.head_ref == reference)
            .map(|item| item.id)
    }

    pub fn claims(&self, name: &str) -> Result<Vec<Claim>, ApiError> {
        let _guard = self.lock();
        self.active_claims(name)
    }

    /// 登记工作意图。与别人的意图重叠时照常登记，并把重叠的意图一起返回。
    pub fn add_claim(
        &self,
        name: &str,
        user: &str,
        paths: Vec<String>,
        intent: &str,
        branch: Option<String>,
        ttl_minutes: Option<i64>,
    ) -> Result<ClaimResult, ApiError> {
        let _guard = self.lock();
        let paths: Vec<String> = paths
            .into_iter()
            .map(|path| path.trim().trim_matches('/').to_string())
            .filter(|path| !path.is_empty())
            .collect();
        if paths.is_empty() {
            return Err(ApiError::bad("至少填写一个路径"));
        }
        if paths.iter().any(|path| path.contains("..")) {
            return Err(ApiError::bad("路径不能包含 .."));
        }
        let intent = intent.trim();
        if intent.is_empty() {
            return Err(ApiError::bad("请说明要做什么"));
        }
        let now = now_secs();
        let ttl = ttl_minutes.unwrap_or(240).clamp(5, 7 * 24 * 60);
        let mut claims = self.active_claims(name)?;
        let overlaps = collab::claim_overlaps(&paths, &claims, user, now);
        let claim = Claim {
            id: format!("w-{}", &random_token()[..8]),
            user: user.to_string(),
            paths,
            intent: truncate_text(intent, 300),
            branch: branch.filter(|branch| !branch.trim().is_empty()),
            created_at: now,
            expires_at: now + ttl * 60,
        };
        claims.push(claim.clone());
        write_json(&self.repo_dir(name).join("claims.json"), &claims).map_err(io_err)?;
        self.record(
            name,
            user,
            "claim",
            &claim.id,
            &format!("开始: {}（{}）", claim.intent, claim.paths.join(", ")),
        );
        Ok(ClaimResult { claim, overlaps })
    }

    pub fn release_claim(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        id: &str,
    ) -> Result<(), ApiError> {
        let _guard = self.lock();
        let policy = self.policy_of(name)?;
        let mut claims = self.active_claims(name)?;
        let Some(index) = claims.iter().position(|claim| claim.id == id) else {
            return Err(ApiError::not_found("工作意图不存在或已过期"));
        };
        if claims[index].user != user && !policy.is_owner(user, admin) {
            return Err(ApiError::forbidden("只能结束自己的工作意图"));
        }
        let claim = claims.remove(index);
        write_json(&self.repo_dir(name).join("claims.json"), &claims).map_err(io_err)?;
        self.record(name, user, "claim", id, &format!("结束: {}", claim.intent));
        Ok(())
    }

    fn active_claims(&self, name: &str) -> Result<Vec<Claim>, ApiError> {
        let now = now_secs();
        Ok(self
            .load_list::<Claim>(name, "claims.json")?
            .into_iter()
            .filter(|claim| claim.expires_at > now)
            .collect())
    }

    pub fn events(&self, name: &str, limit: usize) -> Result<Vec<Event>, ApiError> {
        let _guard = self.lock();
        let mut events = self.load_list::<Event>(name, "events.json")?;
        events.reverse();
        events.truncate(limit.clamp(1, 500));
        Ok(events)
    }

    /// 记录仓库动态。调用方需持有锁；写失败不影响主流程。
    fn record(&self, name: &str, user: &str, kind: &str, target: &str, message: &str) {
        let Ok(mut events) = self.load_list::<Event>(name, "events.json") else {
            return;
        };
        events.push(Event {
            at: now_secs(),
            user: user.to_string(),
            kind: kind.to_string(),
            target: target.to_string(),
            message: message.to_string(),
        });
        if events.len() > 500 {
            events.drain(..events.len() - 500);
        }
        let _ = write_json(&self.repo_dir(name).join("events.json"), &events);
    }

    fn load_list<T: for<'de> Deserialize<'de>>(
        &self,
        name: &str,
        file: &str,
    ) -> Result<Vec<T>, ApiError> {
        self.ensure_repo(name)?;
        let path = self.repo_dir(name).join(file);
        if !path.exists() {
            return Ok(Vec::new());
        }
        read_json(&path).map_err(io_err)
    }

    fn one_proposal(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        id: &str,
    ) -> Result<ProposalView, ApiError> {
        self.proposal_views(name, user, admin)?
            .into_iter()
            .find(|view| view.proposal.id == id)
            .ok_or_else(|| ApiError::not_found("合并请求不存在"))
    }

    fn proposal_views(
        &self,
        name: &str,
        user: &str,
        admin: bool,
    ) -> Result<Vec<ProposalView>, ApiError> {
        let policy = self.policy_of(name)?;
        let repo = self.open_git(name)?;
        let viewer_admin = admin || self.is_admin_user(user)?;
        let viewer = policy.manifest_for(user, viewer_admin);
        let mut views = Vec::new();
        for proposal in self.proposal_list(name)? {
            let gate = self.proposal_gate(&repo, name, &policy, &proposal).ok();
            let diff = gate
                .as_ref()
                .map(|gate| truncate_text(&crate::git::unified_diff(&gate.files), 200_000))
                .unwrap_or_default();
            let (landing, limit_reasons, stat, checks, checks_blocking, base_oid, head_oid) =
                match gate {
                    Some(gate) => (
                        gate.landing,
                        gate.limit_reasons,
                        gate.stat,
                        gate.checks,
                        gate.checks_blocking,
                        Some(gate.base_oid),
                        Some(gate.head_oid),
                    ),
                    None => Default::default(),
                };
            let blocked = landing.blocked.clone();
            let open = proposal.status == "open";
            let can_approve = open
                && !blocked.is_empty()
                && proposal.author != user
                && writes_all(&viewer, &blocked)
                && !proposal.approvals.iter().any(|name| name == user);
            let approved = proposal.approvals.iter().any(|approver| {
                self.is_admin_user(approver)
                    .map(|is_admin| writes_all(&policy.manifest_for(approver, is_admin), &blocked))
                    .unwrap_or(false)
            });
            let can_merge = open
                && landing.denied.is_empty()
                && checks_blocking.is_empty()
                && if blocked.is_empty() {
                    user == proposal.author
                        || policy.is_owner(user, viewer_admin)
                        || writes_all(&viewer, &landing.paths)
                } else {
                    approved
                        && (policy.is_owner(user, viewer_admin)
                            || proposal.approvals.iter().any(|name| name == user))
                };
            let can_close =
                open && (proposal.author == user || policy.is_owner(user, viewer_admin));
            let ai = self.latest_ai(name, &proposal.id);
            let author_kind = policy.kind_of(&proposal.author).to_string();
            views.push(ProposalView {
                proposal,
                diff,
                blocked,
                denied: landing.denied,
                paths: landing.paths,
                can_approve,
                can_merge,
                can_close,
                base_oid,
                head_oid,
                stat,
                limit_reasons,
                checks,
                checks_blocking,
                ai,
                author_kind,
            });
        }
        views.sort_by(|a, b| b.proposal.created_at.cmp(&a.proposal.created_at));
        Ok(views)
    }

    /// 合并请求的门禁：权限之外的路径、职责限制和必过校验。
    fn proposal_gate(
        &self,
        repo: &Repository,
        name: &str,
        policy: &Policy,
        proposal: &Proposal,
    ) -> Result<Gate, ApiError> {
        let base_oid = repo
            .resolve_ref(&proposal.base_ref)
            .map_err(git_err)?
            .ok_or_else(|| ApiError::not_found("目标分支不存在"))?;
        let head_oid = repo
            .resolve_ref(&proposal.head_ref)
            .map_err(git_err)?
            .ok_or_else(|| ApiError::not_found("来源分支不存在"))?;
        let author_admin = self.is_admin_user(&proposal.author)?;
        let author_manifest = policy.manifest_for(&proposal.author, author_admin);
        let mut landing = landing_changes(repo, base_oid, head_oid, &author_manifest)?;
        landing.blocked = review_paths(&author_manifest, &landing.paths);
        let from = repo
            .merge_base(base_oid, head_oid)
            .map_err(git_err)?
            .unwrap_or(base_oid);
        let files = repo.diff_range(Some(from), head_oid).map_err(git_err)?;
        let stat = collab::diff_stat(&files);
        let limit_reasons = collab::limit_violations(&author_manifest, &stat);
        if !limit_reasons.is_empty() {
            landing.blocked = landing.paths.clone();
        }
        let checks = self.gates_for(name, repo, head_oid, &author_manifest, &landing.paths);
        let checks_blocking = collab::failing_required(&checks);
        Ok(Gate {
            landing,
            limit_reasons,
            stat,
            checks,
            checks_blocking,
            files,
            base_oid,
            head_oid,
        })
    }

    fn gates_for(
        &self,
        name: &str,
        repo: &Repository,
        oid: Oid,
        author: &Manifest,
        paths: &[String],
    ) -> Vec<CheckGate> {
        let runs = self.check_runs(name).unwrap_or_default();
        match checks_at(repo, oid) {
            Ok(specs) => {
                collab::check_gates(&specs, &author.limits.required_checks, &runs, oid, paths)
            }
            Err(message) => vec![CheckGate {
                name: collab::CHECKS_FILE.into(),
                required: true,
                status: "error".into(),
                trusted: false,
                reporter: None,
                summary: message,
                run_id: None,
            }],
        }
    }

    fn note_branch_moved(&self, name: &str, reference: &str, _oid: Oid) -> Result<(), ApiError> {
        let _ = (name, reference);
        Ok(())
    }

    fn supersede_and_add(&self, name: &str, review: Review) -> Result<Review, ApiError> {
        let mut reviews = self.review_list(name)?;
        for existing in &mut reviews {
            if existing.status == "pending"
                && existing.author == review.author
                && existing.reference == review.reference
            {
                existing.status = "superseded".into();
            }
        }
        reviews.push(review.clone());
        self.save_reviews(name, &reviews)?;
        Ok(review)
    }

    fn resolve_optional(
        &self,
        repo: &Repository,
        reference: &str,
    ) -> Result<Option<Oid>, ApiError> {
        if reference.is_empty() {
            return repo.head_oid().map_err(git_err);
        }
        match repo.rev_parse(reference) {
            Ok(oid) => Ok(Some(oid)),
            Err(_) => Err(ApiError::not_found(format!("找不到 {reference}"))),
        }
    }

    fn resolve_required(&self, repo: &Repository, reference: &str) -> Result<Oid, ApiError> {
        self.resolve_optional(repo, reference)?
            .ok_or_else(|| ApiError::not_found("仓库还没有提交"))
    }

    fn require_owner(&self, policy: &Policy, user: &str, admin: bool) -> Result<(), ApiError> {
        if policy.is_owner(user, admin) {
            Ok(())
        } else {
            Err(ApiError::forbidden("只有仓库所有者可以修改规划或权限"))
        }
    }

    fn users(&self) -> Result<Vec<UserRecord>, ApiError> {
        read_json(&self.inner.root.join("users.json")).map_err(io_err)
    }

    fn save_users(&self, users: &[UserRecord]) -> Result<(), ApiError> {
        write_json(&self.inner.root.join("users.json"), users).map_err(io_err)
    }

    fn tokens(&self) -> Result<BTreeMap<String, String>, ApiError> {
        read_json(&self.inner.root.join("tokens.json")).map_err(io_err)
    }

    fn save_tokens(&self, tokens: &BTreeMap<String, String>) -> Result<(), ApiError> {
        write_json(&self.inner.root.join("tokens.json"), tokens).map_err(io_err)
    }

    fn meta(&self, name: &str) -> Result<RepoMeta, ApiError> {
        self.ensure_repo(name)?;
        read_json(&self.repo_dir(name).join("meta.json")).map_err(io_err)
    }

    fn policy_of(&self, name: &str) -> Result<Policy, ApiError> {
        self.ensure_repo(name)?;
        read_json(&self.repo_dir(name).join("policy.json")).map_err(io_err)
    }

    fn review_list(&self, name: &str) -> Result<Vec<Review>, ApiError> {
        self.ensure_repo(name)?;
        let path = self.repo_dir(name).join("reviews.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        read_json(&path).map_err(io_err)
    }

    fn save_reviews(&self, name: &str, reviews: &[Review]) -> Result<(), ApiError> {
        write_json(&self.repo_dir(name).join("reviews.json"), reviews).map_err(io_err)
    }

    fn is_admin_user(&self, name: &str) -> Result<bool, ApiError> {
        Ok(self
            .users()?
            .iter()
            .any(|user| user.name == name && user.admin))
    }

    fn branch_owners(&self, name: &str) -> Result<BTreeMap<String, String>, ApiError> {
        let path = self.repo_dir(name).join("branch_owners.json");
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        read_json(&path).map_err(io_err)
    }

    fn save_branch_owners(
        &self,
        name: &str,
        owners: &BTreeMap<String, String>,
    ) -> Result<(), ApiError> {
        write_json(&self.repo_dir(name).join("branch_owners.json"), owners).map_err(io_err)
    }

    fn proposal_list(&self, name: &str) -> Result<Vec<Proposal>, ApiError> {
        self.ensure_repo(name)?;
        let path = self.repo_dir(name).join("pulls.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        read_json(&path).map_err(io_err)
    }

    fn save_proposals(&self, name: &str, items: &[Proposal]) -> Result<(), ApiError> {
        write_json(&self.repo_dir(name).join("pulls.json"), items).map_err(io_err)
    }

    fn open_git(&self, name: &str) -> Result<Repository, ApiError> {
        self.ensure_repo(name)?;
        Repository::open(&self.repo_dir(name).join("git")).map_err(git_err)
    }

    fn ensure_repo(&self, name: &str) -> Result<(), ApiError> {
        if !valid_repo_name(name) || !self.repo_dir(name).join("meta.json").is_file() {
            return Err(ApiError::not_found(format!("仓库不存在: {name}")));
        }
        Ok(())
    }

    fn repo_dir(&self, name: &str) -> PathBuf {
        self.inner.root.join("repos").join(name)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthSession {
    pub token: String,
    pub user: String,
    pub admin: bool,
}

fn registration_open() -> bool {
    match std::env::var("AIT_ALLOW_REGISTER") {
        Ok(value) => {
            let value = value.trim().to_ascii_lowercase();
            !matches!(value.as_str(), "0" | "false" | "no" | "off")
        }
        Err(_) => true,
    }
}

fn unknown_password_hash() -> String {
    use std::sync::OnceLock;
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| password_hash("ait-unknown-user", "\0"))
        .clone()
}

fn can_approve(
    manifest: &Manifest,
    policy: &Policy,
    user: &str,
    admin: bool,
    blocked: &[String],
) -> bool {
    if policy.is_owner(user, admin) {
        return true;
    }
    blocked
        .iter()
        .all(|path| manifest.decide(path) == Access::Write)
}

fn ensure_complete(repo: &Repository, oid: Oid) -> Result<(), ApiError> {
    let mut stack = vec![oid];
    let mut seen = std::collections::HashSet::new();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        match repo.read_object(oid) {
            Ok(Object::Commit(commit)) => {
                stack.push(commit.tree);
                stack.extend(commit.parents);
            }
            Ok(Object::Tree(entries)) => {
                for entry in entries {
                    stack.push(entry.oid);
                }
            }
            Ok(Object::Tag(payload)) => {
                let tag = crate::git::TagObject::parse(&payload)
                    .map_err(|_| ApiError::bad(format!("标签对象 {oid} 无法解析")))?;
                stack.push(tag.object);
            }
            Ok(Object::Blob(_)) => {}
            Err(_) => return Err(ApiError::bad(format!("缺少对象 {oid}，请重新推送完整历史"))),
        }
    }
    Ok(())
}

pub fn normalize_branch(reference: &str) -> Result<String, ApiError> {
    let name = if let Some(rest) = reference.strip_prefix("refs/heads/") {
        rest
    } else if reference.starts_with("refs/") {
        return Err(ApiError::bad("只能推送或拉取分支"));
    } else {
        reference
    };
    if !valid_branch(name) {
        return Err(ApiError::bad(format!("非法分支名: {name}")));
    }
    Ok(format!("refs/heads/{name}"))
}

fn short_ref(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

#[derive(Default)]
struct Landing {
    paths: Vec<String>,
    denied: Vec<String>,
    blocked: Vec<String>,
}

struct Gate {
    landing: Landing,
    limit_reasons: Vec<String>,
    stat: DiffStat,
    checks: Vec<CheckGate>,
    checks_blocking: Vec<String>,
    files: Vec<crate::git::FileDiff>,
    base_oid: Oid,
    head_oid: Oid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirAccess {
    pub path: String,
    pub files: usize,
    /// 除所有者外能直接修改的人。
    pub writers: Vec<String>,
    pub denied: Vec<String>,
    /// 职责描述覆盖这个目录的人。
    pub duties: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicyPreview {
    pub dirs: Vec<DirAccess>,
    pub manifests: Vec<Manifest>,
    pub users: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChecksView {
    pub oid: Option<Oid>,
    pub specs: Vec<CheckSpec>,
    pub config_error: Option<String>,
    pub gates: Vec<CheckGate>,
    pub runs: Vec<CheckRun>,
    /// 服务端执行器；为空表示只接受上报。
    pub runner: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CheckReport {
    #[serde(default)]
    pub reference: String,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub log: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AiConfigUpdate {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub auto: bool,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    /// 不传表示保留原密钥，传空字符串表示清除。
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub instructions: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaimResult {
    pub claim: Claim,
    pub overlaps: Vec<Claim>,
}

struct AiJob {
    title: String,
    body: String,
    author: String,
    manifest: Manifest,
    policy: Policy,
    files: Vec<crate::git::FileDiff>,
    claims: Vec<Claim>,
    plan: String,
    cfg: AiConfig,
    head: Oid,
}

fn verdict_rank(verdict: &str) -> u8 {
    match verdict {
        "request_changes" => 2,
        "comment" => 1,
        _ => 0,
    }
}

fn worse_verdict(a: &str, b: &str) -> String {
    if verdict_rank(b) > verdict_rank(a) {
        b.to_string()
    } else {
        a.to_string()
    }
}

fn verdict_label(verdict: &str) -> &'static str {
    match verdict {
        "request_changes" => "建议修改",
        "comment" => "有提醒",
        _ => "没有发现问题",
    }
}

pub fn check_runner() -> Option<String> {
    std::env::var("AIT_CHECK_RUNNER")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| value == "docker")
}

fn policy_errors(policy: &Policy) -> Vec<String> {
    let mut errors = Vec::new();
    for rule in &policy.rules {
        if rule.access != "write" && rule.access != "deny" {
            errors.push(format!("未知权限类型 {}", rule.access));
        }
        if rule.pattern.trim().is_empty() || rule.pattern.contains("..") {
            errors.push(format!("非法路径规则 {}", rule.pattern));
        }
        if !valid_user_name(&rule.user) && rule.user != "*" {
            errors.push(format!("非法用户 {}", rule.user));
        }
    }
    for owner in &policy.owners {
        if !policy
            .rules
            .iter()
            .any(|rule| rule.user == *owner && rule.pattern == "**" && rule.access == "write")
        {
            errors.push(format!("不能取消所有者 {owner} 对整个仓库的写权限"));
        }
    }
    for duty in &policy.duties {
        if !valid_user_name(&duty.user) {
            errors.push(format!("非法用户 {}", duty.user));
        }
        if duty.paths.iter().any(|path| path.contains("..")) {
            errors.push(format!("{} 的职责路径不能包含 ..", duty.user));
        }
    }
    let mut seen = std::collections::HashSet::new();
    for member in &policy.members {
        if !valid_user_name(&member.user) {
            errors.push(format!("非法用户 {}", member.user));
        }
        if !seen.insert(member.user.clone()) {
            errors.push(format!("成员 {} 重复", member.user));
        }
        if !matches!(member.kind.as_str(), "human" | "agent" | "ci") {
            errors.push(format!("{} 的身份只能是 human、agent 或 ci", member.user));
        }
        if let Some(supervisor) = &member.supervisor {
            if !valid_user_name(supervisor) || supervisor == &member.user {
                errors.push(format!("{} 的负责人不合法", member.user));
            }
        }
        if member.limits.max_files == Some(0) || member.limits.max_lines == Some(0) {
            errors.push(format!("{} 的改动上限必须大于 0", member.user));
        }
        for check in &member.limits.required_checks {
            if !collab::valid_check_name(check) {
                errors.push(format!("{} 的必过校验名不合法: {check}", member.user));
            }
        }
    }
    errors
}

/// 读取某个提交里的 `.ait/checks.toml`；文件不存在时没有校验。
fn checks_at(repo: &Repository, oid: Oid) -> Result<Vec<CheckSpec>, String> {
    match repo.file_at(oid, collab::CHECKS_FILE) {
        Ok((_, _, bytes)) => {
            collab::parse_checks(&String::from_utf8_lossy(&bytes)).map_err(|err| format!("{err:#}"))
        }
        Err(_) => Ok(Vec::new()),
    }
}

fn landing_changes(
    repo: &Repository,
    base: Oid,
    head: Oid,
    manifest: &Manifest,
) -> Result<Landing, ApiError> {
    let from = repo
        .merge_base(base, head)
        .map_err(git_err)?
        .unwrap_or(base);
    let changes = repo.changed_paths(Some(from), head).map_err(git_err)?;
    let mut landing = Landing::default();
    for change in changes {
        if manifest.decide(&change.path) == Access::Deny {
            landing.denied.push(change.path.clone());
        }
        landing.paths.push(change.path);
    }
    Ok(landing)
}

fn review_paths(manifest: &Manifest, paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|path| manifest.decide(path) == Access::Review)
        .cloned()
        .collect()
}

fn writes_all(manifest: &Manifest, paths: &[String]) -> bool {
    paths
        .iter()
        .all(|path| manifest.decide(path) == Access::Write)
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push_str("\n…差异过长，已截断\n");
    }
    out
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("读取 {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn io_err(err: anyhow::Error) -> ApiError {
    ApiError::bad(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_hashed_at_rest_and_bad_logins_look_the_same() {
        let root = std::env::temp_dir().join(format!("ait-auth-{}", random_token()));
        let store = Store::open(root.clone()).unwrap();
        let session = store.register("alice", "secret").unwrap();
        let tokens = std::fs::read_to_string(root.join("tokens.json")).unwrap();
        assert!(!tokens.contains(&session.token), "{tokens}");
        assert!(tokens.contains(&crate::identity::token_id(&session.token)));
        assert_eq!(store.authenticate(&session.token).unwrap().user, "alice");
        store.logout(&session.token).unwrap();
        assert!(store.authenticate(&session.token).is_err());
        let wrong = store.login("alice", "nope").unwrap_err();
        let missing = store.login("nobody", "nope").unwrap_err();
        assert_eq!(wrong.status, missing.status);
        assert_eq!(wrong.message, missing.message);
        assert!(store.create_repo("alice", false, "..", "").is_err());
        assert!(store.repo_summary("..", "http://127.0.0.1:7878").is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
