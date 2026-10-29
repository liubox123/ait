use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context};

use super::diff::{diff_bytes, ChangeKind, FileDiff, PathChange};
use super::ignore::Ignore;
use super::index::{self, FileStat, Index, IndexEntry};
use super::merge::{
    adjust_renames, directory_file_conflicts, merge_maps, merge_mode, merge_text, FileMap,
    MergeAction, TextOptions,
};
use super::object::{
    hash_payload, Commit, Object, Signature, TreeEntry, MODE_EXEC, MODE_LINK, MODE_TREE,
};
use super::odb;
use super::oid::Oid;
use super::pack::PackCache;
use std::cell::RefCell;

mod maint;
mod refs_log;
mod rewrite;
mod stash;
mod view;

pub use refs_log::ReflogEntry;
pub use rewrite::{CleanOptions, ResetMode, RestoreSource, SeqOutcome, Todo, TodoAction};
pub use stash::StashEntry;
pub use maint::{FsckReport, GcReport, ObjectCount};
pub use view::{render_graph, BlameLine, DiffSide, GraphRow, GrepMatch, GrepOptions, LogFilter, TagInfo};

#[derive(Clone, Debug)]
pub enum Head {
    Symbolic(String),
    Detached(Oid),
}

#[derive(Clone, Debug)]
pub struct StatusEntry {
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub branch: Option<String>,
    pub head: Option<Oid>,
    pub staged: Vec<StatusEntry>,
    pub unstaged: Vec<StatusEntry>,
    pub untracked: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CommitView {
    pub oid: Oid,
    pub summary: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub time: i64,
    pub tz: String,
    pub parents: Vec<Oid>,
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub mode: u32,
    pub oid: Oid,
    pub kind: String,
}

#[derive(Clone, Debug)]
pub struct BranchInfo {
    pub name: String,
    pub oid: Oid,
    pub current: bool,
}

#[derive(Clone, Debug)]
pub struct StoredObject {
    pub oid: Oid,
    pub kind: String,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct MergeOptions {
    pub no_ff: bool,
    pub ff_only: bool,
    pub strategy_ours: bool,
    pub favor: i8,
    pub diff3: bool,
}

struct MergedTree {
    files: FileMap,
    conflicts: Vec<String>,
    stages: BTreeMap<String, [Option<(u32, Oid)>; 3]>,
}

struct ResolvedBlob {
    mode: u32,
    oid: Oid,
    conflict: bool,
}

#[derive(Clone, Debug)]
pub enum MergeOutcome {
    UpToDate,
    FastForward(Oid),
    Merged(Oid),
    Conflicts(Vec<String>),
}

pub struct Repository {
    git_dir: PathBuf,
    work_tree: Option<PathBuf>,
    packs: RefCell<Option<PackCache>>,
    staged: RefCell<HashMap<Oid, (&'static str, Vec<u8>)>>,
}

impl Repository {
    pub fn init(path: &Path, bare: bool) -> anyhow::Result<Self> {
        let (git_dir, work_tree) = if bare {
            fs::create_dir_all(path)?;
            (path.to_path_buf(), None)
        } else {
            fs::create_dir_all(path)?;
            (path.join(".git"), Some(path.to_path_buf()))
        };
        fs::create_dir_all(git_dir.join("objects"))?;
        fs::create_dir_all(git_dir.join("refs/heads"))?;
        fs::create_dir_all(git_dir.join("refs/tags"))?;
        fs::create_dir_all(git_dir.join("refs/remotes"))?;
        fs::create_dir_all(git_dir.join("info"))?;
        let head = git_dir.join("HEAD");
        if !head.exists() {
            fs::write(head, "ref: refs/heads/main\n")?;
        }
        let config = git_dir.join("config");
        if !config.exists() {
            fs::write(config, initial_config(bare))?;
        }
        let description = git_dir.join("description");
        if !description.exists() {
            fs::write(
                description,
                "Unnamed repository; edit this file 'description' to name the repository.\n",
            )?;
        }
        let exclude = git_dir.join("info/exclude");
        if !exclude.exists() {
            fs::write(exclude, "")?;
        }
        Ok(Self {
            git_dir,
            work_tree,
            packs: RefCell::new(None),
            staged: RefCell::new(HashMap::new()),
        })
    }

    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let link = path.join(".git");
        if link.is_dir() {
            return Ok(Self::from_dirs(link, Some(path.to_path_buf())));
        }
        if link.is_file() {
            let text = fs::read_to_string(&link)?;
            let gitdir = text
                .trim()
                .strip_prefix("gitdir:")
                .context("无法识别 .git 文件")?
                .trim();
            let git_dir = if Path::new(gitdir).is_absolute() {
                PathBuf::from(gitdir)
            } else {
                path.join(gitdir)
            };
            return Ok(Self::from_dirs(git_dir, Some(path.to_path_buf())));
        }
        if path.join("HEAD").is_file() && path.join("objects").is_dir() {
            let bare = config_lookup(
                &fs::read_to_string(path.join("config")).unwrap_or_default(),
                "core.bare",
            )
            .map(|value| value == "true")
            .unwrap_or(true);
            if bare {
                return Ok(Self::from_dirs(path.to_path_buf(), None));
            }
            let work = if path.file_name().and_then(|s| s.to_str()) == Some(".git") {
                path.parent().map(|parent| parent.to_path_buf())
            } else {
                None
            };
            return Ok(Self::from_dirs(path.to_path_buf(), work));
        }
        bail!("不是 Git 仓库: {}", path.display());
    }

    pub fn discover(start: &Path) -> anyhow::Result<Self> {
        let mut current = if start.is_file() {
            start.parent().unwrap_or(start).to_path_buf()
        } else {
            start.to_path_buf()
        };
        if !current.is_absolute() {
            current = std::env::current_dir()?.join(current);
        }
        loop {
            if current.join(".git").exists() {
                return Self::open(&current);
            }
            if current.join("HEAD").is_file() && current.join("objects").is_dir() {
                return Self::open(&current);
            }
            if !current.pop() {
                bail!("不在 Git 仓库中");
            }
        }
    }

    fn from_dirs(git_dir: PathBuf, work_tree: Option<PathBuf>) -> Self {
        Self {
            git_dir,
            work_tree,
            packs: RefCell::new(None),
            staged: RefCell::new(HashMap::new()),
        }
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub fn work_tree(&self) -> Option<&Path> {
        self.work_tree.as_deref()
    }

    pub fn is_bare(&self) -> bool {
        self.work_tree.is_none()
    }

    pub fn head(&self) -> anyhow::Result<Head> {
        match read_ref_file(&self.git_dir.join("HEAD"))? {
            Some(RefBody::Direct(oid)) => Ok(Head::Detached(oid)),
            Some(RefBody::Symbolic(name)) => Ok(Head::Symbolic(name)),
            None => bail!("缺少 HEAD"),
        }
    }

    pub fn head_oid(&self) -> anyhow::Result<Option<Oid>> {
        match self.head()? {
            Head::Detached(oid) => Ok(Some(oid)),
            Head::Symbolic(name) => self.resolve_ref(&name),
        }
    }

    pub fn branch_name(&self) -> anyhow::Result<Option<String>> {
        match self.head()? {
            Head::Symbolic(name) => Ok(Some(name.trim_start_matches("refs/heads/").to_string())),
            Head::Detached(_) => Ok(None),
        }
    }

    pub fn resolve_ref(&self, name: &str) -> anyhow::Result<Option<Oid>> {
        let mut name = name.to_string();
        for _ in 0..10 {
            if !valid_ref(&name) && name != "HEAD" {
                bail!("非法引用: {name}");
            }
            let path = if name == "HEAD" {
                self.git_dir.join("HEAD")
            } else {
                self.git_dir.join(&name)
            };
            match read_ref_file(&path)? {
                Some(RefBody::Direct(oid)) => return Ok(Some(oid)),
                Some(RefBody::Symbolic(next)) => {
                    name = next;
                    continue;
                }
                None => {
                    for (packed_name, oid) in self.packed_refs()? {
                        if packed_name == name {
                            return Ok(Some(oid));
                        }
                    }
                    return Ok(None);
                }
            }
        }
        bail!("引用嵌套过深");
    }

    /// 只解析名字本身：HEAD、引用、分支、标签、远端分支和（缩写）对象名，不处理 `~`、`^`、`@{}`。
    fn resolve_name(&self, name: &str) -> anyhow::Result<Oid> {
        let name = name.trim();
        if name.is_empty() {
            bail!("缺少版本参数");
        }
        if name == "HEAD" || name == "@" {
            return self.head_oid()?.context("当前还没有提交");
        }
        if matches!(
            name,
            "ORIG_HEAD" | "MERGE_HEAD" | "FETCH_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD"
        ) {
            let path = self.git_dir.join(name);
            if path.is_file() {
                let text = fs::read_to_string(path)?;
                let first = text.split_whitespace().next().unwrap_or("");
                return Oid::parse(first);
            }
            bail!("{name} 不存在");
        }
        if name.starts_with("refs/") {
            return self
                .resolve_ref(name)?
                .with_context(|| format!("找不到 {name}"));
        }
        for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
            if let Some(oid) = self.resolve_ref(&format!("{prefix}{name}"))? {
                return Ok(oid);
            }
        }
        if name.len() == 40 && name.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Oid::parse(name);
        }
        if (4..40).contains(&name.len()) && name.chars().all(|ch| ch.is_ascii_hexdigit()) {
            let matches = self.match_prefix(name)?;
            if matches.len() == 1 {
                return Ok(matches[0]);
            }
            if matches.len() > 1 {
                bail!("对象名 {name} 对应多个对象");
            }
        }
        bail!("无法解析 {name}");
    }

    pub fn set_ref(&self, name: &str, oid: Oid) -> anyhow::Result<()> {
        self.set_ref_logged(name, oid, "update by ait")
    }

