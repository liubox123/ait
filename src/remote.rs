use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::git::{MergeOutcome, Oid, Repository};
use crate::identity::{self, load_credential, save_credential, split_remote};
use crate::perm::Manifest;
use crate::store::{ObjectOut, PullResponse, PushResponse, RefsResponse};

pub struct Client {
    pub origin: String,
    pub repo: String,
    pub user: String,
    pub token: String,
}

impl Client {
    pub fn from_repo(repo: &Repository) -> anyhow::Result<Self> {
        let url = repo
            .config_get("remote.origin.url")?
            .context("没有 origin。先 ait clone，或 ait remote add origin <url>")?;
        let (origin, name) = split_remote(&url)?;
        let cred = load_credential(&origin).context("请先 ait login 登录这个服务")?;
        Ok(Self {
            origin,
            repo: name,
            user: cred.user,
            token: cred.token,
        })
    }

    pub fn open(origin: &str) -> anyhow::Result<Self> {
        let origin = origin.trim().trim_end_matches('/').to_string();
        let cred = load_credential(&origin).context("请先 ait login 登录这个服务")?;
        Ok(Self {
            origin,
            repo: String::new(),
            user: cred.user,
            token: cred.token,
        })
    }
}

pub fn login(
    origin: &str,
    user: &str,
    password: &str,
) -> anyhow::Result<crate::store::AuthSession> {
    let origin = origin.trim().trim_end_matches('/');
    let session: crate::store::AuthSession = request_json(
        "POST",
        &format!("{origin}/api/login"),
        None,
        Some(&serde_json::json!({ "name": user, "password": password })),
    )?;
    save_credential(origin, &session.user, &session.token)?;
    Ok(session)
}

pub fn register(
    origin: &str,
    user: &str,
    password: &str,
) -> anyhow::Result<crate::store::AuthSession> {
    let origin = origin.trim().trim_end_matches('/');
    let session: crate::store::AuthSession = request_json(
        "POST",
        &format!("{origin}/api/register"),
        None,
        Some(&serde_json::json!({ "name": user, "password": password })),
    )?;
    save_credential(origin, &session.user, &session.token)?;
    Ok(session)
}

pub fn create_repo(
    origin: &str,
    name: &str,
    description: &str,
) -> anyhow::Result<crate::store::RepoMeta> {
    let client = Client::open(origin)?;
    request_json(
        "POST",
        &format!("{}/api/repos", client.origin),
        Some(&client.token),
        Some(&serde_json::json!({ "name": name, "description": description })),
    )
}

pub fn list_repos(origin: &str) -> anyhow::Result<Vec<crate::store::RepoSummary>> {
    let client = Client::open(origin)?;
    request_json(
        "GET",
        &format!("{}/api/repos", client.origin),
        Some(&client.token),
        None::<&()>,
    )
}

pub fn resolve_origin(server: Option<&str>) -> anyhow::Result<String> {
    if let Some(server) = server {
        return Ok(server.trim().trim_end_matches('/').to_string());
    }
    let origins = identity::credential_origins();
    match origins.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("还没有登录。请先 ait login --server http://主机:端口 --user 名字"),
        many => bail!(
            "登录了多个服务，请用 --server 指定其中一个: {}",
            many.join(", ")
        ),
    }
}

pub struct SyncReport {
    pub message: String,
    pub manifest: Manifest,
}

pub fn clone_repo(url: &str, dir: &Path) -> anyhow::Result<SyncReport> {
    if dir.exists() {
        if dir.read_dir()?.next().is_some() {
            bail!("目标目录不为空: {}", dir.display());
        }
    }
    let (origin, name) = split_remote(url)?;
    let cred = load_credential(&origin).context("请先 ait login 登录这个服务")?;
    let client = Client {
        origin,
        repo: name,
        user: cred.user,
        token: cred.token,
    };
    let repo = Repository::init(dir, false)?;
    let remote_url = format!("{}/r/{}", client.origin, client.repo);
    repo.config_set("remote.origin.url", &remote_url)?;
    repo.config_set("branch.main.remote", "origin")?;
    repo.config_set("branch.main.merge", "refs/heads/main")?;
    let manifest = fetch_repo(&repo, &client)?;
    let message = if let Some(oid) = repo.resolve_ref("refs/remotes/origin/main")? {
        let (name, email) = identity::load_identity();
        match repo.merge_oid(oid, "origin/main", &identity::signature_now(&name, &email))? {
            MergeOutcome::FastForward(_) | MergeOutcome::Merged(_) | MergeOutcome::UpToDate => {
                format!("已克隆到 {}，当前提交 {}", dir.display(), oid.short())
            }
            MergeOutcome::Conflicts(_) => "克隆时出现冲突".into(),
        }
    } else {
        format!("已克隆空仓库到 {}", dir.display())
    };
    Ok(SyncReport { message, manifest })
}

