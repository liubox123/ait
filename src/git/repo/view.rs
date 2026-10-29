//! 查看历史：带过滤的提交遍历、分支图、blame、grep、标签、describe，以及任意两端之间的差异。

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;

use anyhow::{bail, Context};

use super::{index_map, Repository};
use crate::git::diff::{diff_bytes, FileDiff};
use crate::git::lines::{match_lines, split_lines};
use crate::git::merge::{content_similarity, FileMap};
use crate::git::object::{Commit, Object, Signature, TagObject, MODE_TREE};
use crate::git::oid::Oid;

#[derive(Clone, Debug, Default)]
pub struct LogFilter {
    pub include: Vec<Oid>,
    pub exclude: Vec<Oid>,
    pub paths: Vec<String>,
    pub author: Option<String>,
    pub grep: Option<String>,
    pub ignore_case: bool,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub max_count: Option<usize>,
    pub skip: usize,
    pub first_parent: bool,
    pub no_merges: bool,
    pub merges: bool,
    pub topo: bool,
    pub reverse: bool,
}

#[derive(Clone, Debug)]
pub struct BlameLine {
    pub oid: Oid,
    pub path: String,
    pub orig_line: usize,
    pub final_line: usize,
    pub author: String,
    pub time: i64,
    pub tz: String,
    pub summary: String,
    pub content: String,
}

#[derive(Clone, Debug, Default)]
pub struct GrepOptions {
    pub pattern: String,
    pub ignore_case: bool,
    pub word: bool,
    pub fixed: bool,
    pub invert: bool,
    pub source: Option<Oid>,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct GrepMatch {
    pub path: String,
    pub line_no: usize,
    pub line: String,
}

#[derive(Clone, Debug)]
pub struct TagInfo {
    pub name: String,
    pub target: Oid,
    pub commit: Option<Oid>,
    pub annotated: bool,
    pub message: String,
    pub tagger: Option<Signature>,
}

/// 差异的一端。
#[derive(Clone, Copy, Debug)]
pub enum DiffSide {
    Commit(Oid),
    Index,
    Worktree,
}

impl Repository {
    /// 按过滤条件遍历提交。默认按提交时间从新到旧；`topo` 时保证子提交都在父提交之前。
    pub fn walk(&self, filter: &LogFilter) -> anyhow::Result<Vec<(Oid, Commit)>> {
        let mut excluded = HashSet::new();
        for oid in &filter.exclude {
            self.mark_reachable_commits(*oid, &mut excluded)?;
        }
        let paths: Vec<String> = filter
            .paths
            .iter()
            .map(|p| p.trim_matches('/').to_string())
            .filter(|p| !p.is_empty())
            .collect();
        let author = compile(filter.author.as_deref(), filter.ignore_case)?;
        let grep = compile(filter.grep.as_deref(), filter.ignore_case)?;
        let mut queue: BinaryHeap<(i64, Reverse<u64>, Oid)> = BinaryHeap::new();
        let mut seen = HashSet::new();
        let mut seq = 0u64;
        let mut out = Vec::new();
        let mut skipped = 0;
        let unlimited = filter.topo || filter.reverse;
        for oid in &filter.include {
            if seen.insert(*oid) && !excluded.contains(oid) {
                let commit = self.read_commit(*oid)?;
                queue.push((commit.committer.time, Reverse(seq), *oid));
                seq += 1;
            }
        }
        while let Some((_, _, oid)) = queue.pop() {
            let commit = self.read_commit(oid)?;
            let mut follow: Vec<Oid> = if filter.first_parent {
                commit.parents.iter().take(1).copied().collect()
            } else {
                commit.parents.clone()
            };
            let mut show = true;
            if !paths.is_empty() {
                let own: Vec<Option<Oid>> = paths
                    .iter()
                    .map(|p| self.path_oid(commit.tree, p))
                    .collect();
                if follow.is_empty() {
                    show = own.iter().any(|o| o.is_some());
                } else {
                    let mut same_parent = None;
                    for parent in &follow {
                        let tree = self.read_commit(*parent)?.tree;
                        let theirs: Vec<Option<Oid>> =
                            paths.iter().map(|p| self.path_oid(tree, p)).collect();
                        if theirs == own {
                            same_parent = Some(*parent);
                            break;
                        }
                    }
                    if let Some(parent) = same_parent {
                        show = false;
                        follow = vec![parent];
                    }
                }
            }
            if filter.no_merges && commit.parents.len() > 1 {
                show = false;
            }
            if filter.merges && commit.parents.len() < 2 {
                show = false;
            }
            if let Some(until) = filter.until {
                if commit.committer.time > until {
                    show = false;
                }
            }
            if let Some(re) = &author {
                let who = format!("{} <{}>", commit.author.name, commit.author.email);
                if !re.is_match(&who) {
                    show = false;
                }
            }
            if let Some(re) = &grep {
                if !re.is_match(&commit.message) {
                    show = false;
                }
            }
            let too_old = filter.since.is_some_and(|since| commit.committer.time < since);
            if too_old {
                show = false;
            }
            for parent in follow {
                if seen.insert(parent) && !excluded.contains(&parent) {
                    if let Ok(next) = self.read_commit(parent) {
                        if filter.since.is_some_and(|since| next.committer.time < since) && too_old {
                            continue;
                        }
                        queue.push((next.committer.time, Reverse(seq), parent));
                        seq += 1;
                    }
                }
            }
            if show {
                if skipped < filter.skip {
                    skipped += 1;
                    continue;
                }
                out.push((oid, commit));
                if !unlimited && filter.max_count.is_some_and(|max| out.len() >= max) {
                    break;
                }
            }
        }
        if filter.topo {
            out = topo_sort(out);
        }
        if let Some(max) = filter.max_count {
            out.truncate(max);
        }
        if filter.reverse {
            out.reverse();
        }
        Ok(out)
    }

