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

use crate::git::{valid_branch, Object, Oid, Repository};
use crate::identity::{password_eq, password_hash, random_token, valid_repo_name, valid_user_name};
use crate::perm::{Access, Duty, Manifest, Policy, Rule};
use crate::plan::{self, Assignment};

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
    pub paths: Vec<String>,
    pub can_approve: bool,
    pub can_merge: bool,
    pub can_close: bool,
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
        let _guard = self.lock();
        if !valid_user_name(name) {
            return Err(ApiError::bad(
                "用户名只能包含字母、数字、点、下划线和短横线",
            ));
        }
        if password.is_empty() {
            return Err(ApiError::bad("密码不能为空"));
        }
        let mut users = self.users()?;
        if users.iter().any(|user| user.name == name) {
            return Err(ApiError::conflict("用户已存在"));
        }
        let admin = users.is_empty();
        let salt = random_token();
        users.push(UserRecord {
            name: name.to_string(),
            salt: salt.clone(),
            password_hash: password_hash(&salt, password),
            admin,
        });
        self.save_users(&users)?;
        self.issue_token(name)
    }

    pub fn login(&self, name: &str, password: &str) -> Result<AuthSession, ApiError> {
        let _guard = self.lock();
        let users = self.users()?;
        let Some(user) = users.iter().find(|user| user.name == name) else {
            return Err(ApiError::unauthorized("用户名或密码错误"));
        };
        if !password_eq(&user.password_hash, &password_hash(&user.salt, password)) {
            return Err(ApiError::unauthorized("用户名或密码错误"));
        }
        self.issue_token(name)
    }

    pub fn logout(&self, token: &str) -> Result<(), ApiError> {
        let _guard = self.lock();
        let mut tokens = self.tokens()?;
        tokens.remove(token);
        self.save_tokens(&tokens)
    }

    pub fn authenticate(&self, token: &str) -> Result<AuthSession, ApiError> {
        let _guard = self.lock();
        let tokens = self.tokens()?;
        let Some(name) = tokens.get(token) else {
            return Err(ApiError::unauthorized("登录已失效"));
        };
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
        tokens.insert(token.clone(), name.to_string());
        self.save_tokens(&tokens)?;
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
    ) -> Result<Policy, ApiError> {
        let _guard = self.lock();
        let mut policy = self.policy_of(name)?;
        self.require_owner(&policy, user, admin)?;
        for rule in &rules {
            if rule.access != "write" && rule.access != "deny" {
                return Err(ApiError::bad(format!("未知权限类型 {}", rule.access)));
            }
            if rule.pattern.trim().is_empty() || rule.pattern.contains("..") {
                return Err(ApiError::bad(format!("非法路径规则 {}", rule.pattern)));
            }
            if !valid_user_name(&rule.user) && rule.user != "*" {
                return Err(ApiError::bad(format!("非法用户 {}", rule.user)));
            }
        }
        for owner in &policy.owners {
            if !rules
                .iter()
                .any(|rule| rule.user == *owner && rule.pattern == "**" && rule.access == "write")
            {
                return Err(ApiError::bad(format!(
                    "不能取消所有者 {owner} 对整个仓库的写权限"
                )));
            }
        }
        policy.rules = rules;
        policy.duties = duties;
        write_json(&self.repo_dir(name).join("policy.json"), &policy).map_err(io_err)?;
        Ok(policy)
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
        let preview = plan::policy_from_assignments(&policy.owners, &parsed);
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
        Ok(RefsResponse { refs, manifest })
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
        if request.new_oid.is_zero() {
            return Err(ApiError::bad("不能推送空提交"));
        }
        let reference = normalize_branch(&request.reference)?;
        let repo = self.open_git(name)?;
        for object in &request.objects {
            let bytes = STANDARD
                .decode(object.payload_b64.trim())
                .map_err(|_| ApiError::bad("对象内容不是合法的 base64"))?;
            repo.ingest(&object.kind, &bytes).map_err(git_err)?;
        }
        ensure_complete(&repo, request.new_oid)?;
        let current = repo
            .resolve_ref(&reference)
            .map_err(git_err)?
            .unwrap_or(Oid::ZERO);
        let policy = self.policy_of(name)?;
        let owner = policy.is_owner(user, admin);
        let protected = is_protected(&reference);
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
        if current != request.old_oid && !(request.force && can_force) {
            return Err(ApiError::conflict(format!(
                "远端 {reference} 已是 {current}，请先拉取"
            )));
        }
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
        if !protected {
            repo.set_ref(&reference, request.new_oid).map_err(git_err)?;
            if creator.is_none() {
                branch_owners.insert(reference.clone(), user.to_string());
                self.save_branch_owners(name, &branch_owners)?;
            }
            self.note_branch_moved(name, &reference, request.new_oid)?;
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
                    message: "修改了写权限之外的路径".into(),
                    blocked: blocked.clone(),
                    paths,
                    created_at: now_secs(),
                    decided_by: None,
                    decision_reason: None,
                },
            )?;
            return Ok(PushResponse {
                status: "review".into(),
                reference,
                oid: request.new_oid,
                review_id: Some(review.id.clone()),
                blocked,
                message: format!("超出写权限，已提交评审 {}", review.id),
                manifest,
            });
        }
        repo.set_ref(&reference, request.new_oid).map_err(git_err)?;
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
            let can_approve = review.status == "pending"
                && can_approve(&manifest, &policy, user, admin, &review.blocked);
            views.push(ReviewView {
                review,
                diff,
                can_approve,
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
        if approve {
            let repo = self.open_git(name)?;
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
        let landing = self.landing_of(&repo, name, proposal)?;
        if landing.blocked.is_empty() {
            return Err(ApiError::bad("这些更改都在作者的写权限内，可以直接合并"));
        }
        if !landing.denied.is_empty() {
            return Err(ApiError::forbidden(format!(
                "这些路径禁止修改: {}",
                landing.denied.join(", ")
            )));
        }
        let policy = self.policy_of(name)?;
        let manifest = policy.manifest_for(user, admin || self.is_admin_user(user)?);
        if !writes_all(&manifest, &landing.blocked) {
            return Err(ApiError::forbidden("你没有这些路径的写权限，不能通过"));
        }
        if !proposal.approvals.iter().any(|name| name == user) {
            proposal.approvals.push(user.to_string());
        }
        self.save_proposals(name, &items)?;
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
        self.save_proposals(name, &items)?;
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
        let base_oid = repo
            .resolve_ref(&base_ref)
            .map_err(git_err)?
            .ok_or_else(|| ApiError::not_found("目标分支不存在"))?;
        let head_oid = repo
            .resolve_ref(&head_ref)
            .map_err(git_err)?
            .ok_or_else(|| ApiError::not_found("来源分支不存在"))?;
        let policy = self.policy_of(name)?;
        let author_admin = self.is_admin_user(&author)?;
        let author_manifest = policy.manifest_for(&author, author_admin);
        let landing = landing_changes(&repo, base_oid, head_oid, &author_manifest)?;
        if !landing.denied.is_empty() {
            return Err(ApiError::forbidden(format!(
                "这些路径禁止修改: {}",
                landing.denied.join(", ")
            )));
        }
        let blocked = review_paths(&author_manifest, &landing.paths);
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
        self.one_proposal(name, user, admin, id)
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
            let base_oid = repo
                .resolve_ref(&proposal.base_ref)
                .map_err(git_err)?
                .unwrap_or(Oid::ZERO);
            let head_oid = repo
                .resolve_ref(&proposal.head_ref)
                .map_err(git_err)?
                .unwrap_or(Oid::ZERO);
            let author_admin = self.is_admin_user(&proposal.author)?;
            let author_manifest = policy.manifest_for(&proposal.author, author_admin);
            let landing = if base_oid.is_zero() || head_oid.is_zero() {
                Landing::default()
            } else {
                landing_changes(&repo, base_oid, head_oid, &author_manifest).unwrap_or_default()
            };
            let blocked = review_paths(&author_manifest, &landing.paths);
            let diff = if base_oid.is_zero() || head_oid.is_zero() {
                String::new()
            } else {
                match repo.merge_base(base_oid, head_oid) {
                    Ok(Some(base)) => match repo.diff_range(Some(base), head_oid) {
                        Ok(files) => truncate_text(&crate::git::unified_diff(&files), 200_000),
                        Err(err) => format!("无法生成差异: {err}"),
                    },
                    _ => String::new(),
                }
            };
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
            views.push(ProposalView {
                proposal,
                diff,
                blocked,
                paths: landing.paths,
                can_approve,
                can_merge,
                can_close,
            });
        }
        views.sort_by(|a, b| b.proposal.created_at.cmp(&a.proposal.created_at));
        Ok(views)
    }

    fn landing_of(
        &self,
        repo: &Repository,
        _name: &str,
        proposal: &Proposal,
    ) -> Result<Landing, ApiError> {
        let base_oid = repo
            .resolve_ref(&proposal.base_ref)
            .map_err(git_err)?
            .ok_or_else(|| ApiError::not_found("目标分支不存在"))?;
        let head_oid = repo
            .resolve_ref(&proposal.head_ref)
            .map_err(git_err)?
            .ok_or_else(|| ApiError::not_found("来源分支不存在"))?;
        let author_admin = self.is_admin_user(&proposal.author)?;
        let policy = self.policy_of(_name)?;
        let manifest = policy.manifest_for(&proposal.author, author_admin);
        let mut landing = landing_changes(repo, base_oid, head_oid, &manifest)?;
        landing.blocked = review_paths(&manifest, &landing.paths);
        Ok(landing)
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
            Ok(Object::Blob(_) | Object::Tag(_)) => {}
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

fn is_protected(reference: &str) -> bool {
    matches!(short_ref(reference), "main" | "master")
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