pub fn fetch_repo(repo: &Repository, client: &Client) -> anyhow::Result<Manifest> {
    let refs: RefsResponse = request_json(
        "GET",
        &format!("{}/api/git/{}/refs", client.origin, client.repo),
        Some(&client.token),
        None::<&()>,
    )?;
    let mut have: Vec<Oid> = repo
        .list_refs("refs/")?
        .into_iter()
        .map(|(_, oid)| oid)
        .collect();
    for item in &refs.refs {
        let response: PullResponse = request_json(
            "POST",
            &format!("{}/api/git/{}/pull", client.origin, client.repo),
            Some(&client.token),
            Some(&serde_json::json!({ "reference": item.name, "have": have })),
        )?;
        ingest(repo, &response.objects)?;
        if !response.oid.is_zero() {
            let short = item.name.trim_start_matches("refs/heads/");
            repo.set_ref(&format!("refs/remotes/origin/{short}"), response.oid)?;
            have.push(response.oid);
        }
    }
    identity::save_manifest(repo.git_dir(), &refs.manifest)?;
    Ok(refs.manifest)
}

pub fn pull_repo(repo: &Repository) -> anyhow::Result<SyncReport> {
    let client = Client::from_repo(repo)?;
    let manifest = fetch_repo(repo, &client)?;
    let branch = repo.branch_name()?.unwrap_or_else(|| "main".into());
    let Some(theirs) = repo.resolve_ref(&format!("refs/remotes/origin/{branch}"))? else {
        return Ok(SyncReport {
            message: "远端还没有提交".into(),
            manifest,
        });
    };
    let (name, email) = identity::load_identity();
    let message = match repo.merge_oid(
        theirs,
        &format!("origin/{branch}"),
        &identity::signature_now(&name, &email),
    )? {
        MergeOutcome::UpToDate => "已经是最新".into(),
        MergeOutcome::FastForward(oid) => format!("快进到 {}", oid.short()),
        MergeOutcome::Merged(oid) => format!("已合并，提交 {}", oid.short()),
        MergeOutcome::Conflicts(paths) => {
            return Err(anyhow::anyhow!(
                "合并冲突，请修改这些文件后 ait add 再 ait commit:\n{}",
                paths
                    .iter()
                    .map(|path| format!("  {path}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    };
    Ok(SyncReport { message, manifest })
}

pub fn push_repo(repo: &Repository, force: bool) -> anyhow::Result<PushResponse> {
    let client = Client::from_repo(repo)?;
    let branch = repo.branch_name()?.context("当前是分离 HEAD，不能推送")?;
    let head = repo.head_oid()?.context("没有可推送的提交")?;
    let reference = format!("refs/heads/{branch}");
    let refs: RefsResponse = request_json(
        "GET",
        &format!("{}/api/git/{}/refs", client.origin, client.repo),
        Some(&client.token),
        None::<&()>,
    )?;
    let remote_oid = refs
        .refs
        .iter()
        .find(|item| item.name == reference)
        .map(|item| item.oid)
        .unwrap_or(Oid::ZERO);
    let response = push_objects(repo, &client, &reference, remote_oid, head, force, false)?;
    if response.status == "updated" {
        repo.set_ref(&format!("refs/remotes/origin/{branch}"), head)?;
    }
    identity::save_manifest(repo.git_dir(), &response.manifest)?;
    Ok(response)
}

fn push_objects(
    repo: &Repository,
    client: &Client,
    reference: &str,
    remote_oid: Oid,
    head: Oid,
    force: bool,
    everything: bool,
) -> anyhow::Result<PushResponse> {
    let have: Vec<Oid> = if everything || remote_oid.is_zero() {
        Vec::new()
    } else {
        vec![remote_oid]
    };
    let objects = repo.objects_since(head, &have)?;
    let payload: Vec<_> = objects
        .iter()
        .map(|object| {
            serde_json::json!({
                "kind": object.kind,
                "payload_b64": STANDARD.encode(&object.payload),
            })
        })
        .collect();
    let result: anyhow::Result<PushResponse> = request_json(
        "POST",
        &format!("{}/api/git/{}/push", client.origin, client.repo),
        Some(&client.token),
        Some(&serde_json::json!({
            "reference": reference,
            "old_oid": remote_oid,
            "new_oid": head,
            "force": force,
            "objects": payload,
        })),
    );
    match result {
        Err(err) if !everything && err.to_string().contains("缺少对象") => {
            push_objects(repo, client, reference, remote_oid, head, force, true)
        }
        other => other,
    }
}

pub fn reviews(repo: &Repository) -> anyhow::Result<Vec<crate::store::ReviewView>> {
    let client = Client::from_repo(repo)?;
    request_json(
        "GET",
        &format!("{}/api/repos/{}/reviews", client.origin, client.repo),
        Some(&client.token),
        None::<&()>,
    )
}

pub fn approve(repo: &Repository, id: &str) -> anyhow::Result<crate::store::Review> {
    let client = Client::from_repo(repo)?;
    request_json(
        "POST",
        &format!(
            "{}/api/repos/{}/reviews/{id}/approve",
            client.origin, client.repo
        ),
        Some(&client.token),
        Some(&serde_json::json!({})),
    )
}

pub fn reject(repo: &Repository, id: &str, reason: &str) -> anyhow::Result<crate::store::Review> {
    let client = Client::from_repo(repo)?;
    request_json(
        "POST",
        &format!(
            "{}/api/repos/{}/reviews/{id}/reject",
            client.origin, client.repo
        ),
        Some(&client.token),
        Some(&serde_json::json!({ "reason": reason })),
    )
}

pub fn apply_plan(
    repo: &Repository,
    content: &str,
    llm: bool,
) -> anyhow::Result<crate::store::PlanView> {
    let client = Client::from_repo(repo)?;
    request_json(
        "PUT",
        &format!("{}/api/repos/{}/plan", client.origin, client.repo),
        Some(&client.token),
        Some(&serde_json::json!({ "content": content, "apply": true, "use_llm": llm })),
    )
}

pub fn plan_text(repo: &Repository) -> anyhow::Result<String> {
    let client = Client::from_repo(repo)?;
    let value: serde_json::Value = request_json(
        "GET",
        &format!("{}/api/repos/{}/plan", client.origin, client.repo),
        Some(&client.token),
        None::<&()>,
    )?;
    Ok(value["content"].as_str().unwrap_or("").to_string())
}

pub fn close_proposal(repo: &Repository, id: &str) -> anyhow::Result<crate::store::ProposalView> {
    let client = Client::from_repo(repo)?;
    request_json(
        "POST",
        &format!(
            "{}/api/repos/{}/pulls/{id}/close",
            client.origin, client.repo
        ),
        Some(&client.token),
        Some(&serde_json::json!({})),
    )
}

pub fn merge_proposal(repo: &Repository, id: &str) -> anyhow::Result<crate::store::ProposalView> {
    let client = Client::from_repo(repo)?;
    request_json(
        "POST",
        &format!(
            "{}/api/repos/{}/pulls/{id}/merge",
            client.origin, client.repo
        ),
        Some(&client.token),
        Some(&serde_json::json!({})),
    )
}

pub fn approve_proposal(repo: &Repository, id: &str) -> anyhow::Result<crate::store::ProposalView> {
    let client = Client::from_repo(repo)?;
    request_json(
        "POST",
        &format!(
            "{}/api/repos/{}/pulls/{id}/approve",
            client.origin, client.repo
        ),
        Some(&client.token),
        Some(&serde_json::json!({})),
    )
}

pub fn proposals(repo: &Repository) -> anyhow::Result<Vec<crate::store::ProposalView>> {
    let client = Client::from_repo(repo)?;
    request_json(
        "GET",
        &format!("{}/api/repos/{}/pulls", client.origin, client.repo),
        Some(&client.token),
        None::<&()>,
    )
}

pub fn create_proposal(
    repo: &Repository,
    title: &str,
    base: &str,
) -> anyhow::Result<crate::store::ProposalView> {
    let client = Client::from_repo(repo)?;
    let head = repo
        .branch_name()?
        .context("当前是分离 HEAD，不能创建合并请求")?;
    request_json(
        "POST",
        &format!("{}/api/repos/{}/pulls", client.origin, client.repo),
        Some(&client.token),
        Some(&serde_json::json!({
            "title": title,
            "base": base,
            "head": head,
        })),
    )
}

pub fn policy(repo: &Repository) -> anyhow::Result<crate::perm::Policy> {
    let client = Client::from_repo(repo)?;
    request_json(
        "GET",
        &format!("{}/api/repos/{}/policy", client.origin, client.repo),
        Some(&client.token),
        None::<&()>,
    )
}

fn ingest(repo: &Repository, objects: &[ObjectOut]) -> anyhow::Result<()> {
    for object in objects {
        let bytes = STANDARD
            .decode(object.payload_b64.trim())
            .context("远端对象不是合法 base64")?;
        let oid = repo.ingest(&object.kind, &bytes)?;
        if oid != object.oid {
            bail!("远端对象 {} 与内容不一致（实际是 {oid}）", object.oid);
        }
    }
    Ok(())
}

fn request_json<T: DeserializeOwned, B: Serialize>(
    method: &str,
    url: &str,
    token: Option<&str>,
    body: Option<&B>,
) -> anyhow::Result<T> {
    let value = request_value(method, url, token, body)?;
    Ok(serde_json::from_value(value)?)
}

fn request_value<B: Serialize>(
    method: &str,
    url: &str,
    token: Option<&str>,
    body: Option<&B>,
) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let method = reqwest::Method::from_bytes(method.as_bytes())?;
    let mut request = client.request(method, url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().with_context(|| format!("连接 {url} 失败"))?;
    let status = response.status();
    let text = response.text().context("读取响应失败")?;
    if !status.is_success() {
        let message = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string())
            })
            .unwrap_or(text);
        bail!("{message}");
    }
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    Ok(serde_json::from_str(&text).context("服务端响应不是 JSON")?)
}