    /// 树里某个路径（文件或目录）的对象名，不存在时为 None。
    pub fn path_oid(&self, tree: Oid, path: &str) -> Option<Oid> {
        let mut current = tree;
        for part in path.split('/') {
            let Ok(Object::Tree(entries)) = self.read_object(current) else {
                return None;
            };
            current = entries.into_iter().find(|e| e.name == part)?.oid;
        }
        Some(current)
    }

    pub fn blame(&self, start: Oid, path: &str) -> anyhow::Result<Vec<BlameLine>> {
        let path = path.trim_matches('/').to_string();
        let (_, _, bytes) = self.file_at(start, &path)?;
        let final_lines: Vec<String> = split_lines(&bytes)
            .into_iter()
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect();
        let mut result: Vec<Option<BlameLine>> = vec![None; final_lines.len()];
        // 每个待处理提交：该提交里的路径，以及 (最终行号, 该版本里的行号)。
        let mut pending: HashMap<Oid, (String, Vec<(usize, usize)>)> = HashMap::new();
        let mut queue: BinaryHeap<(i64, Oid)> = BinaryHeap::new();
        let start_commit = self.read_commit(start)?;
        pending.insert(
            start,
            (path.clone(), (0..final_lines.len()).map(|i| (i, i)).collect()),
        );
        queue.push((start_commit.committer.time, start));
        let mut cache: HashMap<Oid, Vec<u8>> = HashMap::new();
        while let Some((_, oid)) = queue.pop() {
            let Some((own_path, mut lines)) = pending.remove(&oid) else {
                continue;
            };
            let commit = self.read_commit(oid)?;
            let own_blob = self
                .path_oid(commit.tree, &own_path)
                .context("blame 路径丢失")?;
            let own_bytes = self.blob_cached(own_blob, &mut cache)?;
            let own_split: Vec<Vec<u8>> = split_lines(&own_bytes).into_iter().map(|l| l.to_vec()).collect();
            for parent in &commit.parents {
                if lines.is_empty() {
                    break;
                }
                let parent_commit = self.read_commit(*parent)?;
                let Some((parent_path, parent_blob)) =
                    self.blame_source(&commit, &parent_commit, &own_path, own_blob, &mut cache)?
                else {
                    continue;
                };
                let moved: Vec<(usize, usize)>;
                if parent_blob == own_blob {
                    moved = std::mem::take(&mut lines);
                } else {
                    let parent_bytes = self.blob_cached(parent_blob, &mut cache)?;
                    let old = split_lines(&parent_bytes);
                    let new: Vec<&[u8]> = own_split.iter().map(|l| l.as_slice()).collect();
                    let map = match_lines(&old, &new);
                    let mut keep = Vec::new();
                    let mut pass = Vec::new();
                    for (final_no, here) in lines {
                        match map.get(here).copied().flatten() {
                            Some(there) => pass.push((final_no, there)),
                            None => keep.push((final_no, here)),
                        }
                    }
                    lines = keep;
                    moved = pass;
                }
                if moved.is_empty() {
                    continue;
                }
                let entry = pending
                    .entry(*parent)
                    .or_insert_with(|| (parent_path.clone(), Vec::new()));
                if entry.1.is_empty() {
                    queue.push((parent_commit.committer.time, *parent));
                }
                entry.1.extend(moved);
            }
            for (final_no, here) in lines {
                result[final_no] = Some(BlameLine {
                    oid,
                    path: own_path.clone(),
                    orig_line: here + 1,
                    final_line: final_no + 1,
                    author: commit.author.name.clone(),
                    time: commit.author.time,
                    tz: commit.author.tz.clone(),
                    summary: commit.summary().to_string(),
                    content: final_lines[final_no].clone(),
                });
            }
        }
        Ok(result.into_iter().flatten().collect())
    }

