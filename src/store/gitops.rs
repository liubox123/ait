//! 服务端的 Git 传输与账号补充：按 pack 收发对象、删分支、推标签、受保护分支、
//! 改密码、注销全部会话、令牌过期，以及 Git smart HTTP 推送命令的处理。

use std::collections::{BTreeMap, HashSet};

use base64::Engine;

use super::{
    git_err, io_err, now_secs, read_json, short_ref, write_json, ApiError, AuthSession,
    PushRequest, PushResponse, Store,
};
use crate::git::{Object, Oid, TagObject};
use crate::identity::{password_hash, password_matches, random_token, token_id};

/// 默认令牌有效期（天），可用 `AIT_TOKEN_TTL_DAYS` 调整，0 表示不过期。
const DEFAULT_TOKEN_DAYS: i64 = 30;

/// smart HTTP 推送里的一条引用更新。
#[derive(Clone, Debug)]
pub struct RefCommand {
    pub old: Oid,
    pub new: Oid,
    pub reference: String,
}

impl Store {
    /// 所有分支和标签，附注标签同时给出剥开后的提交。
    pub fn advertised_refs(&self, name: &str) -> Result<Vec<(String, Oid, Option<Oid>)>, ApiError> {
        let _guard = self.lock();
        let repo = self.open_git(name)?;
        let mut out = Vec::new();
        for (refname, oid) in repo.list_refs("refs/").map_err(git_err)? {
            if !(refname.starts_with("refs/heads/") || refname.starts_with("refs/tags/")) {
                continue;
            }
            let peeled = match repo.read_object(oid) {
                Ok(Object::Tag(_)) => repo.peel_tags(oid).ok(),
                _ => None,
            };
            out.push((refname, oid, peeled));
        }
        Ok(out)
    }

    /// 服务端仓库 HEAD 指向的分支。
    pub fn head_branch(&self, name: &str) -> Result<Option<String>, ApiError> {
        let repo = self.open_git(name)?;
        Ok(match repo.head().map_err(git_err)? {
            crate::git::Head::Symbolic(target) => Some(target),
            crate::git::Head::Detached(_) => None,
        })
    }

    /// 生成包含 `wants`、但不含 `haves` 可达对象的 pack。`wants` 必须是某个分支或标签可达的提交或标签。
    pub fn fetch_pack(
        &self,
        name: &str,
        wants: &[Oid],
        haves: &[Oid],
        include_tags: bool,
    ) -> Result<Vec<u8>, ApiError> {
        let refs = self.advertised_refs(name)?;
        let _guard = self.lock();
        let repo = self.open_git(name)?;
        let tips: HashSet<Oid> = refs
            .iter()
            .flat_map(|(_, oid, peeled)| [Some(*oid), *peeled])
            .flatten()
            .collect();
        let mut reachable = HashSet::new();
        let mut checked = false;
        for want in wants {
            if tips.contains(want) {
                continue;
            }
            if !checked {
                for tip in &tips {
                    let mut seen = HashSet::new();
                    collect_commits(&repo, *tip, &mut seen);
                    reachable.extend(seen);
                }
                checked = true;
            }
            if !reachable.contains(want) {
                return Err(ApiError::forbidden(format!("{want} 不在任何分支或标签上")));
            }
        }
        let mut targets: Vec<Oid> = wants.to_vec();
        if include_tags {
            let mut sent = HashSet::new();
            for want in wants {
                collect_commits(&repo, *want, &mut sent);
            }
            for (refname, oid, peeled) in &refs {
                if refname.starts_with("refs/tags/") {
                    if let Some(target) = peeled {
                        if sent.contains(target) && !targets.contains(oid) {
                            targets.push(*oid);
                        }
                    }
                }
            }
        }
        let haves: Vec<Oid> = haves.iter().copied().filter(|oid| repo.has_object(*oid)).collect();
        repo.pack_for(&targets, &haves).map_err(git_err)
    }

    /// `oids` 中服务端已有的对象。
    pub fn common_objects(&self, name: &str, oids: &[Oid]) -> Result<Vec<Oid>, ApiError> {
        let _guard = self.lock();
        let repo = self.open_git(name)?;
        Ok(oids.iter().copied().filter(|oid| repo.has_object(*oid)).collect())
    }

