use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context};

use super::diff::{diff_bytes, ChangeKind, FileDiff, PathChange};
use super::ignore::Ignore;
use super::index::{self, Index, IndexEntry};
use super::merge::{merge_maps, FileMap, MergeAction};
use super::object::{
    hash_payload, Commit, Object, Signature, TreeEntry, MODE_EXEC, MODE_LINK, MODE_TREE,
};
use super::odb;
use super::oid::Oid;
use super::pack::PackCache;
use std::cell::RefCell;

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

    pub fn rev_parse(&self, name: &str) -> anyhow::Result<Oid> {
        let name = name.trim();
        if name.is_empty() {
            bail!("缺少版本参数");
        }
        if name == "HEAD" {
            return self.head_oid()?.context("当前还没有提交");
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
        if !name.starts_with("refs/") || !valid_ref(name) {
            bail!("非法引用名 {name}");
        }
        atomic_write(&self.git_dir.join(name), format!("{oid}\n").as_bytes())
    }

    pub fn delete_ref(&self, name: &str) -> anyhow::Result<()> {
        if !valid_ref(name) {
            bail!("非法引用名 {name}");
        }
        let path = self.git_dir.join(name);
        if path.exists() {
            fs::remove_file(path)?;
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
        self.set_ref(&refname, oid)
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
        {
            let mut slot = self.packs.borrow_mut();
            if slot.is_none() {
                *slot = Some(PackCache::load(&self.git_dir)?);
            }
        }
        let slot = self.packs.borrow();
        if let Some(found) = slot.as_ref().and_then(|packs| packs.get(oid)) {
            return found;
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
        let mut commits = Vec::new();
        let mut stack = vec![start];
        let mut seen = HashSet::new();
        let cap = limit.saturating_mul(30).max(limit).min(5000);
        while let Some(oid) = stack.pop() {
            if !seen.insert(oid) {
                continue;
            }
            let commit = match self.read_object(oid) {
                Ok(Object::Commit(commit)) => commit,
                _ => continue,
            };
            stack.extend(commit.parents.iter().copied());
            commits.push((oid, commit));
            if commits.len() >= cap {
                break;
            }
        }
        commits.sort_by(|a, b| {
            b.1.committer
                .time
                .cmp(&a.1.committer.time)
                .then(b.0.cmp(&a.0))
        });
        commits.truncate(limit);
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
        let mut queue = VecDeque::from([right]);
        let mut seen = HashSet::new();
        while let Some(oid) = queue.pop_front() {
            if !seen.insert(oid) {
                continue;
            }
            if ancestors.contains(&oid) {
                return Ok(Some(oid));
            }
            if let Ok(Object::Commit(commit)) = self.read_object(oid) {
                queue.extend(commit.parents);
            }
        }
        Ok(None)
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
        let base_files = match self.merge_base(ours, theirs)? {
            Some(base) => self.files_of_commit(Some(base))?,
            None => FileMap::new(),
        };
        let our_files = self.files_of_commit(Some(ours))?;
        let their_files = self.files_of_commit(Some(theirs))?;
        let actions = merge_maps(&base_files, &our_files, &their_files);
        let mut files = FileMap::new();
        let mut conflicts = Vec::new();
        for (path, action) in actions {
            match action {
                MergeAction::Take(mode, oid) => {
                    files.insert(path, (mode, oid));
                }
                MergeAction::Delete => {}
                MergeAction::Conflict { .. } => conflicts.push(path),
            }
        }
        if !conflicts.is_empty() {
            conflicts.sort();
            return Ok(MergeOutcome::Conflicts(conflicts));
        }
        let tree = self.write_tree(&files)?;
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
        for entry in &index.entries {
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
        self.write_index_file(&index)
    }

    pub fn commit(&self, message: &str, all: bool, author: &Signature) -> anyhow::Result<Oid> {
        let message = message.trim();
        if message.is_empty() {
            bail!("提交说明不能为空");
        }
        if self.merge_head()?.is_some() {
            let marked = self.find_conflict_markers()?;
            if !marked.is_empty() {
                bail!("仍有冲突标记: {}", marked.join(", "));
            }
        }
        if all {
            self.stage_tracked()?;
        }
        let index = self.read_index()?;
        if index.entries.is_empty() && self.head_oid()?.is_none() {
            bail!("没有需要提交的内容");
        }
        let tree = self.write_tree(&index_map(&index))?;
        let mut parents = Vec::new();
        if let Some(head) = self.head_oid()? {
            let current = self.read_commit(head)?;
            if current.tree == tree && self.merge_head()?.is_none() {
                bail!("没有需要提交的内容");
            }
            parents.push(head);
            if let Some(extra) = self.merge_head()? {
                if extra != head {
                    parents.push(extra);
                }
            }
        }
        let mut text = message.to_string();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let commit = Commit {
            tree,
            parents,
            author: author.clone(),
            committer: author.clone(),
            message: text,
        };
        let oid = self.write_object("commit", &commit.encode())?;
        match self.head()? {
            Head::Symbolic(name) => self.set_ref(&name, oid)?,
            Head::Detached(_) => self.set_head_detached(oid)?,
        }
        self.clear_merge()?;
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
        if create {
            if !valid_branch(target) {
                bail!("非法分支名: {target}");
            }
            let oid = self.head_oid()?.context("还没有提交，不能创建分支")?;
            self.create_branch(target, oid)?;
            self.set_head_symbolic(&format!("refs/heads/{target}"))?;
            return Ok(format!("已创建并切换到分支 {target}"));
        }
        let branch_ref = format!("refs/heads/{target}");
        if valid_branch(target) {
            if let Some(oid) = self.resolve_ref(&branch_ref)? {
                self.switch_to(oid)?;
                self.set_head_symbolic(&branch_ref)?;
                return Ok(format!("已切换到分支 {target}"));
            }
        }
        let oid = self.rev_parse(target)?;
        self.switch_to(oid)?;
        self.set_head_detached(oid)?;
        Ok(format!("已分离 HEAD 到 {}", oid.short()))
    }

    pub fn merge_oid(
        &self,
        theirs: Oid,
        label: &str,
        author: &Signature,
    ) -> anyhow::Result<MergeOutcome> {
        let Some(ours) = self.head_oid()? else {
            let commit = self.read_commit(theirs)?;
            self.checkout_files(&self.tree_files(commit.tree)?)?;
            match self.head()? {
                Head::Symbolic(name) => self.set_ref(&name, theirs)?,
                Head::Detached(_) => self.set_head_detached(theirs)?,
            }
            return Ok(MergeOutcome::FastForward(theirs));
        };
        if ours == theirs || self.is_ancestor(theirs, ours)? {
            return Ok(MergeOutcome::UpToDate);
        }
        if self.is_ancestor(ours, theirs)? {
            self.switch_to(theirs)?;
            match self.head()? {
                Head::Symbolic(name) => self.set_ref(&name, theirs)?,
                Head::Detached(_) => self.set_head_detached(theirs)?,
            }
            return Ok(MergeOutcome::FastForward(theirs));
        }
        self.require_clean()?;
        let base_files = match self.merge_base(ours, theirs)? {
            Some(base) => self.tree_files(self.read_commit(base)?.tree)?,
            None => FileMap::new(),
        };
        let our_files = self.tree_files(self.read_commit(ours)?.tree)?;
        let their_files = self.tree_files(self.read_commit(theirs)?.tree)?;
        let actions = merge_maps(&base_files, &our_files, &their_files);
        for (path, action) in &actions {
            if let MergeAction::Take(mode, oid) = action {
                if !our_files.contains_key(path) {
                    let disk = self.work_path(path)?;
                    if fs::symlink_metadata(&disk).is_ok() {
                        let (have_mode, have_oid) = self.hash_work(path)?;
                        if have_mode != *mode || have_oid != *oid {
                            bail!("未跟踪文件会被覆盖: {path}");
                        }
                    }
                }
            }
        }
        let mut conflicts = Vec::new();
        let mut result = FileMap::new();
        for (path, action) in &actions {
            match action {
                MergeAction::Take(mode, oid) => {
                    self.materialize(path, *mode, *oid)?;
                    result.insert(path.clone(), (*mode, *oid));
                }
                MergeAction::Delete => {
                    let disk = self.work_path(path)?;
                    if fs::symlink_metadata(&disk).is_ok() {
                        let _ = fs::remove_file(disk);
                    }
                }
                MergeAction::Conflict {
                    ours: our_side,
                    theirs: their_side,
                } => {
                    conflicts.push(path.clone());
                    let bytes = self.conflict_blob(label, *our_side, *their_side)?;
                    let disk = self.work_path(path)?;
                    if let Some(parent) = disk.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    if fs::symlink_metadata(&disk).is_ok() {
                        let _ = fs::remove_file(&disk);
                    }
                    fs::write(disk, bytes)?;
                }
            }
        }
        for path in our_files.keys() {
            if !result.contains_key(path) && !conflicts.iter().any(|item| item == path) {
                let disk = self.work_path(path)?;
                if fs::symlink_metadata(&disk).is_ok() {
                    let _ = fs::remove_file(disk);
                }
            }
        }
        self.write_index_from_map(&result)?;
        if conflicts.is_empty() {
            let tree = self.write_tree(&result)?;
            let commit = Commit {
                tree,
                parents: vec![ours, theirs],
                author: author.clone(),
                committer: author.clone(),
                message: format!("Merge {label}\n"),
            };
            let oid = self.write_object("commit", &commit.encode())?;
            match self.head()? {
                Head::Symbolic(name) => self.set_ref(&name, oid)?,
                Head::Detached(_) => self.set_head_detached(oid)?,
            }
            Ok(MergeOutcome::Merged(oid))
        } else {
            fs::create_dir_all(self.git_dir.join("ait"))?;
            fs::write(self.git_dir.join("ait/MERGE_HEAD"), format!("{theirs}\n"))?;
            fs::write(
                self.git_dir.join("ait/MERGE_MSG"),
                format!("Merge {label}\n"),
            )?;
            Ok(MergeOutcome::Conflicts(conflicts))
        }
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
                if fs::symlink_metadata(self.work_path(&path)?).is_err() {
                    index.remove(&path);
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
        index.upsert(IndexEntry {
            path: rel.to_string(),
            mode,
            oid,
            stat,
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

    fn merge_head(&self) -> anyhow::Result<Option<Oid>> {
        let path = self.git_dir.join("ait/MERGE_HEAD");
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(path)?;
        Ok(Some(Oid::parse(text.trim())?))
    }

    fn clear_merge(&self) -> anyhow::Result<()> {
        for name in ["ait/MERGE_HEAD", "ait/MERGE_MSG"] {
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

    fn conflict_blob(
        &self,
        label: &str,
        ours: Option<(u32, Oid)>,
        theirs: Option<(u32, Oid)>,
    ) -> anyhow::Result<Vec<u8>> {
        let mut out = b"<<<<<<< HEAD\n".to_vec();
        if let Some((_, oid)) = ours {
            push_with_newline(&mut out, &self.read_blob(oid)?);
        }
        out.extend_from_slice(b"=======\n");
        if let Some((_, oid)) = theirs {
            push_with_newline(&mut out, &self.read_blob(oid)?);
        }
        out.extend_from_slice(format!(">>>>>>> {label}\n").as_bytes());
        Ok(out)
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
                        stack.push(entry.oid);
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
        {
            let mut slot = self.packs.borrow_mut();
            if slot.is_none() {
                *slot = Some(PackCache::load(&self.git_dir)?);
            }
            if let Some(packs) = slot.as_ref() {
                for oid in packs.oids() {
                    if oid.to_hex().starts_with(prefix) {
                        found.push(oid);
                    }
                }
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

fn push_with_newline(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(bytes);
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        out.push(b'\n');
    }
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
}