    /// 写引用并记 reflog；HEAD 指向这个引用时，HEAD 的 reflog 也记一笔。
    pub fn set_ref_logged(&self, name: &str, oid: Oid, message: &str) -> anyhow::Result<()> {
        if !name.starts_with("refs/") || !valid_ref(name) {
            bail!("非法引用名 {name}");
        }
        let old = self.resolve_ref(name)?.unwrap_or(Oid::ZERO);
        atomic_write(&self.git_dir.join(name), format!("{oid}\n").as_bytes())?;
        if old != oid {
            self.append_reflog(name, old, oid, message)?;
            if matches!(self.head(), Ok(Head::Symbolic(ref target)) if target == name) {
                self.append_reflog("HEAD", old, oid, message)?;
            }
        }
        Ok(())
    }

    /// 让 HEAD 指向新提交：在分支上就移动分支，分离状态就直接改 HEAD。
    pub fn update_head(&self, oid: Oid, message: &str) -> anyhow::Result<()> {
        match self.head()? {
            Head::Symbolic(name) => self.set_ref_logged(&name, oid, message),
            Head::Detached(old) => {
                self.set_head_detached(oid)?;
                if old != oid {
                    self.append_reflog("HEAD", old, oid, message)?;
                }
                Ok(())
            }
        }
    }

    pub fn delete_ref(&self, name: &str) -> anyhow::Result<()> {
        if !valid_ref(name) {
            bail!("非法引用名 {name}");
        }
        let path = self.git_dir.join(name);
        if path.exists() {
            fs::remove_file(path)?;
        }
        let log = self.git_dir.join("logs").join(name);
        if log.is_file() {
            let _ = fs::remove_file(log);
        }
        self.rewrite_packed_without(name)
    }

    pub fn set_head_symbolic(&self, name: &str) -> anyhow::Result<()> {
        if !name.starts_with("refs/") || !valid_ref(name) {
            bail!("非法 HEAD 目标 {name}");
        }
        atomic_write(
            &self.git_dir.join("HEAD"),
            format!("ref: {name}\n").as_bytes(),
        )
    }

    pub fn set_head_detached(&self, oid: Oid) -> anyhow::Result<()> {
        atomic_write(&self.git_dir.join("HEAD"), format!("{oid}\n").as_bytes())
    }

    pub fn list_refs(&self, prefix: &str) -> anyhow::Result<Vec<(String, Oid)>> {
        let mut map = BTreeMap::new();
        for (name, oid) in self.packed_refs()? {
            if name.starts_with(prefix) {
                map.insert(name, oid);
            }
        }
        let root = self.git_dir.join("refs");
        if root.is_dir() {
            walk_ref_dir(&root, "refs", &mut map)?;
        }
        Ok(map
            .into_iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .collect())
    }

    pub fn branches(&self) -> anyhow::Result<Vec<BranchInfo>> {
        let current = self.branch_name()?;
        let mut out = Vec::new();
        for (name, oid) in self.list_refs("refs/heads/")? {
            let short = name.trim_start_matches("refs/heads/").to_string();
            out.push(BranchInfo {
                current: current.as_deref() == Some(short.as_str()),
                name: short,
                oid,
            });
        }
        Ok(out)
    }

    pub fn create_branch(&self, name: &str, oid: Oid) -> anyhow::Result<()> {
        if !valid_branch(name) {
            bail!("非法分支名: {name}");
        }
        let refname = format!("refs/heads/{name}");
        if self.resolve_ref(&refname)?.is_some() {
            bail!("分支已存在: {name}");
        }
        self.set_ref_logged(&refname, oid, "branch: Created from HEAD")
    }

    pub fn delete_branch(&self, name: &str) -> anyhow::Result<()> {
        let refname = format!("refs/heads/{name}");
        if self.branch_name()?.as_deref() == Some(name) {
            bail!("不能删除当前所在分支");
        }
        if self.resolve_ref(&refname)?.is_none() {
            bail!("分支不存在: {name}");
        }
        self.delete_ref(&refname)
    }

    pub fn read_object(&self, oid: Oid) -> anyhow::Result<Object> {
        if let Some(raw) = odb::read_loose(&self.git_dir, oid)? {
            return odb::parse_stored(&raw);
        }
        if let Some(found) = self.with_packs(|packs| packs.get(oid))? {
            return found;
        }
        if let Some((kind, payload)) = self.staged.borrow().get(&oid) {
            return Object::parse(kind, payload);
        }
        bail!("找不到对象 {oid}")
    }

    pub fn read_commit(&self, oid: Oid) -> anyhow::Result<Commit> {
        match self.read_object(oid)? {
            Object::Commit(commit) => Ok(commit),
            _ => bail!("{oid} 不是提交"),
        }
    }

    pub fn read_blob(&self, oid: Oid) -> anyhow::Result<Vec<u8>> {
        match self.read_object(oid)? {
            Object::Blob(bytes) => Ok(bytes),
            _ => bail!("{oid} 不是文件内容"),
        }
    }

    pub fn write_object(&self, kind: &str, payload: &[u8]) -> anyhow::Result<Oid> {
        match kind {
            "blob" | "tree" | "commit" | "tag" => {}
            other => bail!("不能写入未知对象类型 {other}"),
        }
        odb::write_loose(&self.git_dir, kind, payload)
    }

    pub fn ingest(&self, kind: &str, payload: &[u8]) -> anyhow::Result<Oid> {
        self.write_object(kind, payload)
    }

    pub fn tree_files(&self, tree: Oid) -> anyhow::Result<FileMap> {
        let mut out = FileMap::new();
        self.walk_tree(tree, "", &mut out)?;
        Ok(out)
    }

    pub fn list_dir(&self, oid: Oid, path: &str) -> anyhow::Result<Vec<DirEntry>> {
        let mut tree = match self.read_object(oid)? {
            Object::Commit(commit) => commit.tree,
            Object::Tree(_) => oid,
            _ => bail!("不是提交或目录"),
        };
        let path = path.trim_matches('/');
        if !path.is_empty() {
            for segment in path.split('/') {
                tree = self.tree_child(tree, segment, true)?;
            }
        }
        let Object::Tree(entries) = self.read_object(tree)? else {
            bail!("不是目录");
        };
        let mut out = Vec::new();
        for entry in entries {
            let child = if path.is_empty() {
                entry.name.clone()
            } else {
                format!("{path}/{}", entry.name)
            };
            let kind = if entry.mode == MODE_TREE {
                "dir"
            } else if entry.mode == MODE_EXEC {
                "exec"
            } else if entry.mode == MODE_LINK {
                "link"
            } else {
                "file"
            };
            out.push(DirEntry {
                name: entry.name,
                path: child,
                mode: entry.mode,
                oid: entry.oid,
                kind: kind.to_string(),
            });
        }
        out.sort_by(|a, b| {
            (b.kind == "dir")
                .cmp(&(a.kind == "dir"))
                .then(a.name.cmp(&b.name))
        });
        Ok(out)
    }

    pub fn file_at(&self, oid: Oid, path: &str) -> anyhow::Result<(u32, Oid, Vec<u8>)> {
        let tree = match self.read_object(oid)? {
            Object::Commit(commit) => commit.tree,
            Object::Tree(_) => oid,
            _ => bail!("不是提交或目录"),
        };
        let path = path.trim_matches('/');
        if path.is_empty() {
            bail!("没有指定文件");
        }
        let mut parts: Vec<&str> = path.split('/').collect();
        let last = parts.pop().context("空路径")?;
        let mut current = tree;
        for segment in parts {
            current = self.tree_child(current, segment, true)?;
        }
        let Object::Tree(entries) = self.read_object(current)? else {
            bail!("不是目录");
        };
        let entry = entries
            .into_iter()
            .find(|entry| entry.name == last)
            .context("文件不存在")?;
        if entry.mode == MODE_TREE {
            bail!("{path} 是目录");
        }
        let bytes = self.read_blob(entry.oid)?;
        Ok((entry.mode, entry.oid, bytes))
    }

    pub fn log(&self, start: Oid, limit: usize) -> anyhow::Result<Vec<CommitView>> {
        // 与 git log 相同：按提交时间从新到旧弹出，父提交只在子提交弹出后才入队，
        // 所以时间相同时子提交也排在父提交前面。
        let mut queue: BinaryHeap<(i64, Reverse<u64>, Oid)> = BinaryHeap::new();
        let mut pending: HashMap<Oid, Commit> = HashMap::new();
        let mut seen = HashSet::new();
        let mut seq = 0u64;
        let mut frontier = vec![start];
        let mut commits = Vec::new();
        loop {
            for oid in frontier.drain(..) {
                if !seen.insert(oid) {
                    continue;
                }
                if let Ok(Object::Commit(commit)) = self.read_object(oid) {
                    queue.push((commit.committer.time, Reverse(seq), oid));
                    pending.insert(oid, commit);
                    seq += 1;
                }
            }
            if commits.len() >= limit {
                break;
            }
            let Some((_, _, oid)) = queue.pop() else {
                break;
            };
            let Some(commit) = pending.remove(&oid) else {
                continue;
            };
            frontier.extend(commit.parents.iter().copied());
            commits.push((oid, commit));
        }
        Ok(commits
            .into_iter()
            .map(|(oid, commit)| CommitView {
                oid,
                summary: commit.summary().to_string(),
                message: commit.message.clone(),
                author_name: commit.author.name,
                author_email: commit.author.email,
                time: commit.author.time,
                tz: commit.author.tz,
                parents: commit.parents,
            })
            .collect())
    }

    pub fn changed_paths(&self, from: Option<Oid>, to: Oid) -> anyhow::Result<Vec<PathChange>> {
        let old = self.files_of_commit(from)?;
        let new = self.files_of_commit(Some(to))?;
        Ok(path_changes(&old, &new))
    }

    pub fn diff_range(&self, from: Option<Oid>, to: Oid) -> anyhow::Result<Vec<FileDiff>> {
        let old = self.files_of_commit(from)?;
        let new = self.files_of_commit(Some(to))?;
        self.diffs_between(&old, &new)
    }