    /// 解码推送请求里的对象并放进仓库的隔离区（调用方持有锁）。
    pub(super) fn stage_request(
        &self,
        repo: &crate::git::Repository,
        request: &PushRequest,
    ) -> Result<(), ApiError> {
        for object in &request.objects {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(object.payload_b64.trim())
                .map_err(|_| ApiError::bad("对象内容不是合法的 base64"))?;
            repo.stage_object(&object.kind, &bytes).map_err(git_err)?;
        }
        if let Some(pack) = &request.pack_b64 {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(pack.trim())
                .map_err(|_| ApiError::bad("pack 不是合法的 base64"))?;
            repo.stage_pack(&bytes).map_err(git_err)?;
        }
        for (_, kind, payload) in &request.staged {
            repo.stage_object(kind, payload).map_err(git_err)?;
        }
        Ok(())
    }

    /// 删除分支（调用方持有锁）。受保护分支不能删；别人建的分支只有所有者能删。
    pub(super) fn delete_branch_locked(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        request: &PushRequest,
    ) -> Result<PushResponse, ApiError> {
        let reference = super::normalize_branch(&request.reference)?;
        let repo = self.open_git(name)?;
        let policy = self.policy_of(name)?;
        let owner = policy.is_owner(user, admin);
        if self.protected_branch(name, &reference) {
            return Err(ApiError::forbidden(format!(
                "{} 是受保护分支，不能删除",
                short_ref(&reference)
            )));
        }
        let mut owners = self.branch_owners(name)?;
        let creator = owners.get(&reference).cloned();
        if !owner && creator.as_deref() != Some(user) {
            return Err(ApiError::forbidden(format!(
                "只能删除自己创建的分支 {}",
                short_ref(&reference)
            )));
        }
        let current = repo
            .resolve_ref(&reference)
            .map_err(git_err)?
            .ok_or_else(|| ApiError::not_found(format!("分支不存在: {}", short_ref(&reference))))?;
        if !request.old_oid.is_zero() && request.old_oid != current && !request.force {
            return Err(ApiError::conflict(format!(
                "远端 {reference} 已是 {current}，请先拉取"
            )));
        }
        repo.delete_ref(&reference).map_err(git_err)?;
        if owners.remove(&reference).is_some() {
            self.save_branch_owners(name, &owners)?;
        }
        Ok(PushResponse {
            status: "deleted".into(),
            reference,
            oid: Oid::ZERO,
            review_id: None,
            blocked: Vec::new(),
            message: "已删除分支".into(),
            manifest: policy.manifest_for(user, admin),
        })
    }

    /// 推送、移动或删除标签（调用方持有锁）。只有仓库所有者可以操作标签，移动已有标签需要 force。
    pub(super) fn push_tag_locked(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        request: &PushRequest,
    ) -> Result<PushResponse, ApiError> {
        let tag = request
            .reference
            .strip_prefix("refs/tags/")
            .unwrap_or(&request.reference);
        if !crate::git::valid_branch(tag) {
            return Err(ApiError::bad(format!("非法标签名: {tag}")));
        }
        let reference = format!("refs/tags/{tag}");
        let repo = self.open_git(name)?;
        let policy = self.policy_of(name)?;
        if !policy.is_owner(user, admin) {
            return Err(ApiError::forbidden("只有仓库所有者可以推送或删除标签"));
        }
        let current = repo.resolve_ref(&reference).map_err(git_err)?;
        let manifest = policy.manifest_for(user, admin);
        if request.new_oid.is_zero() {
            let Some(_) = current else {
                return Err(ApiError::not_found(format!("标签不存在: {tag}")));
            };
            repo.delete_ref(&reference).map_err(git_err)?;
            return Ok(PushResponse {
                status: "deleted".into(),
                reference,
                oid: Oid::ZERO,
                review_id: None,
                blocked: Vec::new(),
                message: "已删除标签".into(),
                manifest,
            });
        }
        if let Some(existing) = current {
            if existing == request.new_oid {
                return Ok(PushResponse {
                    status: "unchanged".into(),
                    reference,
                    oid: existing,
                    review_id: None,
                    blocked: Vec::new(),
                    message: "标签没有变化".into(),
                    manifest,
                });
            }
            if !request.force {
                return Err(ApiError::conflict(format!("标签 {tag} 已存在，移动标签需要 --force")));
            }
        }
        self.stage_request(&repo, request)?;
        super::ensure_complete(&repo, request.new_oid)?;
        repo.persist_staged().map_err(git_err)?;
        repo.set_ref(&reference, request.new_oid).map_err(git_err)?;
        Ok(PushResponse {
            status: "updated".into(),
            reference,
            oid: request.new_oid,
            review_id: None,
            blocked: Vec::new(),
            message: "已更新标签".into(),
            manifest,
        })
    }