    fn blob_cached(&self, oid: Oid, cache: &mut HashMap<Oid, Vec<u8>>) -> anyhow::Result<Vec<u8>> {
        if let Some(bytes) = cache.get(&oid) {
            return Ok(bytes.clone());
        }
        let bytes = self.read_blob(oid)?;
        if cache.len() > 256 {
            cache.clear();
        }
        cache.insert(oid, bytes.clone());
        Ok(bytes)
    }

    /// 父提交里与这个文件对应的路径和内容；文件在父提交里不存在时尝试找改名来源。
    fn blame_source(
        &self,
        commit: &Commit,
        parent: &Commit,
        path: &str,
        blob: Oid,
        cache: &mut HashMap<Oid, Vec<u8>>,
    ) -> anyhow::Result<Option<(String, Oid)>> {
        if let Some(oid) = self.path_oid(parent.tree, path) {
            if matches!(self.read_object(oid)?, Object::Blob(_)) {
                return Ok(Some((path.to_string(), oid)));
            }
        }
        let ours = self.tree_files(commit.tree)?;
        let theirs = self.tree_files(parent.tree)?;
        let removed: Vec<(&String, &(u32, Oid))> = theirs
            .iter()
            .filter(|(p, _)| !ours.contains_key(*p))
            .collect();
        if let Some((p, (_, oid))) = removed.iter().find(|(_, (_, oid))| *oid == blob) {
            return Ok(Some(((*p).clone(), *oid)));
        }
        let own = self.blob_cached(blob, cache)?;
        let mut best: Option<(u32, String, Oid)> = None;
        for (p, (_, oid)) in removed {
            let bytes = self.blob_cached(*oid, cache)?;
            let score = content_similarity(&bytes, &own);
            if score >= 50 && best.as_ref().map_or(true, |b| score > b.0) {
                best = Some((score, p.clone(), *oid));
            }
        }
        Ok(best.map(|(_, p, oid)| (p, oid)))
    }

    pub fn grep(&self, options: &GrepOptions) -> anyhow::Result<Vec<GrepMatch>> {
        let mut pattern = if options.fixed {
            regex::escape(&options.pattern)
        } else {
            options.pattern.clone()
        };
        if options.word {
            pattern = format!(r"\b(?:{pattern})\b");
        }
        let re = regex::RegexBuilder::new(&pattern)
            .case_insensitive(options.ignore_case)
            .build()
            .with_context(|| format!("无效的正则 {}", options.pattern))?;
        let specs: Vec<String> = options
            .paths
            .iter()
            .map(|p| self.resolve_spec(p))
            .collect::<anyhow::Result<_>>()?;
        let covers = |path: &str| {
            specs.is_empty()
                || specs
                    .iter()
                    .any(|s| s.is_empty() || path == s || path.starts_with(&format!("{s}/")))
        };
        let files: Vec<(String, Option<Oid>)> = match options.source {
            Some(oid) => self
                .files_of_commit(Some(oid))?
                .into_iter()
                .filter(|(p, _)| covers(p))
                .map(|(p, (_, o))| (p, Some(o)))
                .collect(),
            None => self
                .read_index()?
                .entries
                .iter()
                .filter(|e| e.stage == 0 && covers(&e.path))
                .map(|e| (e.path.clone(), None))
                .collect(),
        };
        let mut out = Vec::new();
        for (path, oid) in files {
            let bytes = match oid {
                Some(oid) => self.read_blob(oid)?,
                None => match self.read_work_bytes(&path) {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                },
            };
            if bytes.iter().take(8000).any(|b| *b == 0) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (index, line) in text.lines().enumerate() {
                if re.is_match(line) != options.invert {
                    out.push(GrepMatch {
                        path: path.clone(),
                        line_no: index + 1,
                        line: line.to_string(),
                    });
                }
            }
        }
        Ok(out)
    }