    pub fn is_ancestor(&self, ancestor: Oid, descendant: Oid) -> anyhow::Result<bool> {
        if ancestor == descendant {
            return Ok(true);
        }
        let mut stack = vec![descendant];
        let mut seen = HashSet::new();
        while let Some(oid) = stack.pop() {
            if !seen.insert(oid) {
                continue;
            }
            let Ok(Object::Commit(commit)) = self.read_object(oid) else {
                continue;
            };
            for parent in commit.parents {
                if parent == ancestor {
                    return Ok(true);
                }
                stack.push(parent);
            }
        }
        Ok(false)
    }

    pub fn merge_base(&self, left: Oid, right: Oid) -> anyhow::Result<Option<Oid>> {
        Ok(self.best_merge_bases(left, right)?.into_iter().next())
    }

    /// 所有最佳合并基。交叉合并时会多于一个，之后要先合成虚拟祖先。
    pub fn best_merge_bases(&self, left: Oid, right: Oid) -> anyhow::Result<Vec<Oid>> {
        let mut ancestors = HashSet::new();
        let mut queue = VecDeque::from([left]);
        while let Some(oid) = queue.pop_front() {
            if !ancestors.insert(oid) {
                continue;
            }
            if let Ok(Object::Commit(commit)) = self.read_object(oid) {
                queue.extend(commit.parents);
            }
        }
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([right]);
        while let Some(oid) = queue.pop_front() {
            if !seen.insert(oid) {
                continue;
            }
            if ancestors.contains(&oid) {
                found.push(oid);
                continue;
            }
            if let Ok(Object::Commit(commit)) = self.read_object(oid) {
                queue.extend(commit.parents);
            }
        }
        let mut best = Vec::new();
        for oid in &found {
            let dominated = found
                .iter()
                .any(|other| other != oid && self.is_ancestor(*oid, *other).unwrap_or(false));
            if !dominated {
                best.push(*oid);
            }
        }
        Ok(best)
    }

    /// 把 `theirs` 合进 `ours`，不修改工作区。快进时不产生新提交。
    pub fn integrate(
        &self,
        ours: Oid,
        theirs: Oid,
        message: &str,
        author: &Signature,
    ) -> anyhow::Result<MergeOutcome> {
        if ours == theirs || self.is_ancestor(theirs, ours)? {
            return Ok(MergeOutcome::UpToDate);
        }
        if self.is_ancestor(ours, theirs)? {
            return Ok(MergeOutcome::FastForward(theirs));
        }
        let base_files = self.base_files_for(ours, theirs)?;
        let our_files = self.files_of_commit(Some(ours))?;
        let their_files = self.files_of_commit(Some(theirs))?;
        let merged = self.merge_file_maps(
            &base_files,
            &our_files,
            &their_files,
            "theirs",
            &TextOptions::default(),
        )?;
        if !merged.conflicts.is_empty() {
            return Ok(MergeOutcome::Conflicts(merged.conflicts));
        }
        let tree = self.write_tree(&merged.files)?;
        let commit = Commit {
            tree,
            parents: vec![ours, theirs],
            author: author.clone(),
            committer: author.clone(),
            message: {
                let mut message = message.trim().to_string();
                if !message.ends_with('\n') {
                    message.push('\n');
                }
                message
            },
        };
        let oid = self.write_object("commit", &commit.encode())?;
        Ok(MergeOutcome::Merged(oid))
    }

    pub fn objects_since(&self, tip: Oid, have: &[Oid]) -> anyhow::Result<Vec<StoredObject>> {
        let mut known = HashSet::new();
        for oid in have {
            if !oid.is_zero() {
                let _ = self.mark_reachable(*oid, &mut known);
            }
        }
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![tip];
        while let Some(oid) = stack.pop() {
            if oid.is_zero() || !seen.insert(oid) || known.contains(&oid) {
                continue;
            }
            let object = self.read_object(oid)?;
            match &object {
                Object::Commit(commit) => {
                    stack.push(commit.tree);
                    stack.extend(commit.parents.iter().copied());
                }
                Object::Tree(entries) => {
                    for entry in entries {
                        stack.push(entry.oid);
                    }
                }
                Object::Blob(_) | Object::Tag(_) => {}
            }
            out.push(StoredObject {
                oid,
                kind: object.kind().to_string(),
                payload: object.payload(),
            });
        }
        Ok(out)
    }

    pub fn status(&self) -> anyhow::Result<Status> {
        let work = self.work_tree_required()?.to_path_buf();
        let _ = work;
        let head_files = self.head_files()?;
        let index = self.read_index()?;
        let work_files = self.scan_files("")?;
        let mut staged = Vec::new();
        let mut unstaged = Vec::new();
        let mut untracked = Vec::new();
        let mut seen_unmerged = BTreeSet::new();
        for entry in &index.entries {
            if entry.stage > 0 {
                if seen_unmerged.insert(entry.path.clone()) {
                    staged.push(StatusEntry {
                        path: entry.path.clone(),
                        kind: ChangeKind::Unmerged,
                    });
                }
                continue;
            }
            match head_files.get(&entry.path) {
                None => staged.push(StatusEntry {
                    path: entry.path.clone(),
                    kind: ChangeKind::Added,
                }),
                Some((mode, oid)) if *mode != entry.mode || *oid != entry.oid => {
                    staged.push(StatusEntry {
                        path: entry.path.clone(),
                        kind: ChangeKind::Modified,
                    })
                }
                _ => {}
            }
            let disk = self.work_path(&entry.path)?;
            if fs::symlink_metadata(&disk).is_err() {
                unstaged.push(StatusEntry {
                    path: entry.path.clone(),
                    kind: ChangeKind::Deleted,
                });
            } else {
                let (mode, oid) = self.hash_work(&entry.path)?;
                if mode != entry.mode || oid != entry.oid {
                    unstaged.push(StatusEntry {
                        path: entry.path.clone(),
                        kind: ChangeKind::Modified,
                    });
                }
            }
        }
        for path in head_files.keys() {
            if index.get(path).is_none() {
                staged.push(StatusEntry {
                    path: path.clone(),
                    kind: ChangeKind::Deleted,
                });
            }
        }
        for path in work_files {
            if index.get(&path).is_none() {
                untracked.push(path);
            }
        }
        staged.sort_by(|a, b| a.path.cmp(&b.path));
        unstaged.sort_by(|a, b| a.path.cmp(&b.path));
        untracked.sort();
        Ok(Status {
            branch: self.branch_name()?,
            head: self.head_oid()?,
            staged,
            unstaged,
            untracked,
        })
    }

    pub fn add(&self, specs: &[String]) -> anyhow::Result<()> {
        self.work_tree_required()?;
        let specs = if specs.is_empty() {
            vec![".".to_string()]
        } else {
            specs.to_vec()
        };
        let mut index = self.read_index()?;
        for spec in &specs {
            let rel = self.resolve_spec(spec)?;
            if rel.is_empty() {
                self.stage_tree(&mut index, "")?;
                continue;
            }
            let disk = self.work_path(&rel)?;
            let meta = fs::symlink_metadata(&disk).ok();
            if meta.as_ref().map(|item| item.is_dir()).unwrap_or(false) {
                self.stage_tree(&mut index, &rel)?;
            } else if meta.is_some() {
                self.stage_one(&mut index, &rel)?;
            } else if !index.remove(&rel) {
                bail!("路径不存在: {rel}");
            }
        }
        self.write_index_file(&index)?;
        self.forget_conflicts(&specs)
    }

    pub fn commit(&self, message: &str, all: bool, author: &Signature) -> anyhow::Result<Oid> {
        self.commit_with(message, all, author, false)
    }

