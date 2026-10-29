//! reflog 与版本表达式：`~n`、`^n`、`^{commit}`、`@{n}`、`@{-n}`、`@{u}`、`<版本>:<路径>`。

use std::fs;
use std::io::Write;

use anyhow::{bail, Context};

use super::{Head, Repository};
use crate::git::object::{Object, TagObject, MODE_TREE};
use crate::git::oid::Oid;

#[derive(Clone, Debug)]
pub struct ReflogEntry {
    pub old: Oid,
    pub new: Oid,
    pub who: String,
    pub time: i64,
    pub tz: String,
    pub message: String,
}

impl Repository {
    fn reflog_enabled(&self) -> bool {
        match self.config_get("core.logAllRefUpdates").ok().flatten() {
            Some(value) if value == "false" => false,
            Some(value) if value == "always" => true,
            _ => self.work_tree.is_some(),
        }
    }

    pub(super) fn append_reflog(
        &self,
        name: &str,
        old: Oid,
        new: Oid,
        message: &str,
    ) -> anyhow::Result<()> {
        let path = self.git_dir.join("logs").join(name);
        if !self.reflog_enabled() && !path.is_file() {
            return Ok(());
        }
        let (user, email) = crate::identity::load_identity();
        let when = crate::identity::signature_now(&user, &email);
        let message = message.replace('\n', " ");
        let line = format!(
            "{old} {new} {user} <{email}> {} {}\t{message}\n",
            when.time, when.tz
        );
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    /// 由新到旧排列的 reflog。
    pub fn reflog(&self, name: &str) -> anyhow::Result<Vec<ReflogEntry>> {
        let full = self.reflog_name(name)?;
        let path = self.git_dir.join("logs").join(&full);
        let text = fs::read_to_string(path).unwrap_or_default();
        let mut out = Vec::new();
        for line in text.lines() {
            if let Some(entry) = parse_reflog_line(line) {
                out.push(entry);
            }
        }
        out.reverse();
        Ok(out)
    }

    pub(super) fn write_reflog(&self, name: &str, newest_first: &[ReflogEntry]) -> anyhow::Result<()> {
        let path = self.git_dir.join("logs").join(name);
        let mut text = String::new();
        for entry in newest_first.iter().rev() {
            text.push_str(&format!(
                "{} {} {} {} {}\t{}\n",
                entry.old, entry.new, entry.who, entry.time, entry.tz, entry.message
            ));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, text)?;
        Ok(())
    }

    /// 把简写名换成 reflog 所在的完整引用名。
    pub fn reflog_name(&self, name: &str) -> anyhow::Result<String> {
        if name.is_empty() || name == "HEAD" || name == "@" {
            return Ok("HEAD".into());
        }
        if name.starts_with("refs/") {
            return Ok(name.to_string());
        }
        if name == "stash" {
            return Ok("refs/stash".into());
        }
        for prefix in ["refs/heads/", "refs/remotes/", "refs/tags/"] {
            let full = format!("{prefix}{name}");
            if self.resolve_ref(&full).ok().flatten().is_some()
                || self.git_dir.join("logs").join(&full).is_file()
            {
                return Ok(full);
            }
        }
        bail!("找不到引用 {name}")
    }

    /// 解析版本表达式并剥到提交。
    pub fn rev_parse(&self, spec: &str) -> anyhow::Result<Oid> {
        let oid = self.rev_parse_object(spec)?;
        self.peel_to_commit(oid)
            .with_context(|| format!("{spec} 不是提交"))
    }

    /// 解析版本表达式，返回它直接指向的对象（附注标签返回标签对象本身，`rev:path` 返回文件或目录）。
    pub fn rev_parse_object(&self, spec: &str) -> anyhow::Result<Oid> {
        let spec = spec.trim();
        if spec.is_empty() {
            bail!("缺少版本参数");
        }
        if let Some(rest) = spec.strip_prefix(':') {
            if rest.starts_with('/') {
                return self.search_message(&rest[1..], None);
            }
            let (stage, path) = match rest.split_once(':') {
                Some((n, path)) if n.len() == 1 && n.chars().all(|c| c.is_ascii_digit()) => {
                    (n.parse::<u16>().unwrap_or(0), path)
                }
                _ => (0, rest),
            };
            let index = self.read_index()?;
            return index
                .entries
                .iter()
                .find(|entry| entry.path == path && entry.stage == stage)
                .map(|entry| entry.oid)
                .with_context(|| format!("暂存区里没有 {path}"));
        }
        if let Some((rev, path)) = split_rev_path(spec) {
            let commit = self.rev_parse(rev)?;
            let tree = self.read_commit(commit)?.tree;
            return self.tree_entry(tree, path);
        }
        let cut = suffix_start(spec);
        let (base, mut rest) = spec.split_at(cut);
        let mut oid = self.resolve_base(base)?;
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix('~') {
                let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                let n: usize = if digits.is_empty() { 1 } else { digits.parse()? };
                rest = &after[digits.len()..];
                for _ in 0..n {
                    let commit = self.read_commit(self.peel_to_commit(oid)?)?;
                    oid = *commit
                        .parents
                        .first()
                        .with_context(|| format!("{spec} 超出了历史起点"))?;
                }
            } else if let Some(after) = rest.strip_prefix("^{") {
                let end = after.find('}').context("缺少 }")?;
                let want = &after[..end];
                rest = &after[end + 1..];
                oid = match want {
                    "" => self.peel_tags(oid)?,
                    "commit" => self.peel_to_commit(oid)?,
                    "tree" => self.read_commit(self.peel_to_commit(oid)?)?.tree,
                    "object" => oid,
                    "tag" => match self.read_object(oid)? {
                        Object::Tag(_) => oid,
                        _ => bail!("{spec} 不是标签"),
                    },
                    "blob" => match self.read_object(self.peel_tags(oid)?)? {
                        Object::Blob(_) => self.peel_tags(oid)?,
                        _ => bail!("{spec} 不是文件"),
                    },
                    text if text.starts_with('/') => {
                        self.search_message(&text[1..], Some(self.peel_to_commit(oid)?))?
                    }
                    other => bail!("不支持的 ^{{{other}}}"),
                };
            } else if let Some(after) = rest.strip_prefix('^') {
                let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                let n: usize = if digits.is_empty() { 1 } else { digits.parse()? };
                rest = &after[digits.len()..];
                let commit_oid = self.peel_to_commit(oid)?;
                if n == 0 {
                    oid = commit_oid;
                    continue;
                }
                let commit = self.read_commit(commit_oid)?;
                oid = *commit
                    .parents
                    .get(n - 1)
                    .with_context(|| format!("{spec} 没有第 {n} 个父提交"))?;
            } else {
                bail!("无法解析 {spec}");
            }
        }
        Ok(oid)
    }