    pub fn tags(&self) -> anyhow::Result<Vec<TagInfo>> {
        let mut out = Vec::new();
        for (name, oid) in self.list_refs("refs/tags/")? {
            let short = name.trim_start_matches("refs/tags/").to_string();
            let mut info = TagInfo {
                name: short,
                target: oid,
                commit: self.peel_to_commit(oid).ok(),
                annotated: false,
                message: String::new(),
                tagger: None,
            };
            if let Ok(Object::Tag(payload)) = self.read_object(oid) {
                if let Ok(tag) = TagObject::parse(&payload) {
                    info.annotated = true;
                    info.message = tag.message;
                    info.tagger = tag.tagger;
                }
            }
            out.push(info);
        }
        Ok(out)
    }

    /// 建标签。给出 `message` 时写附注标签对象，否则是轻量标签。
    pub fn create_tag(
        &self,
        name: &str,
        target: Oid,
        message: Option<&str>,
        tagger: &Signature,
        force: bool,
    ) -> anyhow::Result<Oid> {
        if !super::valid_branch(name) {
            bail!("非法标签名: {name}");
        }
        let refname = format!("refs/tags/{name}");
        if !force && self.resolve_ref(&refname)?.is_some() {
            bail!("标签已存在: {name}");
        }
        let oid = match message {
            Some(text) => {
                let kind = self.read_object(target)?.kind().to_string();
                let tag = TagObject {
                    object: target,
                    kind,
                    name: name.to_string(),
                    tagger: Some(tagger.clone()),
                    message: format!("{}\n", text.trim_end()),
                };
                self.write_object("tag", &tag.encode())?
            }
            None => target,
        };
        self.set_ref(&refname, oid)?;
        Ok(oid)
    }

    pub fn delete_tag(&self, name: &str) -> anyhow::Result<Oid> {
        let refname = format!("refs/tags/{name}");
        let oid = self
            .resolve_ref(&refname)?
            .with_context(|| format!("标签不存在: {name}"))?;
        self.delete_ref(&refname)?;
        Ok(oid)
    }

    /// 用最近的标签描述提交：`标签` 或 `标签-距离-g缩写`。
    pub fn describe(&self, oid: Oid, all_tags: bool, always: bool) -> anyhow::Result<String> {
        let mut tagged: HashMap<Oid, Vec<(String, bool)>> = HashMap::new();
        for tag in self.tags()? {
            if !all_tags && !tag.annotated {
                continue;
            }
            if let Some(commit) = tag.commit {
                tagged.entry(commit).or_default().push((tag.name, tag.annotated));
            }
        }
        if let Some(names) = tagged.get(&oid) {
            let mut names = names.clone();
            names.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            return Ok(names[0].0.clone());
        }
        let mut candidates = Vec::new();
        if !tagged.is_empty() {
            for view in self.log(oid, usize::MAX)? {
                if let Some(names) = tagged.get(&view.oid) {
                    let mut names = names.clone();
                    names.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                    candidates.push((view.oid, names[0].0.clone()));
                    if candidates.len() >= 10 {
                        break;
                    }
                }
            }
        }
        if candidates.is_empty() {
            if always {
                return Ok(oid.short());
            }
            bail!("没有可用来描述的标签{}", if all_tags { "" } else { "（可加 --tags 使用轻量标签）" });
        }
        let mut mine = HashSet::new();
        self.mark_reachable_commits(oid, &mut mine)?;
        let mut best: Option<(usize, String)> = None;
        for (commit, name) in candidates {
            let mut theirs = HashSet::new();
            self.mark_reachable_commits(commit, &mut theirs)?;
            let depth = mine.iter().filter(|o| !theirs.contains(o)).count();
            if best.as_ref().map_or(true, |b| depth < b.0) {
                best = Some((depth, name));
            }
        }
        let (depth, name) = best.expect("至少有一个候选");
        Ok(format!("{name}-{depth}-g{}", oid.short()))
    }