    /// `amend` 时沿用原提交的父提交和作者；说明为空则沿用原说明。
    /// 拣选冲突解决后提交时，作者沿用被拣选的提交。
    pub fn commit_with(
        &self,
        message: &str,
        all: bool,
        committer: &Signature,
        amend: bool,
    ) -> anyhow::Result<Oid> {
        let mut message = message.trim().to_string();
        let picking = self.pick_head()?;
        let merging = self.merge_head()?;
        if amend && merging.is_some() {
            bail!("合并进行中不能 --amend");
        }
        if message.is_empty() {
            if amend {
                let head = self.head_oid()?.context("还没有提交，不能 --amend")?;
                message = self.read_commit(head)?.message.trim().to_string();
            } else if merging.is_some() || picking.is_some() || self.revert_head()?.is_some() {
                message = fs::read_to_string(self.git_dir.join("MERGE_MSG"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
            }
        }
        if message.is_empty() {
            bail!("提交说明不能为空");
        }
        if merging.is_some() || picking.is_some() || self.revert_head()?.is_some() {
            let marked = self.find_conflict_markers()?;
            if !marked.is_empty() {
                bail!("仍有冲突标记: {}", marked.join(", "));
            }
            let pending = self.read_conflict_list()?;
            if !pending.is_empty() {
                bail!("仍有未解决的冲突: {}", pending.join(", "));
            }
        }
        if all {
            self.stage_tracked()?;
        }
        let index = self.read_index()?;
        if index.entries.iter().any(|entry| entry.stage > 0) {
            bail!("暂存区仍有未合并的路径，请先 ait add");
        }
        if index.entries.is_empty() && self.head_oid()?.is_none() {
            bail!("没有需要提交的内容");
        }
        let tree = self.write_tree(&index_map(&index))?;
        let mut parents = Vec::new();
        let mut author = committer.clone();
        let kind;
        if amend {
            let head = self.head_oid()?.context("还没有提交，不能 --amend")?;
            let old = self.read_commit(head)?;
            parents = old.parents;
            author = old.author;
            kind = "commit (amend)";
        } else if let Some(head) = self.head_oid()? {
            let current = self.read_commit(head)?;
            if current.tree == tree && merging.is_none() {
                bail!("没有需要提交的内容");
            }
            parents.push(head);
            if let Some(extra) = merging {
                if extra != head {
                    parents.push(extra);
                }
            }
            if let Some(picked) = picking {
                author = self.read_commit(picked)?.author;
            }
            kind = if parents.len() > 1 {
                "commit (merge)"
            } else {
                "commit"
            };
        } else {
            kind = "commit (initial)";
        }
        let mut text = message;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let summary = text.lines().next().unwrap_or("").to_string();
        let commit = Commit {
            tree,
            parents,
            author,
            committer: committer.clone(),
            message: text,
        };
        let oid = self.write_object("commit", &commit.encode())?;
        self.update_head(oid, &format!("{kind}: {summary}"))?;
        self.clear_merge()?;
        self.clear_pick()?;
        Ok(oid)
    }

    pub fn diff_unstaged(&self) -> anyhow::Result<Vec<FileDiff>> {
        let status = self.status()?;
        let index = self.read_index()?;
        let mut diffs = Vec::new();
        for entry in &status.unstaged {
            let old = match index.get(&entry.path) {
                Some(item) => Some(self.read_blob(item.oid)?),
                None => None,
            };
            let new = if entry.kind == ChangeKind::Deleted {
                None
            } else {
                Some(self.read_work_bytes(&entry.path)?)
            };
            diffs.push(diff_bytes(&entry.path, old.as_deref(), new.as_deref()));
        }
        Ok(diffs)
    }

    pub fn diff_staged(&self) -> anyhow::Result<Vec<FileDiff>> {
        let head = self.head_files()?;
        let index = self.read_index()?;
        let new = index_map(&index);
        self.diffs_between(&head, &new)
    }

    pub fn checkout(&self, target: &str, create: bool) -> anyhow::Result<String> {
        self.checkout_from(target, create, None)
    }

    /// `create` 时以 `start`（默认 HEAD）为起点建分支并切换。目标不是分支时分离 HEAD。
    pub fn checkout_from(
        &self,
        target: &str,
        create: bool,
        start: Option<&str>,
    ) -> anyhow::Result<String> {
        let from = self.head_label()?;
        let old = self.head_oid()?;
        if create {
            if !valid_branch(target) {
                bail!("非法分支名: {target}");
            }
            let oid = match start {
                Some(spec) => self.rev_parse(spec)?,
                None => old.context("还没有提交，不能创建分支")?,
            };
            if self.resolve_ref(&format!("refs/heads/{target}"))?.is_some() {
                bail!("分支已存在: {target}");
            }
            self.switch_carrying(oid)?;
            self.set_ref_logged(
                &format!("refs/heads/{target}"),
                oid,
                &format!("branch: Created from {}", start.unwrap_or("HEAD")),
            )?;
            self.set_head_symbolic(&format!("refs/heads/{target}"))?;
            self.append_reflog(
                "HEAD",
                old.unwrap_or(Oid::ZERO),
                oid,
                &format!("checkout: moving from {from} to {target}"),
            )?;
            if let Some(spec) = start {
                self.track_if_remote(target, spec)?;
            }
            return Ok(format!("已创建并切换到分支 {target}"));
        }
        let branch_ref = format!("refs/heads/{target}");
        if valid_branch(target) {
            if let Some(oid) = self.resolve_ref(&branch_ref)? {
                self.switch_carrying(oid)?;
                self.set_head_symbolic(&branch_ref)?;
                self.append_reflog(
                    "HEAD",
                    old.unwrap_or(Oid::ZERO),
                    oid,
                    &format!("checkout: moving from {from} to {target}"),
                )?;
                return Ok(format!("已切换到分支 {target}"));
            }
            if let Some(remote) = self.unique_remote_branch(target)? {
                return self.checkout_from(target, true, Some(&remote));
            }
        }
        let oid = self.rev_parse(target)?;
        self.switch_carrying(oid)?;
        self.set_head_detached(oid)?;
        self.append_reflog(
            "HEAD",
            old.unwrap_or(Oid::ZERO),
            oid,
            &format!("checkout: moving from {from} to {target}"),
        )?;
        Ok(format!("已分离 HEAD 到 {}", oid.short()))
    }

    fn head_label(&self) -> anyhow::Result<String> {
        Ok(match self.head()? {
            Head::Symbolic(name) => name.trim_start_matches("refs/heads/").to_string(),
            Head::Detached(oid) => oid.to_hex(),
        })
    }

    /// 只有一个远端有同名分支时返回 `<远端>/<分支>`，用于 `checkout <分支>` 自动建跟踪分支。
    fn unique_remote_branch(&self, name: &str) -> anyhow::Result<Option<String>> {
        let found: Vec<String> = self
            .list_refs("refs/remotes/")?
            .into_iter()
            .filter_map(|(refname, _)| {
                let short = refname.trim_start_matches("refs/remotes/");
                let (_, branch) = short.split_once('/')?;
                (branch == name).then(|| short.to_string())
            })
            .collect();
        Ok(if found.len() == 1 {
            found.into_iter().next()
        } else {
            None
        })
    }

    fn track_if_remote(&self, branch: &str, start: &str) -> anyhow::Result<()> {
        let full = format!("refs/remotes/{start}");
        if self.resolve_ref(&full).ok().flatten().is_none() {
            return Ok(());
        }
        if let Some((remote, upstream)) = start.split_once('/') {
            self.config_set(&format!("branch.{branch}.remote"), remote)?;
            self.config_set(
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{upstream}"),
            )?;
        }
        Ok(())
    }

    pub fn merge_oid(
        &self,
        theirs: Oid,
        label: &str,
        author: &Signature,
    ) -> anyhow::Result<MergeOutcome> {
        self.merge_with(theirs, label, author, &MergeOptions::default())
    }

    /// `no_ff` 即使可以快进也生成合并提交。`ff_only` 在需要真正合并时拒绝。
    /// `strategy_ours` 保留当前树。`favor` 为 1 或 -1 时，冲突块分别采用当前侧或并入侧。
    pub fn merge_with(
        &self,
        theirs: Oid,
        label: &str,
        author: &Signature,
        options: &MergeOptions,
    ) -> anyhow::Result<MergeOutcome> {
        let Some(ours) = self.head_oid()? else {
            let commit = self.read_commit(theirs)?;
            self.checkout_files(&self.tree_files(commit.tree)?)?;
            self.update_head(theirs, &format!("merge {label}: Fast-forward"))?;
            return Ok(MergeOutcome::FastForward(theirs));
        };
        if ours == theirs || self.is_ancestor(theirs, ours)? {
            return Ok(MergeOutcome::UpToDate);
        }
        if options.strategy_ours {
            self.require_clean()?;
            return self.write_merge_commit(ours, theirs, label, author);
        }
        if self.is_ancestor(ours, theirs)? {
            if options.no_ff {
                self.require_clean()?;
                self.switch_to(theirs)?;
                return self.write_merge_commit(ours, theirs, label, author);
            }
            self.switch_to(theirs)?;
            self.update_head(theirs, &format!("merge {label}: Fast-forward"))?;
            return Ok(MergeOutcome::FastForward(theirs));
        }
        if options.ff_only {
            bail!("不是快进，已拒绝合并");
        }
        self.require_clean()?;
        atomic_write(
            &self.git_dir.join("ORIG_HEAD"),
            format!("{ours}\n").as_bytes(),
        )?;
        let bases = self.best_merge_bases(ours, theirs)?;
        let base_label = if bases.len() == 1 {
            bases[0].short()
        } else {
            "merged common ancestors".to_string()
        };
        let base_files = self.combine_bases(&bases)?;
        let our_files = self.tree_files(self.read_commit(ours)?.tree)?;
        let their_files = self.tree_files(self.read_commit(theirs)?.tree)?;
        for (path, (mode, oid)) in &their_files {
            if our_files.contains_key(path) {
                continue;
            }
            let disk = self.work_path(path)?;
            if fs::symlink_metadata(&disk).is_ok() {
                let (have_mode, have_oid) = self.hash_work(path)?;
                if have_mode != *mode || have_oid != *oid {
                    bail!("未跟踪文件会被覆盖: {path}");
                }
            }
        }
        let mut text = TextOptions {
            diff3: options.diff3,
            favor: options.favor,
            base_label,
        };
        if !text.diff3 && self.config_get("merge.conflictStyle")?.as_deref() == Some("diff3") {
            text.diff3 = true;
        }
        let merged = self.merge_file_maps(&base_files, &our_files, &their_files, label, &text)?;
        for path in our_files.keys() {
            if !merged.files.contains_key(path) && !merged.conflicts.iter().any(|item| item == path)
            {
                let disk = self.work_path(path)?;
                if fs::symlink_metadata(&disk).is_ok() {
                    let _ = fs::remove_file(disk);
                }
            }
        }
        self.checkout_files(&merged.files)?;
        if merged.conflicts.is_empty() {
            self.write_merge_commit(ours, theirs, label, author)
        } else {
            self.write_unmerged_index(&merged)?;
            let message = format!("Merge {label}\n");
            fs::create_dir_all(self.git_dir.join("ait"))?;
            atomic_write(
                &self.git_dir.join("MERGE_HEAD"),
                format!("{theirs}\n").as_bytes(),
            )?;
            atomic_write(&self.git_dir.join("MERGE_MSG"), message.as_bytes())?;
            atomic_write(
                &self.git_dir.join("ait/MERGE_HEAD"),
                format!("{theirs}\n").as_bytes(),
            )?;
            atomic_write(&self.git_dir.join("ait/MERGE_MSG"), message.as_bytes())?;
            self.write_conflict_list(&merged.conflicts)?;
            Ok(MergeOutcome::Conflicts(merged.conflicts))
        }
    }

    pub fn merge_abort(&self) -> anyhow::Result<()> {
        if self.merge_head()?.is_none() {
            bail!("没有进行中的合并");
        }
        let oid = self.head_oid()?.context("没有当前提交")?;
        let files = self.tree_files(self.read_commit(oid)?.tree)?;
        let index = self.read_index()?;
        for entry in &index.entries {
            if !files.contains_key(&entry.path) {
                let disk = self.work_path(&entry.path)?;
                if fs::symlink_metadata(&disk).is_ok() {
                    let _ = fs::remove_file(disk);
                }
            }
        }
        self.checkout_files(&files)?;
        self.clear_merge()?;
        Ok(())
    }

    fn write_merge_commit(
        &self,
        ours: Oid,
        theirs: Oid,
        label: &str,
        author: &Signature,
    ) -> anyhow::Result<MergeOutcome> {
        let tree = self.write_tree(&index_map(&self.read_index()?))?;
        let commit = Commit {
            tree,
            parents: vec![ours, theirs],
            author: author.clone(),
            committer: author.clone(),
            message: format!("Merge {label}\n"),
        };
        let oid = self.write_object("commit", &commit.encode())?;
        self.update_head(
            oid,
            &format!("merge {label}: Merge made by the 'ort' strategy."),
        )?;
        self.clear_merge()?;
        Ok(MergeOutcome::Merged(oid))
    }

    fn base_files_for(&self, left: Oid, right: Oid) -> anyhow::Result<FileMap> {
        let bases = self.best_merge_bases(left, right)?;
        self.combine_bases(&bases)
    }

    fn combine_bases(&self, bases: &[Oid]) -> anyhow::Result<FileMap> {
        if bases.is_empty() {
            return Ok(FileMap::new());
        }
        let mut files = self.files_of_commit(Some(bases[0]))?;
        let mut current = bases[0];
        for (index, next) in bases.iter().enumerate().skip(1) {
            let ancestor = self.base_files_for(current, *next)?;
            let right = self.files_of_commit(Some(*next))?;
            files = self
                .merge_file_maps(
                    &ancestor,
                    &files,
                    &right,
                    "merge-base",
                    &TextOptions::default(),
                )?
                .files;
            if index + 1 < bases.len() {
                current = self.virtual_commit(&files, vec![current, *next])?;
            }
        }
        Ok(files)
    }

    fn virtual_commit(&self, files: &FileMap, parents: Vec<Oid>) -> anyhow::Result<Oid> {
        let tree = self.write_tree(files)?;
        let author = Signature {
            name: "ait".into(),
            email: "ait@local".into(),
            time: 0,
            tz: "+0000".into(),
        };
        let commit = Commit {
            tree,
            parents,
            author: author.clone(),
            committer: author,
            message: "virtual merge base\n".into(),
        };
        self.write_object("commit", &commit.encode())
    }

    fn merge_file_maps(
        &self,
        base: &FileMap,
        ours: &FileMap,
        theirs: &FileMap,
        label: &str,
        options: &TextOptions,
    ) -> anyhow::Result<MergedTree> {
        let mut base = base.clone();
        let mut ours = ours.clone();
        let mut theirs = theirs.clone();
        let mut conflicts = adjust_renames(&mut base, &mut ours, &mut theirs, &|oid| {
            self.read_blob(oid).unwrap_or_default()
        });
        let actions = merge_maps(&base, &ours, &theirs);
        let mut files = FileMap::new();
        let mut stages = BTreeMap::new();
        for (path, action) in actions {
            match action {
                MergeAction::Take(mode, oid) => {
                    files.insert(path, (mode, oid));
                }
                MergeAction::Delete => {}
                MergeAction::Conflict {
                    ours: our_side,
                    theirs: their_side,
                } => {
                    stages.insert(
                        path.clone(),
                        [base.get(&path).copied(), our_side, their_side],
                    );
                    if let Some(resolved) =
                        self.resolve_content(&path, &base, our_side, their_side, label, options)?
                    {
                        if resolved.conflict {
                            conflicts.push(path.clone());
                        } else {
                            stages.remove(&path);
                        }
                        files.insert(path, (resolved.mode, resolved.oid));
                    } else if our_side.is_some() || their_side.is_some() {
                        conflicts.push(path);
                    }
                }
            }
        }
        for path in directory_file_conflicts(&files) {
            if let Some(entry) = files.remove(&path) {
                files.insert(format!("{path}~HEAD"), entry);
            }
            conflicts.push(path);
        }
        conflicts.sort();
        conflicts.dedup();
        Ok(MergedTree {
            files,
            conflicts,
            stages,
        })
    }

    fn resolve_content(
        &self,
        path: &str,
        base: &FileMap,
        ours: Option<(u32, Oid)>,
        theirs: Option<(u32, Oid)>,
        label: &str,
        options: &TextOptions,
    ) -> anyhow::Result<Option<ResolvedBlob>> {
        let base_entry = base.get(path).copied();
        match (ours, theirs) {
            (Some(our), Some(their)) => {
                let base_bytes = match base_entry {
                    Some((_, oid)) => self.read_blob(oid)?,
                    None => Vec::new(),
                };
                let our_bytes = self.read_blob(our.1)?;
                let their_bytes = self.read_blob(their.1)?;
                let merged = merge_text(
                    &base_bytes,
                    &our_bytes,
                    &their_bytes,
                    "HEAD",
                    label,
                    options,
                );
                let (mode, mode_conflict) =
                    merge_mode(base_entry.map(|item| item.0), Some(our.0), Some(their.0));
                let oid = if merged.bytes == our_bytes {
                    our.1
                } else if merged.bytes == their_bytes {
                    their.1
                } else if base_entry.is_some_and(|(_, oid)| {
                    self.read_blob(oid).ok().as_deref() == Some(merged.bytes.as_slice())
                }) {
                    base_entry.unwrap().1
                } else {
                    self.write_object("blob", &merged.bytes)?
                };
                Ok(Some(ResolvedBlob {
                    mode,
                    oid,
                    conflict: merged.conflict || mode_conflict,
                }))
            }
            (Some(side), None) | (None, Some(side)) => Ok(Some(ResolvedBlob {
                mode: side.0,
                oid: side.1,
                conflict: true,
            })),
            (None, None) => Ok(None),
        }
    }

    fn write_unmerged_index(&self, merged: &MergedTree) -> anyhow::Result<()> {
        let mut index = self.read_index()?;
        for path in &merged.conflicts {
            let Some(sides) = merged.stages.get(path) else {
                continue;
            };
            index.remove(path);
            for (stage, side) in [(1u16, sides[0]), (2, sides[1]), (3, sides[2])] {
                let Some((mode, oid)) = side else {
                    continue;
                };
                index.upsert(IndexEntry {
                    path: path.clone(),
                    mode,
                    oid,
                    stat: FileStat::default(),
                    stage,
                });
            }
        }
        self.write_index_file(&index)
    }

    fn write_conflict_list(&self, paths: &[String]) -> anyhow::Result<()> {
        let path = self.git_dir.join("ait/MERGE_CONFLICTS");
        if paths.is_empty() {
            if path.exists() {
                fs::remove_file(path)?;
            }
            return Ok(());
        }
        fs::create_dir_all(self.git_dir.join("ait"))?;
        atomic_write(&path, format!("{}\n", paths.join("\n")).as_bytes())
    }

    fn read_conflict_list(&self) -> anyhow::Result<Vec<String>> {
        let path = self.git_dir.join("ait/MERGE_CONFLICTS");
        if !path.is_file() {
            return Ok(Vec::new());
        }
        Ok(fs::read_to_string(path)?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn forget_conflicts(&self, specs: &[String]) -> anyhow::Result<()> {
        let pending = self.read_conflict_list()?;
        if pending.is_empty() {
            return Ok(());
        }
        let mut matched = Vec::new();
        for spec in specs {
            matched.push(self.resolve_spec(spec).unwrap_or_default());
        }
        let left: Vec<String> = pending
            .into_iter()
            .filter(|path| {
                !matched.iter().any(|spec| {
                    spec.is_empty() || spec == path || path.starts_with(&format!("{spec}/"))
                })
            })
            .collect();
        self.write_conflict_list(&left)
    }

    pub fn config_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let text = fs::read_to_string(self.git_dir.join("config")).unwrap_or_default();
        Ok(config_lookup(&text, key))
    }

    pub fn config_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let path = self.git_dir.join("config");
        let text = fs::read_to_string(&path).unwrap_or_default();
        atomic_write(&path, config_upsert(&text, key, value).as_bytes())
    }

    /// 删除一个配置项；所在小节因此变空时整节删掉。
    pub fn config_unset_section_key(&self, key: &str) -> anyhow::Result<()> {
        let Some((section, subsection, name)) = split_config_key(key) else {
            bail!("非法配置项 {key}");
        };
        let path = self.git_dir.join("config");
        let text = fs::read_to_string(&path).unwrap_or_default();
        let mut out: Vec<String> = Vec::new();
        let mut in_section = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                in_section = header_matches(trimmed, &section, subsection.as_deref());
                out.push(line.to_string());
                continue;
            }
            if in_section {
                if let Some((left, _)) = trimmed.split_once('=') {
                    if left.trim() == name {
                        continue;
                    }
                }
            }
            out.push(line.to_string());
        }
        let mut cleaned: Vec<String> = Vec::new();
        for (index, line) in out.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                let empty = out[index + 1..]
                    .iter()
                    .take_while(|next| !next.trim().starts_with('['))
                    .all(|next| next.trim().is_empty());
                if empty {
                    continue;
                }
            }
            cleaned.push(line.clone());
        }
        let mut text = cleaned.join("\n");
        text.push('\n');
        atomic_write(&path, text.as_bytes())
    }

    /// 删除整个配置小节，例如 `remote.origin`。
    pub fn config_remove_section(&self, section: &str) -> anyhow::Result<()> {
        let (name, sub) = match section.split_once('.') {
            Some((name, sub)) => (name.to_string(), Some(sub.to_string())),
            None => (section.to_string(), None),
        };
        let path = self.git_dir.join("config");
        let text = fs::read_to_string(&path).unwrap_or_default();
        let mut out = Vec::new();
        let mut skipping = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                skipping = header_matches(trimmed, &name, sub.as_deref());
            }
            if !skipping {
                out.push(line);
            }
        }
        let mut text = out.join("\n");
        text.push('\n');
        atomic_write(&path, text.as_bytes())
    }

    /// 配置文件里所有 `[section "x"]` 的 x。
    pub fn config_subsections(&self, section: &str) -> anyhow::Result<Vec<String>> {
        let text = fs::read_to_string(self.git_dir.join("config")).unwrap_or_default();
        let mut out = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(header) = trimmed.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                if let Some((sec, sub)) = header.split_once(' ') {
                    if sec.trim() == section {
                        let sub = sub.trim().trim_matches('"').to_string();
                        if !out.contains(&sub) {
                            out.push(sub);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn rename_branch(&self, old: &str, new: &str, force: bool) -> anyhow::Result<()> {
        if !valid_branch(new) {
            bail!("非法分支名: {new}");
        }
        let old_ref = format!("refs/heads/{old}");
        let new_ref = format!("refs/heads/{new}");
        let oid = self
            .resolve_ref(&old_ref)?
            .with_context(|| format!("分支不存在: {old}"))?;
        if old == new {
            return Ok(());
        }
        if self.resolve_ref(&new_ref)?.is_some() && !force {
            bail!("分支已存在: {new}");
        }
        let current = self.branch_name()?.as_deref() == Some(old);
        let old_log = self.git_dir.join("logs").join(&old_ref);
        let new_log = self.git_dir.join("logs").join(&new_ref);
        let saved_log = fs::read(&old_log).ok();
        atomic_write(&self.git_dir.join(&new_ref), format!("{oid}\n").as_bytes())?;
        if let Some(bytes) = saved_log {
            if let Some(parent) = new_log.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&new_log, bytes)?;
        }
        if current {
            self.set_head_symbolic(&new_ref)?;
        }
        self.delete_ref(&old_ref)?;
        self.append_reflog(&new_ref, oid, oid, &format!("Branch: renamed {old_ref} to {new_ref}"))?;
        for key in ["remote", "merge"] {
            if let Some(value) = self.config_get(&format!("branch.{old}.{key}"))? {
                self.config_set(&format!("branch.{new}.{key}"), &value)?;
            }
        }
        self.config_remove_section(&format!("branch.{old}"))?;
        Ok(())
    }

    pub fn description(&self) -> anyhow::Result<String> {
        Ok(fs::read_to_string(self.git_dir.join("description")).unwrap_or_default())
    }

    pub fn set_description(&self, text: &str) -> anyhow::Result<()> {
        let mut text = text.trim().to_string();
        text.push('\n');
        atomic_write(&self.git_dir.join("description"), text.as_bytes())
    }

    fn switch_to(&self, oid: Oid) -> anyhow::Result<()> {
        if self.head_oid()? == Some(oid) {
            return Ok(());
        }
        let commit = self.read_commit(oid)?;
        let files = self.tree_files(commit.tree)?;
        self.require_clean()?;
        let status = self.status()?;
        for path in files.keys() {
            if status.untracked.iter().any(|item| item == path) {
                bail!("未跟踪文件会被覆盖: {path}");
            }
        }
        self.checkout_files(&files)
    }

    /// 与 `git checkout` 相同：本地修改随之保留，只有落在两次提交之间有变化的路径上才拒绝。
    fn switch_carrying(&self, oid: Oid) -> anyhow::Result<()> {
        if self.head_oid()? == Some(oid) {
            return Ok(());
        }
        let target = self.tree_files(self.read_commit(oid)?.tree)?;
        let head = self.head_files().unwrap_or_default();
        let status = self.status()?;
        if status.staged.iter().any(|entry| entry.kind == ChangeKind::Unmerged) {
            bail!("有未解决的冲突，请先解决或中止");
        }
        let mut index = self.read_index()?;
        let unstaged: BTreeSet<&str> = status.unstaged.iter().map(|e| e.path.as_str()).collect();
        let staged: BTreeSet<&str> = status.staged.iter().map(|e| e.path.as_str()).collect();
        let changed: BTreeSet<&String> = head
            .keys()
            .chain(target.keys())
            .filter(|path| head.get(*path) != target.get(*path))
            .collect();
        let mut blocked = Vec::new();
        let mut untracked = Vec::new();
        for path in &changed {
            let path = path.as_str();
            let entry = index.get(path).map(|entry| (entry.mode, entry.oid));
            let already = entry == target.get(path).copied() && !unstaged.contains(path);
            if (staged.contains(path) || unstaged.contains(path)) && !already {
                blocked.push(path);
            } else if !head.contains_key(path) && status.untracked.iter().any(|p| p == path) {
                untracked.push(path);
            }
        }
        if !blocked.is_empty() {
            bail!(
                "您对下列文件的本地修改将被检出操作覆盖：\n\t{}\n请在切换分支前提交或贮藏您的修改。",
                blocked.join("\n\t")
            );
        }
        if !untracked.is_empty() {
            bail!(
                "工作区中下列未跟踪的文件将会因为检出操作而被覆盖：\n\t{}\n请在切换分支前移动或删除。",
                untracked.join("\n\t")
            );
        }
        for path in changed.iter().filter(|path| !target.contains_key(path.as_str())) {
            let disk = self.work_path(path)?;
            if fs::symlink_metadata(&disk).is_ok() {
                let _ = fs::remove_file(&disk);
            }
            index.remove(path);
        }
        for path in changed {
            let Some((mode, oid)) = target.get(path.as_str()).copied() else {
                continue;
            };
            if index.get(path).map(|entry| (entry.mode, entry.oid)) == Some((mode, oid))
                && !unstaged.contains(path.as_str())
            {
                continue;
            }
            self.materialize(path, mode, oid)?;
            let stat = index::stat_path(&self.work_path(path)?)
                .map(|(_, stat)| stat)
                .unwrap_or_default();
            index.upsert(IndexEntry {
                path: path.clone(),
                mode,
                oid,
                stat,
                stage: 0,
            });
        }
        self.write_index_file(&index)
    }

    fn require_clean(&self) -> anyhow::Result<()> {
        let status = self.status()?;
        if !status.staged.is_empty() || !status.unstaged.is_empty() {
            bail!("工作区有未提交的修改，请先提交");
        }
        Ok(())
    }

    fn checkout_files(&self, files: &FileMap) -> anyhow::Result<()> {
        let old = self.head_files().unwrap_or_default();
        let mut remove: Vec<String> = old
            .keys()
            .filter(|path| !files.contains_key(*path))
            .cloned()
            .collect();
        remove.sort_by_key(|path| std::cmp::Reverse(path.len()));
        for path in remove {
            let disk = self.work_path(&path)?;
            if fs::symlink_metadata(&disk).is_ok() {
                let _ = fs::remove_file(&disk);
            }
        }
        for (path, (mode, oid)) in files {
            self.materialize(path, *mode, *oid)?;
        }
        self.write_index_from_map(files)
    }

    fn materialize(&self, rel: &str, mode: u32, oid: Oid) -> anyhow::Result<()> {
        let path = self.work_path(rel)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if path.is_dir() && !path.is_symlink() {
            fs::remove_dir_all(&path)?;
        }
        let bytes = self.read_blob(oid)?;
        if mode == MODE_LINK {
            #[cfg(unix)]
            {
                let target = String::from_utf8_lossy(&bytes);
                if fs::symlink_metadata(&path).is_ok() {
                    let _ = fs::remove_file(&path);
                }
                std::os::unix::fs::symlink(target.as_ref(), &path)?;
                return Ok(());
            }
        }
        if fs::symlink_metadata(&path).is_ok() {
            fs::remove_file(&path)?;
        }
        fs::write(&path, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let bits = if mode == MODE_EXEC { 0o755 } else { 0o644 };
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(bits));
        }
        Ok(())
    }

    fn write_index_from_map(&self, files: &FileMap) -> anyhow::Result<()> {
        let mut index = Index::default();
        for (path, (mode, oid)) in files {
            let disk = self.work_path(path)?;
            let stat = index::stat_path(&disk)
                .map(|(_, stat)| stat)
                .unwrap_or_default();
            index.upsert(IndexEntry {
                path: path.clone(),
                mode: *mode,
                oid: *oid,
                stat,
                stage: 0,
            });
        }
        self.write_index_file(&index)
    }

    fn stage_tracked(&self) -> anyhow::Result<()> {
        let mut index = self.read_index()?;
        let paths: Vec<String> = index
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        for path in paths {
            if fs::symlink_metadata(self.work_path(&path)?).is_err() {
                index.remove(&path);
            } else {
                self.stage_one(&mut index, &path)?;
            }
        }
        self.write_index_file(&index)
    }

    fn stage_tree(&self, index: &mut Index, prefix: &str) -> anyhow::Result<()> {
        if prefix.is_empty() {
            let tracked: Vec<String> = index
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect();
            for path in tracked {
                let disk = self.work_path(&path)?;
                match fs::symlink_metadata(&disk) {
                    Ok(meta) if meta.is_dir() => {
                        index.remove(&path);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        index.remove(&path);
                    }
                }
            }
        } else {
            index.remove_prefix(prefix);
        }
        for path in self.scan_files(prefix)? {
            self.stage_one(index, &path)?;
        }
        Ok(())
    }

    fn stage_one(&self, index: &mut Index, rel: &str) -> anyhow::Result<()> {
        let disk = self.work_path(rel)?;
        let (mode, stat) = index::stat_path(&disk)?;
        let bytes = read_disk(&disk, mode)?;
        let oid = self.write_object("blob", &bytes)?;
        index.remove(rel);
        index.upsert(IndexEntry {
            path: rel.to_string(),
            mode,
            oid,
            stat,
            stage: 0,
        });
        Ok(())
    }

    fn hash_work(&self, rel: &str) -> anyhow::Result<(u32, Oid)> {
        let disk = self.work_path(rel)?;
        let (mode, _) = index::stat_path(&disk)?;
        let bytes = read_disk(&disk, mode)?;
        Ok((mode, hash_payload("blob", &bytes).0))
    }

    fn read_work_bytes(&self, rel: &str) -> anyhow::Result<Vec<u8>> {
        let disk = self.work_path(rel)?;
        let (mode, _) = index::stat_path(&disk)?;
        read_disk(&disk, mode)
    }

    fn read_index(&self) -> anyhow::Result<Index> {
        let path = self.git_dir.join("index");
        if !path.exists() {
            return Ok(Index::default());
        }
        index::read_index(&fs::read(path)?)
    }

    fn write_index_file(&self, index: &Index) -> anyhow::Result<()> {
        atomic_write(&self.git_dir.join("index"), &index::write_index(index))
    }

    fn head_files(&self) -> anyhow::Result<FileMap> {
        self.files_of_commit(self.head_oid()?)
    }

    fn files_of_commit(&self, oid: Option<Oid>) -> anyhow::Result<FileMap> {
        let Some(oid) = oid.filter(|oid| !oid.is_zero()) else {
            return Ok(FileMap::new());
        };
        let commit = self.read_commit(oid)?;
        self.tree_files(commit.tree)
    }

    fn walk_tree(&self, tree: Oid, prefix: &str, out: &mut FileMap) -> anyhow::Result<()> {
        let Object::Tree(entries) = self.read_object(tree)? else {
            bail!("{tree} 不是树");
        };
        for entry in entries {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            if entry.mode == MODE_TREE {
                self.walk_tree(entry.oid, &path, out)?;
            } else {
                out.insert(path, (entry.mode, entry.oid));
            }
        }
        Ok(())
    }

    fn write_tree(&self, files: &FileMap) -> anyhow::Result<Oid> {
        fn insert(node: &mut TreeBuild, path: &str, mode: u32, oid: Oid) {
            let (name, rest) = match path.split_once('/') {
                Some((name, rest)) => (name.to_string(), Some(rest)),
                None => (path.to_string(), None),
            };
            if let Some(rest) = rest {
                let child = node
                    .dirs
                    .entry(name)
                    .or_insert_with(|| Box::new(TreeBuild::default()));
                insert(child, rest, mode, oid);
            } else {
                node.files.push((name, mode, oid));
            }
        }
        fn finish(repo: &Repository, node: TreeBuild) -> anyhow::Result<Oid> {
            let mut entries = Vec::new();
            for (name, child) in node.dirs {
                let oid = finish(repo, *child)?;
                entries.push(TreeEntry {
                    mode: MODE_TREE,
                    name,
                    oid,
                });
            }
            for (name, mode, oid) in node.files {
                entries.push(TreeEntry { mode, name, oid });
            }
            let payload = super::object::encode_tree(&entries);
            repo.write_object("tree", &payload)
        }
        let mut root = TreeBuild::default();
        for (path, (mode, oid)) in files {
            insert(&mut root, path, *mode, *oid);
        }
        finish(self, root)
    }

    fn tree_child(&self, tree: Oid, name: &str, want_dir: bool) -> anyhow::Result<Oid> {
        let Object::Tree(entries) = self.read_object(tree)? else {
            bail!("不是目录");
        };
        let entry = entries
            .into_iter()
            .find(|entry| entry.name == name)
            .context("路径不存在")?;
        if want_dir && entry.mode != MODE_TREE {
            bail!("{name} 不是目录");
        }
        Ok(entry.oid)
    }

    fn diffs_between(&self, old: &FileMap, new: &FileMap) -> anyhow::Result<Vec<FileDiff>> {
        let mut diffs = Vec::new();
        for change in path_changes(old, new) {
            let old_bytes = match old.get(&change.path) {
                Some((_, oid)) => Some(self.read_blob(*oid)?),
                None => None,
            };
            let new_bytes = match new.get(&change.path) {
                Some((_, oid)) => Some(self.read_blob(*oid)?),
                None => None,
            };
            diffs.push(diff_bytes(
                &change.path,
                old_bytes.as_deref(),
                new_bytes.as_deref(),
            ));
        }
        Ok(diffs)
    }

    fn scan_files(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let ignore = self.load_ignore()?;
        let mut out = Vec::new();
        if prefix.is_empty() {
            let root = self.work_tree_required()?.to_path_buf();
            self.walk_dir(&root, "", &ignore, &mut out)?;
        } else {
            let root = self.work_path(prefix)?;
            if root.is_file()
                || root
                    .symlink_metadata()
                    .map(|meta| meta.file_type().is_symlink())
                    .unwrap_or(false)
            {
                out.push(prefix.to_string());
            } else if root.is_dir() {
                self.walk_dir(&root, prefix, &ignore, &mut out)?;
            }
        }
        out.sort();
        Ok(out)
    }

    fn walk_dir(
        &self,
        dir: &Path,
        prefix: &str,
        ignore: &Ignore,
        out: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name == ".git" {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                if !ignore.ignored(&rel, false) {
                    out.push(rel);
                }
                continue;
            }
            if file_type.is_dir() {
                if ignore.ignored(&rel, true) {
                    continue;
                }
                self.walk_dir(&entry.path(), &rel, ignore, out)?;
                continue;
            }
            if file_type.is_file() && !ignore.ignored(&rel, false) {
                out.push(rel);
            }
        }
        Ok(())
    }

    fn load_ignore(&self) -> anyhow::Result<Ignore> {
        let Some(work) = &self.work_tree else {
            return Ok(Ignore::default());
        };
        let path = work.join(".gitignore");
        if path.is_file() {
            Ok(Ignore::parse(&fs::read_to_string(path)?))
        } else {
            Ok(Ignore::default())
        }
    }

    fn resolve_spec(&self, spec: &str) -> anyhow::Result<String> {
        let work = self.canonical_work()?;
        let raw = Path::new(spec);
        let from_cwd = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            std::env::current_dir()?.join(raw)
        };
        if let Some(rel) = inside(&work, &normalize_path(&from_cwd)) {
            return Ok(rel);
        }
        if let Some(found) = fs::canonicalize(&from_cwd)
            .ok()
            .and_then(|path| inside(&work, &path))
        {
            return Ok(found);
        }
        let from_work = work.join(raw);
        if let Some(rel) = inside(&work, &normalize_path(&from_work)) {
            return Ok(rel);
        }
        bail!("路径不在仓库内: {spec}")
    }

    fn canonical_work(&self) -> anyhow::Result<PathBuf> {
        let work = self.work_tree_required()?;
        Ok(fs::canonicalize(work).unwrap_or_else(|_| work.to_path_buf()))
    }

    fn work_tree_required(&self) -> anyhow::Result<&Path> {
        self.work_tree.as_deref().context("裸仓库没有工作区")
    }

    fn work_path(&self, rel: &str) -> anyhow::Result<PathBuf> {
        let work = self.work_tree_required()?;
        if rel.is_empty() || rel == "." {
            return Ok(work.to_path_buf());
        }
        if rel
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        {
            bail!("非法路径: {rel}");
        }
        let mut path = work.to_path_buf();
        for part in rel.split('/') {
            path.push(part);
        }
        Ok(path)
    }

    /// 暂存区相对某个提交改了哪些路径。合并进行中时，用 MERGE_HEAD 求出的就是本人在对方之上的改动。
    pub fn staged_changes_against(&self, oid: Oid) -> anyhow::Result<Vec<String>> {
        let base = self.files_of_commit(Some(oid))?;
        let staged = index_map(&self.read_index()?);
        Ok(path_changes(&base, &staged)
            .into_iter()
            .map(|change| change.path)
            .collect())
    }

    pub fn merge_head(&self) -> anyhow::Result<Option<Oid>> {
        for name in ["MERGE_HEAD", "ait/MERGE_HEAD"] {
            let path = self.git_dir.join(name);
            if path.is_file() {
                let text = fs::read_to_string(path)?;
                return Ok(Some(Oid::parse(text.trim())?));
            }
        }
        Ok(None)
    }

    fn clear_merge(&self) -> anyhow::Result<()> {
        for name in [
            "MERGE_HEAD",
            "MERGE_MSG",
            "ait/MERGE_HEAD",
            "ait/MERGE_MSG",
            "ait/MERGE_CONFLICTS",
        ] {
            let path = self.git_dir.join(name);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    fn find_conflict_markers(&self) -> anyhow::Result<Vec<String>> {
        let mut found = Vec::new();
        for path in self.scan_files("")? {
            let bytes = self.read_work_bytes(&path).unwrap_or_default();
            if bytes.len() > 2_000_000 {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            let start = text.lines().any(|line| line.starts_with("<<<<<<< "));
            let mid = text.lines().any(|line| line == "=======");
            let end = text.lines().any(|line| line.starts_with(">>>>>>> "));
            if start && mid && end {
                found.push(path);
            }
        }
        Ok(found)
    }

    fn mark_reachable(&self, start: Oid, out: &mut HashSet<Oid>) -> anyhow::Result<()> {
        let mut stack = vec![start];
        while let Some(oid) = stack.pop() {
            if oid.is_zero() || !out.insert(oid) {
                continue;
            }
            match self.read_object(oid) {
                Ok(Object::Commit(commit)) => {
                    stack.push(commit.tree);
                    stack.extend(commit.parents);
                }
                Ok(Object::Tree(entries)) => {
                    for entry in entries {
                        if entry.mode != 0o160000 {
                            stack.push(entry.oid);
                        }
                    }
                }
                Ok(Object::Tag(payload)) => {
                    if let Ok(tag) = super::object::TagObject::parse(&payload) {
                        stack.push(tag.object);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn match_prefix(&self, prefix: &str) -> anyhow::Result<Vec<Oid>> {
        let mut found = Vec::new();
        let objects = self.git_dir.join("objects");
        if objects.is_dir() {
            for dir in fs::read_dir(&objects)? {
                let dir = dir?;
                if !dir.path().is_dir() {
                    continue;
                }
                let Some(fan) = dir.file_name().to_str().map(|s| s.to_string()) else {
                    continue;
                };
                if fan.len() != 2 || fan == "pack" || fan == "info" {
                    continue;
                }
                for file in fs::read_dir(dir.path())? {
                    let file = file?;
                    let Some(rest) = file.file_name().to_str().map(|s| s.to_string()) else {
                        continue;
                    };
                    let hex = format!("{fan}{rest}");
                    if hex.starts_with(prefix) {
                        if let Ok(oid) = Oid::parse(&hex) {
                            found.push(oid);
                        }
                    }
                }
            }
        }
        for oid in self.with_packs(|packs| packs.oids())? {
            if oid.to_hex().starts_with(prefix) {
                found.push(oid);
            }
        }
        found.sort();
        found.dedup();
        Ok(found)
    }

    fn packed_refs(&self) -> anyhow::Result<Vec<(String, Oid)>> {
        let path = self.git_dir.join("packed-refs");
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for line in fs::read_to_string(path)?.lines() {
            if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(oid) = parts.next() else { continue };
            let Some(name) = parts.next() else { continue };
            if let Ok(oid) = Oid::parse(oid) {
                out.push((name.to_string(), oid));
            }
        }
        Ok(out)
    }

    fn rewrite_packed_without(&self, name: &str) -> anyhow::Result<()> {
        let path = self.git_dir.join("packed-refs");
        if !path.is_file() {
            return Ok(());
        }
        let mut lines = Vec::new();
        for line in fs::read_to_string(&path)?.lines() {
            let mut parts = line.split_whitespace();
            let oid = parts.next();
            let refname = parts.next();
            if refname == Some(name) || line.starts_with('^') && lines.last().is_none() {
                continue;
            }
            if refname == Some(name) {
                continue;
            }
            let _ = oid;
            lines.push(line.to_string());
        }
        // Drop peeled lines that belonged to a removed ref: a leading ^ after removal is enough
        // to ignore. Rewrite the kept lines.
        let mut kept = Vec::new();
        let text = fs::read_to_string(&path)?;
        let raw: Vec<&str> = text.lines().collect();
        let mut index = 0;
        while index < raw.len() {
            let line = raw[index];
            let refname = line.split_whitespace().nth(1);
            if refname == Some(name) {
                index += 1;
                if index < raw.len() && raw[index].starts_with('^') {
                    index += 1;
                }
                continue;
            }
            kept.push(line);
            index += 1;
        }
        let _ = lines;
        atomic_write(&path, format!("{}\n", kept.join("\n")).as_bytes())
    }
}

#[derive(Default)]
struct TreeBuild {
    dirs: BTreeMap<String, Box<TreeBuild>>,
    files: Vec<(String, u32, Oid)>,
}

fn index_map(index: &Index) -> FileMap {
    index
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), (entry.mode, entry.oid)))
        .collect()
}

fn path_changes(old: &FileMap, new: &FileMap) -> Vec<PathChange> {
    let mut names = BTreeSet::new();
    names.extend(old.keys().cloned());
    names.extend(new.keys().cloned());
    let mut out = Vec::new();
    for path in names {
        match (old.get(&path), new.get(&path)) {
            (None, Some(_)) => out.push(PathChange {
                path,
                kind: ChangeKind::Added,
            }),
            (Some(_), None) => out.push(PathChange {
                path,
                kind: ChangeKind::Deleted,
            }),
            (Some(left), Some(right)) if left != right => out.push(PathChange {
                path,
                kind: ChangeKind::Modified,
            }),
            _ => {}
        }
    }
    out
}

fn read_disk(path: &Path, mode: u32) -> anyhow::Result<Vec<u8>> {
    if mode == MODE_LINK {
        #[cfg(unix)]
        {
            let target = fs::read_link(path)?;
            return Ok(target.to_string_lossy().into_owned().into_bytes());
        }
    }
    Ok(fs::read(path)?)
}

enum RefBody {
    Direct(Oid),
    Symbolic(String),
}

fn read_ref_file(path: &Path) -> anyhow::Result<Option<RefBody>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    let text = text.trim();
    if let Some(target) = text.strip_prefix("ref: ") {
        return Ok(Some(RefBody::Symbolic(target.trim().to_string())));
    }
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(RefBody::Direct(Oid::parse(text)?)))
}

fn walk_ref_dir(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Oid>) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.ends_with(".lock") {
            continue;
        }
        let next = format!("{prefix}/{name}");
        let path = entry.path();
        if path.is_dir() {
            walk_ref_dir(&path, &next, out)?;
        } else if let Some(RefBody::Direct(oid)) = read_ref_file(&path)? {
            out.insert(next, oid);
        }
    }
    Ok(())
}

