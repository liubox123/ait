//! 多远端、上游跟踪、按 pack 收发对象、推送引用规格、`pull --rebase` / `--ff-only`。

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;

use super::{request_json, Client, SyncReport};
use crate::git::{MergeOptions, MergeOutcome, Oid, Repository, SeqOutcome};
use crate::identity::{self, load_credential, split_remote};
use crate::perm::Manifest;
use crate::store::{PushResponse, RefsResponse};

impl Client {
    /// 按远端名取地址和登录凭据。
    pub fn from_remote(repo: &Repository, name: &str) -> anyhow::Result<Self> {
        let url = repo
            .config_get(&format!("remote.{name}.url"))?
            .with_context(|| format!("没有远端 {name}。先 ait remote add {name} <url>"))?;
        let (origin, repo_name) = split_remote(&url)?;
        let cred = load_credential(&origin).context("请先 ait login 登录这个服务")?;
        Ok(Self {
            origin,
            repo: repo_name,
            user: cred.user,
            token: cred.token,
        })
    }
}

pub struct FetchReport {
    pub updated: Vec<String>,
    pub pruned: Vec<String>,
    pub manifest: Manifest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullMode {
    Merge,
    Rebase,
    FastForwardOnly,
}

pub struct PushOptions {
    pub remote: Option<String>,
    pub refspecs: Vec<String>,
    pub force: bool,
    pub set_upstream: bool,
    pub tags: bool,
    pub delete: bool,
}

pub struct PushResult {
    pub local: String,
    pub remote: String,
    pub old: Oid,
    pub new: Oid,
    pub forced: bool,
    /// `new`、`updated`、`forced`、`deleted`、`up-to-date`、`review`、`rejected`。
    pub status: String,
    pub message: String,
    pub blocked: Vec<String>,
    pub manifest: Option<Manifest>,
}

impl PushResult {
    pub fn line(&self) -> String {
        let local = short_name(&self.local);
        let remote = short_name(&self.remote);
        let pair = if local.is_empty() {
            remote.to_string()
        } else {
            format!("{local} -> {remote}")
        };
        let note = if self.message.is_empty() {
            String::new()
        } else {
            format!("  {}", self.message)
        };
        match self.status.as_str() {
            "new" => format!(" * [新引用]          {pair}{note}"),
            "deleted" => format!(" - [已删除]          {remote}{note}"),
            "up-to-date" => format!(" = [最新]            {pair}"),
            "forced" => format!(
                " + {}...{} {pair} (强制更新){note}",
                self.old.short(),
                self.new.short()
            ),
            "review" => format!(" ? [待评审]          {pair} ({})", self.message),
            "rejected" => format!(" ! [被拒绝]          {pair} ({})", self.message),
            _ => format!("   {}..{}  {pair}{note}", self.old.short(), self.new.short()),
        }
    }
}

fn short_name(name: &str) -> &str {
    name.strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/tags/"))
        .unwrap_or(name)
}

// ---------- 远端配置 ----------

pub fn remote_names(repo: &Repository) -> anyhow::Result<Vec<String>> {
    repo.config_subsections("remote")
}

/// 当前分支的上游远端，没有就用 origin（或唯一的远端）。
pub fn default_remote(repo: &Repository) -> String {
    if let Ok(Some(branch)) = repo.branch_name() {
        if let Ok(Some(remote)) = repo.config_get(&format!("branch.{branch}.remote")) {
            return remote;
        }
    }
    let names = remote_names(repo).unwrap_or_default();
    if names.iter().any(|name| name == "origin") || names.len() != 1 {
        "origin".into()
    } else {
        names[0].clone()
    }
}

fn valid_remote_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub fn add_remote(repo: &Repository, name: &str, url: &str) -> anyhow::Result<()> {
    if !valid_remote_name(name) {
        bail!("非法远端名: {name}");
    }
    if repo.config_get(&format!("remote.{name}.url"))?.is_some() {
        bail!("远端 {name} 已存在");
    }
    split_remote(url)?;
    repo.config_set(&format!("remote.{name}.url"), url)?;
    repo.config_set(
        &format!("remote.{name}.fetch"),
        &format!("+refs/heads/*:refs/remotes/{name}/*"),
    )?;
    Ok(())
}

pub fn remove_remote(repo: &Repository, name: &str) -> anyhow::Result<()> {
    if repo.config_get(&format!("remote.{name}.url"))?.is_none() {
        bail!("没有远端 {name}");
    }
    repo.config_remove_section(&format!("remote.{name}"))?;
    for (refname, _) in repo.list_refs(&format!("refs/remotes/{name}/"))? {
        repo.delete_ref(&refname)?;
    }
    for branch in repo.config_subsections("branch")? {
        if repo.config_get(&format!("branch.{branch}.remote"))?.as_deref() == Some(name) {
            repo.config_unset_section_key(&format!("branch.{branch}.remote"))?;
            repo.config_unset_section_key(&format!("branch.{branch}.merge"))?;
        }
    }
    Ok(())
}

pub fn rename_remote(repo: &Repository, old: &str, new: &str) -> anyhow::Result<()> {
    if !valid_remote_name(new) {
        bail!("非法远端名: {new}");
    }
    let url = repo
        .config_get(&format!("remote.{old}.url"))?
        .with_context(|| format!("没有远端 {old}"))?;
    if repo.config_get(&format!("remote.{new}.url"))?.is_some() {
        bail!("远端 {new} 已存在");
    }
    repo.config_remove_section(&format!("remote.{old}"))?;
    repo.config_set(&format!("remote.{new}.url"), &url)?;
    repo.config_set(
        &format!("remote.{new}.fetch"),
        &format!("+refs/heads/*:refs/remotes/{new}/*"),
    )?;
    let prefix = format!("refs/remotes/{old}/");
    for (refname, oid) in repo.list_refs(&prefix)? {
        let rest = &refname[prefix.len()..];
        repo.set_ref_logged(
            &format!("refs/remotes/{new}/{rest}"),
            oid,
            &format!("remote: renamed {refname}"),
        )?;
        repo.delete_ref(&refname)?;
    }
    for branch in repo.config_subsections("branch")? {
        if repo.config_get(&format!("branch.{branch}.remote"))?.as_deref() == Some(old) {
            repo.config_set(&format!("branch.{branch}.remote"), new)?;
        }
    }
    Ok(())
}

// ---------- fetch ----------

fn remote_refs(client: &Client) -> anyhow::Result<RefsResponse> {
    request_json(
        "GET",
        &format!("{}/api/git/{}/refs", client.origin, client.repo),
        Some(&client.token),
        None::<&()>,
    )
}

/// 把 `wants` 及其历史取到本地（不含本地已有的对象）。
fn download(repo: &Repository, client: &Client, wants: &[Oid]) -> anyhow::Result<()> {
    let wants: Vec<Oid> = wants
        .iter()
        .copied()
        .filter(|oid| !oid.is_zero() && !repo.has_object(*oid))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if wants.is_empty() {
        return Ok(());
    }
    let haves: Vec<Oid> = repo
        .list_refs("refs/")?
        .into_iter()
        .map(|(_, oid)| oid)
        .filter(|oid| repo.has_object(*oid))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let pack = request_bytes(
        &format!("{}/api/git/{}/fetch-pack", client.origin, client.repo),
        &client.token,
        &serde_json::json!({ "wants": wants, "haves": haves, "include_tags": true }),
    )?;
    repo.stage_pack(&pack).context("远端返回的 pack 无法解开")?;
    for want in &wants {
        if !repo.has_object(*want) {
            repo.discard_staged();
            bail!("远端 pack 缺少对象 {want}");
        }
    }
    repo.persist_staged()?;
    Ok(())
}

/// 从远端取回所有分支（写到 `refs/remotes/<远端>/`）和标签。
pub fn fetch_remote(repo: &Repository, name: &str, prune: bool) -> anyhow::Result<FetchReport> {
    let client = Client::from_remote(repo, name)?;
    fetch_with_client(repo, &client, name, prune)
}

pub fn fetch_with_client(
    repo: &Repository,
    client: &Client,
    name: &str,
    prune: bool,
) -> anyhow::Result<FetchReport> {
    let refs = remote_refs(client)?;
    let wanted: Vec<Oid> = refs
        .refs
        .iter()
        .chain(refs.tags.iter())
        .map(|item| item.oid)
        .collect();
    download(repo, client, &wanted)?;
    let mut updated = Vec::new();
    let prefix = format!("refs/remotes/{name}/");
    let mut seen = HashSet::new();
    for item in &refs.refs {
        let branch = item.name.trim_start_matches("refs/heads/");
        let local = format!("{prefix}{branch}");
        seen.insert(local.clone());
        let old = repo.resolve_ref(&local)?;
        if old == Some(item.oid) {
            continue;
        }
        let (line, message) = match old {
            None => (
                format!(" * [新分支]      {branch} -> {name}/{branch}"),
                "fetch: storing head".to_string(),
            ),
            Some(old) if repo.is_ancestor(old, item.oid).unwrap_or(false) => (
                format!("   {}..{}  {branch} -> {name}/{branch}", old.short(), item.oid.short()),
                "fetch: fast-forward".to_string(),
            ),
            Some(old) => (
                format!(
                    " + {}...{} {branch} -> {name}/{branch} (强制更新)",
                    old.short(),
                    item.oid.short()
                ),
                "fetch: forced-update".to_string(),
            ),
        };
        repo.set_ref_logged(&local, item.oid, &message)?;
        updated.push(line);
    }
    for item in &refs.tags {
        let tag = item.name.trim_start_matches("refs/tags/");
        let local = format!("refs/tags/{tag}");
        match repo.resolve_ref(&local)? {
            None => {
                repo.set_ref_logged(&local, item.oid, "fetch: storing tag")?;
                updated.push(format!(" * [新标签]      {tag} -> {tag}"));
            }
            Some(existing) if existing != item.oid => {
                updated.push(format!(" ! [已存在]      {tag} -> {tag} (本地标签不同，未覆盖)"));
            }
            Some(_) => {}
        }
    }
    let mut pruned = Vec::new();
    if prune {
        for (refname, _) in repo.list_refs(&prefix)? {
            if refname.ends_with("/HEAD") || seen.contains(&refname) {
                continue;
            }
            repo.delete_ref(&refname)?;
            pruned.push(refname.trim_start_matches("refs/remotes/").to_string());
        }
    }
    identity::save_manifest(repo.git_dir(), &refs.manifest)?;
    Ok(FetchReport {
        updated,
        pruned,
        manifest: refs.manifest,
    })
}

// ---------- pull ----------

/// 取回远端后把上游并入当前分支。`branch` 指定远端分支，否则用上游配置或同名分支。
pub fn pull_with(
    repo: &Repository,
    remote: Option<&str>,
    branch: Option<&str>,
    mode: PullMode,
) -> anyhow::Result<SyncReport> {
    let current = repo.branch_name()?;
    let remote = match remote {
        Some(remote) => remote.to_string(),
        None => default_remote(repo),
    };
    let merge_ref = match branch {
        Some(branch) => format!("refs/heads/{}", branch.trim_start_matches("refs/heads/")),
        None => {
            let current = current.clone().context("当前是分离 HEAD，请指定要拉取的分支")?;
            repo.config_get(&format!("branch.{current}.merge"))?
                .unwrap_or_else(|| format!("refs/heads/{current}"))
        }
    };
    let report = fetch_remote(repo, &remote, false)?;
    let manifest = report.manifest;
    let short = merge_ref.trim_start_matches("refs/heads/");
    let label = format!("{remote}/{short}");
    let Some(theirs) = repo.resolve_ref(&format!("refs/remotes/{remote}/{short}"))? else {
        return Ok(SyncReport {
            message: format!("远端没有分支 {short}"),
            manifest,
        });
    };
    let (name, email) = identity::load_identity();
    let who = identity::signature_now(&name, &email);
    if mode == PullMode::Rebase && repo.head_oid()?.is_some() {
        let message = match repo.rebase(theirs, None, None, false, None, &who)? {
            SeqOutcome::Done(message) => message,
            SeqOutcome::Stopped { message, conflicts } => {
                let mut text = message;
                for path in conflicts {
                    text.push_str(&format!("\n  冲突: {path}"));
                }
                text.push_str("\n解决后 ait add 再 ait rebase --continue，或 ait rebase --abort");
                bail!("{text}");
            }
        };
        return Ok(SyncReport { message, manifest });
    }
    let options = MergeOptions {
        ff_only: mode == PullMode::FastForwardOnly,
        ..MergeOptions::default()
    };
    let message = match repo.merge_with(theirs, &label, &who, &options)? {
        MergeOutcome::UpToDate => "已经是最新".into(),
        MergeOutcome::FastForward(oid) => format!("快进到 {}", oid.short()),
        MergeOutcome::Merged(oid) => format!("已合并 {label}，提交 {}", oid.short()),
        MergeOutcome::Conflicts(paths) => {
            bail!(
                "合并冲突，请修改这些文件后 ait add 再 ait commit:\n{}",
                paths
                    .iter()
                    .map(|path| format!("  {path}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    };
    Ok(SyncReport { message, manifest })
}

// ---------- push ----------

struct PushSpec {
    local: String,
    src: Option<Oid>,
    dst: String,
    force: bool,
}

fn parse_specs(repo: &Repository, options: &PushOptions, remote: &str) -> anyhow::Result<Vec<PushSpec>> {
    let mut specs = Vec::new();
    if options.refspecs.is_empty() {
        if options.tags {
            for (name, oid) in repo.list_refs("refs/tags/")? {
                specs.push(PushSpec {
                    local: name.clone(),
                    src: Some(oid),
                    dst: name,
                    force: options.force,
                });
            }
            return Ok(specs);
        }
        if options.delete {
            bail!("--delete 需要指定要删除的远端引用");
        }
        let branch = repo
            .branch_name()?
            .context("当前是分离 HEAD，请指定要推送的引用，例如 ait push origin HEAD:refs/heads/名字")?;
        let oid = repo.head_oid()?.context("当前分支还没有提交")?;
        let dst = match (
            repo.config_get(&format!("branch.{branch}.remote"))?,
            repo.config_get(&format!("branch.{branch}.merge"))?,
        ) {
            (Some(up_remote), Some(merge)) if up_remote == remote => merge,
            _ => format!("refs/heads/{branch}"),
        };
        specs.push(PushSpec {
            local: format!("refs/heads/{branch}"),
            src: Some(oid),
            dst,
            force: options.force,
        });
        return Ok(specs);
    }
    for raw in &options.refspecs {
        let (force, spec) = match raw.strip_prefix('+') {
            Some(rest) => (true, rest),
            None => (options.force, raw.as_str()),
        };
        if options.delete {
            specs.push(PushSpec {
                local: String::new(),
                src: None,
                dst: full_remote_ref(repo, spec, None),
                force,
            });
            continue;
        }
        let (src, dst) = match spec.split_once(':') {
            Some((src, dst)) => (src, Some(dst)),
            None => (spec, None),
        };
        if src.is_empty() {
            let dst = dst.filter(|d| !d.is_empty()).context("空的推送引用规格")?;
            specs.push(PushSpec {
                local: String::new(),
                src: None,
                dst: full_remote_ref(repo, dst, None),
                force,
            });
            continue;
        }
        let local = local_ref(repo, src)?;
        let oid = repo
            .rev_parse_object(src)
            .with_context(|| format!("本地找不到 {src}"))?;
        let dst = match dst {
            Some(dst) if !dst.is_empty() => full_remote_ref(repo, dst, local.as_deref()),
            _ => match &local {
                Some(local) => local.clone(),
                None => bail!("{src} 不是分支或标签，请写成 {src}:refs/heads/名字"),
            },
        };
        specs.push(PushSpec {
            local: local.unwrap_or_else(|| src.to_string()),
            src: Some(oid),
            dst,
            force,
        });
    }
    if options.tags {
        for (name, oid) in repo.list_refs("refs/tags/")? {
            if !specs.iter().any(|spec| spec.dst == name) {
                specs.push(PushSpec {
                    local: name.clone(),
                    src: Some(oid),
                    dst: name,
                    force: options.force,
                });
            }
        }
    }
    Ok(specs)
}

fn local_ref(repo: &Repository, name: &str) -> anyhow::Result<Option<String>> {
    if name == "HEAD" || name == "@" {
        return Ok(repo.branch_name()?.map(|branch| format!("refs/heads/{branch}")));
    }
    if name.starts_with("refs/") {
        return Ok(repo.resolve_ref(name).ok().flatten().map(|_| name.to_string()));
    }
    for candidate in [format!("refs/heads/{name}"), format!("refs/tags/{name}")] {
        if repo.resolve_ref(&candidate).ok().flatten().is_some() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn full_remote_ref(repo: &Repository, name: &str, local: Option<&str>) -> String {
    if name.starts_with("refs/") {
        return name.to_string();
    }
    if local.is_some_and(|local| local.starts_with("refs/tags/"))
        || (local.is_none() && repo.resolve_ref(&format!("refs/tags/{name}")).ok().flatten().is_some()
            && repo.resolve_ref(&format!("refs/heads/{name}")).ok().flatten().is_none())
    {
        format!("refs/tags/{name}")
    } else {
        format!("refs/heads/{name}")
    }
}

/// 按引用规格推送到远端，返回每条引用的结果；单条失败不影响其他引用。
pub fn push_refs(repo: &Repository, options: &PushOptions) -> anyhow::Result<Vec<PushResult>> {
    let remote = match &options.remote {
        Some(remote) => remote.clone(),
        None => default_remote(repo),
    };
    let client = Client::from_remote(repo, &remote)?;
    let specs = parse_specs(repo, options, &remote)?;
    if specs.is_empty() {
        bail!("没有要推送的引用");
    }
    let refs = remote_refs(&client)?;
    let mut remote_map: BTreeMap<String, Oid> = BTreeMap::new();
    for item in refs.refs.iter().chain(refs.tags.iter()) {
        remote_map.insert(item.name.clone(), item.oid);
    }
    let known: Vec<Oid> = remote_map
        .values()
        .copied()
        .filter(|oid| repo.has_object(*oid))
        .collect();
    let mut results = Vec::new();
    for spec in specs {
        let old = remote_map.get(&spec.dst).copied().unwrap_or(Oid::ZERO);
        let new = spec.src.unwrap_or(Oid::ZERO);
        let mut result = PushResult {
            local: spec.local.clone(),
            remote: spec.dst.clone(),
            old,
            new,
            forced: false,
            status: String::new(),
            message: String::new(),
            blocked: Vec::new(),
            manifest: None,
        };
        if new.is_zero() && old.is_zero() {
            result.status = "rejected".into();
            result.message = "远端没有这个引用".into();
            results.push(result);
            continue;
        }
        if new == old && !spec.force {
            result.status = "up-to-date".into();
            results.push(result);
            continue;
        }
        let fast_forward = old.is_zero()
            || new.is_zero()
            || (repo.has_object(old) && repo.is_ancestor(old, new).unwrap_or(false));
        if !fast_forward && !spec.force {
            result.status = "rejected".into();
            result.message = if spec.dst.starts_with("refs/tags/") {
                "标签已存在，移动需要 --force".into()
            } else {
                "不是快进，请先 ait pull 合并远端改动，或使用 --force".into()
            };
            results.push(result);
            continue;
        }
        let pack = if new.is_zero() {
            None
        } else {
            Some(STANDARD.encode(repo.pack_for(&[new], &known)?))
        };
        let response: anyhow::Result<PushResponse> = request_json(
            "POST",
            &format!("{}/api/git/{}/push", client.origin, client.repo),
            Some(&client.token),
            Some(&serde_json::json!({
                "reference": spec.dst,
                "old_oid": old,
                "new_oid": new,
                "force": spec.force,
                "objects": [],
                "pack_b64": pack,
            })),
        );
        match response {
            Ok(response) => {
                result.blocked = response.blocked.clone();
                result.message = response.message.clone();
                result.manifest = Some(response.manifest.clone());
                result.status = match response.status.as_str() {
                    "review" => "review".into(),
                    "deleted" => "deleted".into(),
                    "unchanged" => "up-to-date".into(),
                    _ if old.is_zero() => "new".into(),
                    _ if !fast_forward => "forced".into(),
                    _ => "updated".into(),
                };
                result.forced = !fast_forward;
                if let Some(branch) = spec.dst.strip_prefix("refs/heads/") {
                    let tracking = format!("refs/remotes/{remote}/{branch}");
                    match result.status.as_str() {
                        "deleted" => {
                            if repo.resolve_ref(&tracking)?.is_some() {
                                repo.delete_ref(&tracking)?;
                            }
                        }
                        "new" | "updated" | "forced" => {
                            repo.set_ref_logged(&tracking, new, "update by push")?;
                        }
                        _ => {}
                    }
                    if options.set_upstream && result.status != "deleted" {
                        if let Some(local) = spec.local.strip_prefix("refs/heads/") {
                            repo.config_set(&format!("branch.{local}.remote"), &remote)?;
                            repo.config_set(&format!("branch.{local}.merge"), &spec.dst)?;
                        }
                    }
                }
                identity::save_manifest(repo.git_dir(), &response.manifest)?;
            }
            Err(err) => {
                result.status = "rejected".into();
                result.message = err.to_string().replace('\n', " ");
            }
        }
        results.push(result);
    }
    Ok(results)
}

fn request_bytes(url: &str, token: &str, body: &serde_json::Value) -> anyhow::Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;
    let response = client
        .post(url)
        .bearer_auth(token)
        .json(body)
        .send()
        .with_context(|| format!("连接 {url} 失败"))?;
    let status = response.status();
    let bytes = response.bytes().context("读取响应失败")?.to_vec();
    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes).to_string();
        let message = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.get("error").and_then(|v| v.as_str()).map(str::to_string))
            .unwrap_or(text);
        bail!("{message}");
    }
    Ok(bytes)
}