    /// 两端之间的差异。`paths` 为空时比较全部文件；工作区一端只包含已跟踪的文件。
    pub fn diff_sides(
        &self,
        old: DiffSide,
        new: DiffSide,
        paths: &[String],
    ) -> anyhow::Result<Vec<FileDiff>> {
        let specs: Vec<String> = paths
            .iter()
            .map(|p| self.resolve_spec(p))
            .collect::<anyhow::Result<_>>()?;
        let covers = |path: &str| {
            specs.is_empty()
                || specs
                    .iter()
                    .any(|s| s.is_empty() || path == s || path.starts_with(&format!("{s}/")))
        };
        let old_map = self.side_map(old)?;
        let new_map = self.side_map(new)?;
        let mut diffs = Vec::new();
        for change in super::path_changes(&old_map, &new_map) {
            if !covers(&change.path) {
                continue;
            }
            let old_bytes = self.side_bytes(old, &old_map, &change.path)?;
            let new_bytes = self.side_bytes(new, &new_map, &change.path)?;
            diffs.push(diff_bytes(
                &change.path,
                old_bytes.as_deref(),
                new_bytes.as_deref(),
            ));
        }
        Ok(diffs)
    }

    fn side_map(&self, side: DiffSide) -> anyhow::Result<FileMap> {
        Ok(match side {
            DiffSide::Commit(oid) => self.files_of_commit(Some(oid))?,
            DiffSide::Index => index_map(&self.read_index()?),
            DiffSide::Worktree => {
                let mut map = FileMap::new();
                for entry in self.read_index()?.entries {
                    let disk = self.work_path(&entry.path)?;
                    if fs::symlink_metadata(&disk).is_err() {
                        continue;
                    }
                    if let Ok(found) = self.hash_work(&entry.path) {
                        map.insert(entry.path.clone(), found);
                    }
                }
                map
            }
        })
    }

    fn side_bytes(&self, side: DiffSide, map: &FileMap, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let Some((_, oid)) = map.get(path) else {
            return Ok(None);
        };
        Ok(Some(match side {
            DiffSide::Worktree => self.read_work_bytes(path)?,
            _ => self.read_blob(*oid)?,
        }))
    }

    /// 暂存区条目：路径、模式、对象名、阶段。
    pub fn index_entries(&self) -> anyhow::Result<Vec<(String, u32, Oid, u16)>> {
        Ok(self
            .read_index()?
            .entries
            .into_iter()
            .map(|e| (e.path, e.mode, e.oid, e.stage))
            .collect())
    }

    /// 树的直接子项（`recursive` 时展开子目录），返回 (模式, 类型, 对象名, 路径)。
    pub fn ls_tree(&self, tree: Oid, recursive: bool) -> anyhow::Result<Vec<(u32, String, Oid, String)>> {
        let mut out = Vec::new();
        self.ls_tree_into(tree, "", recursive, &mut out)?;
        Ok(out)
    }