fn valid_ref(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..")
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("//")
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-'))
}

pub fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name != "HEAD"
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains('@')
        && !name.contains('\\')
        && !name.contains(' ')
        && !name.contains("//")
        && name.chars().all(|ch| ch.is_ascii_graphic())
}

fn initial_config(bare: bool) -> String {
    let filemode = if cfg!(unix) { "true" } else { "false" };
    let bare = if bare { "true" } else { "false" };
    let mut text = format!(
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = {filemode}\n\tbare = {bare}\n\tlogallrefupdates = true\n\tautocrlf = false\n"
    );
    if cfg!(target_os = "macos") {
        text.push_str("\tignorecase = true\n\tprecomposeunicode = true\n");
    }
    text
}

fn config_lookup(text: &str, key: &str) -> Option<String> {
    let (section, subsection, name) = split_config_key(key)?;
    let mut current_section = String::new();
    let mut current_sub: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(header) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            if let Some((sec, sub)) = header.split_once(' ') {
                current_section = sec.trim().to_string();
                current_sub = Some(sub.trim().trim_matches('"').to_string());
            } else {
                current_section = header.trim().to_string();
                current_sub = None;
            }
            continue;
        }
        if current_section != section || current_sub != subsection {
            continue;
        }
        let Some((left, right)) = trimmed.split_once('=') else {
            continue;
        };
        if left.trim() == name {
            return Some(right.trim().to_string());
        }
    }
    None
}