    /// 处理 smart HTTP 推送：先解开 pack，再逐条走与 ait 推送相同的权限检查。
    /// 返回每条引用的结果：`Ok(说明)` 或 `Err(拒绝原因)`。
    pub fn receive_commands(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        commands: &[RefCommand],
        pack: &[u8],
    ) -> Result<Vec<(String, Result<PushResponse, String>)>, ApiError> {
        let (objects, ff) = {
            let _guard = self.lock();
            let repo = self.open_git(name)?;
            let objects = if pack.is_empty() {
                Vec::new()
            } else {
                crate::git::pack::unpack(pack, &|oid| {
                    let object = repo.read_object(oid).ok()?;
                    Some((crate::git::pack::static_kind(object.kind())?, object.payload()))
                })
                .map_err(|err| ApiError::bad(format!("pack 无法解开: {err:#}")))?
            };
            for (_, kind, payload) in &objects {
                repo.stage_object(kind, payload).map_err(git_err)?;
            }
            let ff: Vec<bool> = commands
                .iter()
                .map(|cmd| {
                    cmd.old.is_zero()
                        || cmd.new.is_zero()
                        || repo.is_ancestor(cmd.old, cmd.new).unwrap_or(false)
                })
                .collect();
            (objects, ff)
        };
        let mut results = Vec::new();
        for (cmd, fast_forward) in commands.iter().zip(ff) {
            let request = PushRequest {
                reference: cmd.reference.clone(),
                old_oid: cmd.old,
                new_oid: cmd.new,
                objects: Vec::new(),
                force: !fast_forward,
                pack_b64: None,
                staged: objects.clone(),
            };
            let outcome = self
                .push(name, user, admin, request)
                .map_err(|err| err.message.replace('\n', " "));
            results.push((cmd.reference.clone(), outcome));
        }
        Ok(results)
    }

    // ---------- 受保护分支 ----------

    pub fn protected_patterns(&self, name: &str) -> Result<Vec<String>, ApiError> {
        self.ensure_repo(name)?;
        let path = self.repo_dir(name).join("protected.json");
        if !path.exists() {
            return Ok(vec!["main".into(), "master".into()]);
        }
        read_json(&path).map_err(io_err)
    }

    pub fn set_protected(
        &self,
        name: &str,
        user: &str,
        admin: bool,
        patterns: Vec<String>,
    ) -> Result<Vec<String>, ApiError> {
        let _guard = self.lock();
        let policy = self.policy_of(name)?;
        self.require_owner(&policy, user, admin)?;
        let mut cleaned = Vec::new();
        for pattern in patterns {
            let pattern = pattern.trim().trim_start_matches("refs/heads/").to_string();
            if pattern.is_empty() {
                continue;
            }
            if pattern.contains("..") || pattern.contains(' ') || pattern.starts_with('/') {
                return Err(ApiError::bad(format!("非法分支模式: {pattern}")));
            }
            if !cleaned.contains(&pattern) {
                cleaned.push(pattern);
            }
        }
        write_json(&self.repo_dir(name).join("protected.json"), &cleaned).map_err(io_err)?;
        Ok(cleaned)
    }

    pub(super) fn protected_branch(&self, name: &str, reference: &str) -> bool {
        let branch = short_ref(reference);
        self.protected_patterns(name)
            .unwrap_or_else(|_| vec!["main".into(), "master".into()])
            .iter()
            .any(|pattern| crate::perm::path_match(pattern, branch))
    }

    // ---------- 账号 ----------

    /// 校验用户名和口令（或该用户的有效令牌），不签发新令牌。供 smart HTTP 的 Basic 认证使用。
    pub fn verify_basic(&self, user: &str, secret: &str) -> Result<AuthSession, ApiError> {
        if let Ok(session) = self.authenticate(secret) {
            if session.user == user {
                return Ok(session);
            }
        }
        let snapshot = {
            let _guard = self.lock();
            self.users()?
                .into_iter()
                .find(|record| record.name == user)
                .map(|record| (record.salt, record.password_hash, record.admin))
        };
        let (salt, stored, admin) = match snapshot {
            Some(found) => found,
            None => ("ait-unknown-user".to_string(), super::unknown_password_hash(), false),
        };
        let ok = password_matches(&stored, &salt, secret);
        if !ok || stored == super::unknown_password_hash() {
            return Err(ApiError::unauthorized("用户名或密码错误"));
        }
        Ok(AuthSession {
            token: String::new(),
            user: user.to_string(),
            admin,
        })
    }