    fn resolve_base(&self, base: &str) -> anyhow::Result<Oid> {
        if base.is_empty() {
            bail!("缺少版本参数");
        }
        if let Some(at) = base.rfind("@{") {
            if !base.ends_with('}') {
                bail!("无法解析 {base}");
            }
            let name = &base[..at];
            let selector = &base[at + 2..base.len() - 1];
            if let Some(n) = selector.strip_prefix('-') {
                let n: usize = n.parse().context("@{-n} 需要数字")?;
                let branch = self.previous_branch(n)?;
                return self.resolve_name(&branch);
            }
            if matches!(selector, "u" | "upstream" | "push") {
                let branch = if name.is_empty() {
                    self.branch_name()?.context("分离 HEAD 没有上游")?
                } else {
                    name.to_string()
                };
                let upstream = self
                    .upstream_of(&branch)?
                    .with_context(|| format!("分支 {branch} 没有设置上游"))?;
                return self
                    .resolve_ref(&upstream)?
                    .with_context(|| format!("上游 {upstream} 还不存在"));
            }
            if let Ok(n) = selector.parse::<usize>() {
                let refname = if name.is_empty() {
                    match self.head()? {
                        Head::Symbolic(target) => target,
                        Head::Detached(_) => "HEAD".into(),
                    }
                } else {
                    self.reflog_name(name)?
                };
                let log = self.reflog(&refname)?;
                if n == 0 {
                    if let Some(first) = log.first() {
                        return Ok(first.new);
                    }
                    return self.resolve_name(&refname);
                }
                if let Some(entry) = log.get(n) {
                    return Ok(entry.new);
                }
                if n == log.len() {
                    if let Some(last) = log.last() {
                        if !last.old.is_zero() {
                            return Ok(last.old);
                        }
                    }
                }
                bail!("{refname} 的 reflog 只有 {} 条", log.len());
            }
            bail!("不支持的 @{{{selector}}}（暂不支持按日期）");
        }
        self.resolve_name(base)
    }