fn config_upsert(text: &str, key: &str, value: &str) -> String {
    let Some((section, subsection, name)) = split_config_key(key) else {
        return text.to_string();
    };
    let mut lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
    let mut in_section = false;
    let mut saw_section = false;
    let mut insert_at = None;
    let mut replaced = false;
    for index in 0..lines.len() {
        let trimmed = lines[index].trim().to_string();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_section && insert_at.is_none() {
                insert_at = Some(index);
            }
            in_section = header_matches(&trimmed, &section, subsection.as_deref());
            if in_section {
                saw_section = true;
            }
            continue;
        }
        if in_section {
            if let Some((left, _)) = trimmed.split_once('=') {
                if left.trim() == name {
                    lines[index] = format!("\t{name} = {value}");
                    replaced = true;
                }
            }
        }
    }
    if !replaced {
        if saw_section {
            let at = insert_at.unwrap_or(lines.len());
            lines.insert(at, format!("\t{name} = {value}"));
        } else {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            match &subsection {
                Some(sub) => lines.push(format!("[{section} \"{sub}\"]")),
                None => lines.push(format!("[{section}]")),
            }
            lines.push(format!("\t{name} = {value}"));
        }
    }
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn header_matches(header_line: &str, section: &str, subsection: Option<&str>) -> bool {
    let header = header_line
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    match subsection {
        None => header == section,
        Some(sub) => {
            let Some((sec, rest)) = header.split_once(' ') else {
                return false;
            };
            sec.trim() == section && rest.trim().trim_matches('"') == sub
        }
    }
}