    /// 修改口令，注销该用户的全部会话，并签发一个新令牌。
    pub fn change_password(
        &self,
        user: &str,
        old: &str,
        new: &str,
    ) -> Result<AuthSession, ApiError> {
        if new.is_empty() {
            return Err(ApiError::bad("新密码不能为空"));
        }
        let snapshot = {
            let _guard = self.lock();
            self.users()?
                .into_iter()
                .find(|record| record.name == user)
                .map(|record| (record.salt, record.password_hash))
        };
        let Some((salt, stored)) = snapshot else {
            return Err(ApiError::unauthorized("用户不存在"));
        };
        if !password_matches(&stored, &salt, old) {
            return Err(ApiError::unauthorized("原密码错误"));
        }
        let new_salt = random_token();
        let hash = password_hash(&new_salt, new);
        let _guard = self.lock();
        let mut users = self.users()?;
        let Some(record) = users.iter_mut().find(|record| record.name == user) else {
            return Err(ApiError::unauthorized("用户不存在"));
        };
        if record.password_hash != stored {
            return Err(ApiError::conflict("密码刚被修改过，请重试"));
        }
        record.salt = new_salt;
        record.password_hash = hash;
        self.save_users(&users)?;
        self.revoke_user_tokens(user)?;
        self.issue_token(user)
    }

    /// 注销用户的全部会话，返回注销的数量。
    pub fn logout_all(&self, user: &str) -> Result<usize, ApiError> {
        let _guard = self.lock();
        self.revoke_user_tokens(user)
    }

    fn revoke_user_tokens(&self, user: &str) -> Result<usize, ApiError> {
        let mut tokens = self.tokens()?;
        let before = tokens.len();
        let removed: Vec<String> = tokens
            .iter()
            .filter(|(_, owner)| owner.as_str() == user)
            .map(|(id, _)| id.clone())
            .collect();
        tokens.retain(|_, owner| owner != user);
        self.save_tokens(&tokens)?;
        let mut times = self.token_times()?;
        for id in removed {
            times.remove(&id);
        }
        self.save_token_times(&times)?;
        Ok(before - tokens.len())
    }

    fn token_times(&self) -> Result<BTreeMap<String, i64>, ApiError> {
        let path = self.inner.root.join("token_times.json");
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        read_json(&path).map_err(io_err)
    }

    fn save_token_times(&self, times: &BTreeMap<String, i64>) -> Result<(), ApiError> {
        write_json(&self.inner.root.join("token_times.json"), times).map_err(io_err)
    }

    /// 记下令牌签发时间（调用方持有锁）。
    pub(super) fn note_token_issued(&self, token: &str) -> Result<(), ApiError> {
        let mut times = self.token_times()?;
        times.insert(token_id(token), now_secs());
        self.save_token_times(&times)
    }

    pub(super) fn forget_token_time(&self, token: &str) -> Result<(), ApiError> {
        let mut times = self.token_times()?;
        if times.remove(&token_id(token)).is_some() {
            self.save_token_times(&times)?;
        }
        Ok(())
    }

    /// 令牌是否已过期（调用方持有锁）。没有记录签发时间的旧令牌从现在开始计时。
    pub(super) fn token_expired(&self, token: &str) -> Result<bool, ApiError> {
        let ttl = token_ttl_secs();
        if ttl <= 0 {
            return Ok(false);
        }
        let id = token_id(token);
        let mut times = self.token_times()?;
        match times.get(&id) {
            Some(issued) => Ok(now_secs() - issued > ttl),
            None => {
                times.insert(id, now_secs());
                self.save_token_times(&times)?;
                Ok(false)
            }
        }
    }

    /// 删除令牌（调用方持有锁）。
    pub(super) fn drop_token(&self, token: &str) -> Result<(), ApiError> {
        let mut tokens = self.tokens()?;
        tokens.remove(&token_id(token));
        self.save_tokens(&tokens)?;
        self.forget_token_time(token)
    }
}

fn token_ttl_secs() -> i64 {
    let days = std::env::var("AIT_TOKEN_TTL_DAYS")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT_TOKEN_DAYS);
    days.saturating_mul(86400)
}

fn collect_commits(repo: &crate::git::Repository, start: Oid, out: &mut HashSet<Oid>) {
    let mut stack = vec![start];
    while let Some(oid) = stack.pop() {
        if oid.is_zero() || !out.insert(oid) {
            continue;
        }
        match repo.read_object(oid) {
            Ok(Object::Commit(commit)) => stack.extend(commit.parents),
            Ok(Object::Tag(payload)) => {
                if let Ok(tag) = TagObject::parse(&payload) {
                    stack.push(tag.object);
                }
            }
            _ => {}
        }
    }
}