    /// `@{-n}`：从 HEAD 的 reflog 里找倒数第 n 次切换前所在的分支。
    pub fn previous_branch(&self, n: usize) -> anyhow::Result<String> {
        let mut count = 0;
        for entry in self.reflog("HEAD")? {
            if let Some(rest) = entry.message.strip_prefix("checkout: moving from ") {
                if let Some((from, _)) = rest.split_once(" to ") {
                    count += 1;
                    if count == n {
                        return Ok(from.to_string());
                    }
                }
            }
        }
        bail!("没有第 {n} 个之前的分支")
    }

    /// 分支的上游，形如 `refs/remotes/origin/main`。
    pub fn upstream_of(&self, branch: &str) -> anyhow::Result<Option<String>> {
        let Some(remote) = self.config_get(&format!("branch.{branch}.remote"))? else {
            return Ok(None);
        };
        let Some(merge) = self.config_get(&format!("branch.{branch}.merge"))? else {
            return Ok(None);
        };
        let short = merge.trim_start_matches("refs/heads/");
        if remote == "." {
            return Ok(Some(format!("refs/heads/{short}")));
        }
        Ok(Some(format!("refs/remotes/{remote}/{short}")))
    }

    pub fn peel_tags(&self, mut oid: Oid) -> anyhow::Result<Oid> {
        for _ in 0..20 {
            match self.read_object(oid)? {
                Object::Tag(payload) => oid = TagObject::parse(&payload)?.object,
                _ => return Ok(oid),
            }
        }
        bail!("标签嵌套过深")
    }

    pub fn peel_to_commit(&self, oid: Oid) -> anyhow::Result<Oid> {
        let oid = self.peel_tags(oid)?;
        match self.read_object(oid)? {
            Object::Commit(_) => Ok(oid),
            other => bail!("{oid} 是 {}，不是提交", other.kind()),
        }
    }

    fn tree_entry(&self, tree: Oid, path: &str) -> anyhow::Result<Oid> {
        let path = path.trim_matches('/');
        if path.is_empty() {
            return Ok(tree);
        }
        let mut current = tree;
        let parts: Vec<&str> = path.split('/').collect();
        for (index, part) in parts.iter().enumerate() {
            let Object::Tree(entries) = self.read_object(current)? else {
                bail!("{path} 不存在");
            };
            let entry = entries
                .into_iter()
                .find(|entry| entry.name == *part)
                .with_context(|| format!("路径不存在: {path}"))?;
            if index + 1 < parts.len() && entry.mode != MODE_TREE {
                bail!("路径不存在: {path}");
            }
            current = entry.oid;
        }
        Ok(current)
    }

    /// `:/文本`：从所有分支（或给定提交）往回找说明里含这段文本的最新提交。
    fn search_message(&self, text: &str, from: Option<Oid>) -> anyhow::Result<Oid> {
        let starts: Vec<Oid> = match from {
            Some(oid) => vec![oid],
            None => {
                let mut tips: Vec<Oid> = self.list_refs("refs/")?.into_iter().map(|(_, o)| o).collect();
                if let Some(head) = self.head_oid()? {
                    tips.push(head);
                }
                tips
            }
        };
        for start in starts {
            if let Ok(found) = self.log(start, usize::MAX) {
                if let Some(hit) = found.iter().find(|commit| commit.message.contains(text)) {
                    return Ok(hit.oid);
                }
            }
        }
        bail!("没有说明包含 {text} 的提交")
    }
}

fn parse_reflog_line(line: &str) -> Option<ReflogEntry> {
    let (head, message) = line.split_once('\t').unwrap_or((line, ""));
    let mut parts = head.splitn(3, ' ');
    let old = Oid::parse(parts.next()?).ok()?;
    let new = Oid::parse(parts.next()?).ok()?;
    let rest = parts.next()?;
    let mut tail = rest.rsplitn(3, ' ');
    let tz = tail.next()?.to_string();
    let time = tail.next()?.parse().ok()?;
    let who = tail.next()?.to_string();
    Some(ReflogEntry {
        old,
        new,
        who,
        time,
        tz,
        message: message.to_string(),
    })
}

/// 找到名字之后第一个 `~` 或 `^`（`@{...}` 里面的不算）。
fn suffix_start(spec: &str) -> usize {
    let bytes = spec.as_bytes();
    let mut depth = 0;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'~' | b'^' if depth == 0 && index > 0 => return index,
            _ => {}
        }
    }
    spec.len()
}

fn split_rev_path(spec: &str) -> Option<(&str, &str)> {
    let mut depth = 0;
    for (index, ch) in spec.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            ':' if depth == 0 => return Some((&spec[..index], &spec[index + 1..])),
            _ => {}
        }
    }
    None
}