fn split_config_key(key: &str) -> Option<(String, Option<String>, String)> {
    let parts: Vec<&str> = key.split('.').collect();
    match parts.as_slice() {
        [section, name] => Some(((*section).to_string(), None, (*name).to_string())),
        [section, sub, name] => Some((
            (*section).to_string(),
            Some((*sub).to_string()),
            (*name).to_string(),
        )),
        _ => None,
    }
}

fn atomic_write(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("tmp");
    let tmp = path.with_file_name(format!("{name}.tmp"));
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn normalize_path(path: &Path) -> PathBuf {
    let combined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in combined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn inside(work: &Path, full: &Path) -> Option<String> {
    let rel = full.strip_prefix(work).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str()?;
                if part == ".git" {
                    return None;
                }
                parts.push(part.to_string());
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo() -> (PathBuf, Repository) {
        let mut path = std::env::temp_dir();
        path.push(format!("ait-repo-{}", crate::identity::random_token()));
        let repo = Repository::init(&path, false).unwrap();
        (path, repo)
    }

    #[test]
    fn commits_a_blob_git_can_understand() {
        let (dir, repo) = temp_repo();
        fs::write(dir.join("hello.txt"), "hello\n").unwrap();
        repo.add(&["hello.txt".into()]).unwrap();
        let author = Signature {
            name: "A".into(),
            email: "a@b.c".into(),
            time: 1_700_000_000,
            tz: "+0000".into(),
        };
        let oid = repo.commit("first", false, &author).unwrap();
        let commit = repo.read_commit(oid).unwrap();
        assert_eq!(commit.summary(), "first");
        let files = repo.tree_files(commit.tree).unwrap();
        assert_eq!(files["hello.txt"].1, hash_payload("blob", b"hello\n").0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn log_lists_children_before_parents_within_the_same_second() {
        let (dir, repo) = temp_repo();
        let author = Signature {
            name: "A".into(),
            email: "a@b.c".into(),
            time: 1_700_000_000,
            tz: "+0000".into(),
        };
        let mut oids = Vec::new();
        for index in 0..4 {
            fs::write(dir.join("n.txt"), format!("{index}\n")).unwrap();
            repo.add(&["n.txt".into()]).unwrap();
            oids.push(repo.commit(&format!("c{index}"), false, &author).unwrap());
        }
        let head = *oids.last().unwrap();
        let listed: Vec<Oid> = repo.log(head, 10).unwrap().iter().map(|view| view.oid).collect();
        oids.reverse();
        assert_eq!(listed, oids);
        assert_eq!(repo.log(head, 1).unwrap()[0].oid, head);
        let _ = fs::remove_dir_all(dir);
    }
}