    fn ls_tree_into(
        &self,
        tree: Oid,
        prefix: &str,
        recursive: bool,
        out: &mut Vec<(u32, String, Oid, String)>,
    ) -> anyhow::Result<()> {
        let Object::Tree(entries) = self.read_object(tree)? else {
            bail!("{tree} 不是目录");
        };
        for entry in entries {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            if entry.mode == MODE_TREE && recursive {
                self.ls_tree_into(entry.oid, &path, true, out)?;
            } else {
                let kind = if entry.mode == MODE_TREE {
                    "tree"
                } else if entry.mode == 0o160000 {
                    "commit"
                } else {
                    "blob"
                };
                out.push((entry.mode, kind.to_string(), entry.oid, path));
            }
        }
        Ok(())
    }
}

fn compile(pattern: Option<&str>, ignore_case: bool) -> anyhow::Result<Option<regex::Regex>> {
    pattern
        .map(|p| {
            regex::RegexBuilder::new(p)
                .case_insensitive(ignore_case)
                .build()
                .with_context(|| format!("无效的正则 {p}"))
        })
        .transpose()
}

/// 子提交一定排在父提交之前；可选的提交里按时间从新到旧。
fn topo_sort(commits: Vec<(Oid, Commit)>) -> Vec<(Oid, Commit)> {
    let present: HashSet<Oid> = commits.iter().map(|(o, _)| *o).collect();
    let mut children: HashMap<Oid, usize> = HashMap::new();
    for (_, commit) in &commits {
        for parent in &commit.parents {
            if present.contains(parent) {
                *children.entry(*parent).or_default() += 1;
            }
        }
    }
    let mut by_oid: HashMap<Oid, Commit> = HashMap::new();
    let mut order: HashMap<Oid, usize> = HashMap::new();
    for (index, (oid, commit)) in commits.into_iter().enumerate() {
        order.insert(oid, index);
        by_oid.insert(oid, commit);
    }
    let mut ready: Vec<Oid> = by_oid
        .keys()
        .filter(|o| !children.contains_key(o))
        .copied()
        .collect();
    let mut out = Vec::new();
    while !ready.is_empty() {
        // 优先沿着刚输出提交的第一父提交继续，避免分支线交错。
        ready.sort_by_key(|o| Reverse(order[o]));
        let next = ready.pop().unwrap();
        let commit = by_oid.remove(&next).unwrap();
        for parent in &commit.parents {
            if let Some(count) = children.get_mut(parent) {
                *count -= 1;
                if *count == 0 {
                    children.remove(parent);
                    ready.push(*parent);
                }
            }
        }
        out.push((next, commit));
    }
    out
}

/// 分支图的一行：提交行前缀、正文行前缀，以及提交后需要输出的连接线。
#[derive(Clone, Debug)]
pub struct GraphRow {
    pub commit: String,
    pub body: String,
    pub after: Vec<String>,
}

pub fn render_graph(commits: &[(Oid, Vec<Oid>)]) -> Vec<GraphRow> {
    let mut lanes: Vec<Option<Oid>> = Vec::new();
    let mut rows = Vec::new();
    for (oid, parents) in commits {
        let col = match lanes.iter().position(|l| *l == Some(*oid)) {
            Some(col) => col,
            None => match lanes.iter().position(|l| l.is_none()) {
                Some(free) => {
                    lanes[free] = Some(*oid);
                    free
                }
                None => {
                    lanes.push(Some(*oid));
                    lanes.len() - 1
                }
            },
        };
        let commit_line = draw(&lanes, |i, lane| {
            if i == col {
                '*'
            } else if lane.is_some() {
                '|'
            } else {
                ' '
            }
        });
        for (i, lane) in lanes.iter_mut().enumerate() {
            if i != col && *lane == Some(*oid) {
                *lane = None;
            }
        }
        lanes[col] = parents.first().copied();
        let mut after = Vec::new();
        let mut inserted = Vec::new();
        for parent in parents.iter().skip(1) {
            if lanes.contains(&Some(*parent)) {
                continue;
            }
            let at = col + 1 + inserted.len();
            lanes.insert(at.min(lanes.len()), Some(*parent));
            inserted.push(at.min(lanes.len() - 1));
        }
        let body = draw(&lanes, |_, lane| if lane.is_some() { '|' } else { ' ' });
        if !inserted.is_empty() {
            let mut line: Vec<char> = draw(&lanes, |i, lane| {
                if inserted.contains(&i) || lane.is_none() {
                    ' '
                } else {
                    '|'
                }
            })
            .chars()
            .collect();
            for at in &inserted {
                let pos = 2 * at - 1;
                if pos < line.len() {
                    line[pos] = '\\';
                }
            }
            after.push(line.into_iter().collect::<String>().trim_end().to_string());
        }
        loop {
            let mut changed = false;
            for j in (1..lanes.len()).rev() {
                let Some(value) = lanes[j] else { continue };
                if lanes[..j].contains(&Some(value)) {
                    let mut line: Vec<char> = draw(&lanes, |i, lane| {
                        if i >= j || lane.is_none() {
                            ' '
                        } else {
                            '|'
                        }
                    })
                    .chars()
                    .collect();
                    for k in j..lanes.len() {
                        if lanes[k].is_some() {
                            let pos = 2 * k - 1;
                            if pos < line.len() {
                                line[pos] = '/';
                            }
                        }
                    }
                    after.push(line.into_iter().collect::<String>().trim_end().to_string());
                    lanes.remove(j);
                    changed = true;
                    break;
                }
            }
            if !changed {
                break;
            }
        }
        while lanes.last().is_some_and(|l| l.is_none()) {
            lanes.pop();
        }
        rows.push(GraphRow {
            commit: commit_line,
            body,
            after,
        });
    }
    rows
}

fn draw(lanes: &[Option<Oid>], mark: impl Fn(usize, &Option<Oid>) -> char) -> String {
    let mut out = String::new();
    for (i, lane) in lanes.iter().enumerate() {
        out.push(mark(i, lane));
        out.push(' ');
    }
    if out.is_empty() {
        out.push_str("* ");
    }
    out
}
