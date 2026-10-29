//! 撤销与改写历史：reset、restore、rm/mv/clean，以及 cherry-pick、revert、rebase 共用的执行序列。

use std::collections::{BTreeSet, HashSet};
use std::fs;

use anyhow::{bail, Context};
use sha1::{Digest, Sha1};

use super::{atomic_write, index_map, Head, MergedTree, Repository};
use crate::git::ignore::Ignore;
use crate::git::index::{self, FileStat, Index, IndexEntry};
use crate::git::merge::{FileMap, TextOptions};
use crate::git::object::{Commit, Signature};
use crate::git::oid::Oid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreSource {
    Head,
    Index,
    Commit(Oid),
}

#[derive(Clone, Debug, Default)]
pub struct CleanOptions {
    pub dry_run: bool,
    pub dirs: bool,
    pub ignored: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoAction {
    Pick,
    Revert,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
    Exec,
    Break,
}

#[derive(Clone, Debug)]
pub struct Todo {
    pub action: TodoAction,
    pub oid: Option<Oid>,
    pub text: String,
}

impl Todo {
    fn line(&self) -> String {
        let word = match self.action {
            TodoAction::Pick => "pick",
            TodoAction::Revert => "revert",
            TodoAction::Reword => "reword",
            TodoAction::Edit => "edit",
            TodoAction::Squash => "squash",
            TodoAction::Fixup => "fixup",
            TodoAction::Drop => "drop",
            TodoAction::Exec => "exec",
            TodoAction::Break => "break",
        };
        match self.oid {
            Some(oid) => format!("{word} {oid} {}", self.text),
            None if self.text.is_empty() => word.to_string(),
            None => format!("{word} {}", self.text),
        }
    }
}

#[derive(Clone, Debug)]
pub enum SeqOutcome {
    Done(String),
    Stopped {
        message: String,
        conflicts: Vec<String>,
    },
}

const SEQ_DIR: &str = "ait/sequencer";

impl Repository {
    // ---------- reset / restore ----------

    pub fn reset(&self, target: Oid, mode: ResetMode, label: &str) -> anyhow::Result<()> {
        self.work_tree_required()?;
        let old = self.head_oid()?;
        if mode == ResetMode::Soft && self.merge_head()?.is_some() {
            bail!("合并进行中不能 --soft 重置");
        }
        let files = self.files_of_commit(Some(target))?;
        match mode {
            ResetMode::Hard => {
                let index = self.read_index()?;
                for entry in &index.entries {
                    if !files.contains_key(&entry.path) {
                        let disk = self.work_path(&entry.path)?;
                        if fs::symlink_metadata(&disk).is_ok() {
                            let _ = fs::remove_file(&disk);
                            self.prune_empty_dirs(&entry.path);
                        }
                    }
                }
                self.checkout_files(&files)?;
            }
            ResetMode::Mixed => self.reset_index_to(&files)?,
            ResetMode::Soft => {}
        }
        if let Some(old) = old {
            atomic_write(&self.git_dir.join("ORIG_HEAD"), format!("{old}\n").as_bytes())?;
        }
        self.update_head(target, &format!("reset: moving to {label}"))?;
        if mode != ResetMode::Soft {
            self.clear_merge()?;
            self.clear_pick()?;
        }
        Ok(())
    }

    /// 只重写暂存区。磁盘内容与目标一致的文件保留真实 stat，其余写空 stat 让 Git 重新比较。
    fn reset_index_to(&self, files: &FileMap) -> anyhow::Result<()> {
        let mut index = Index::default();
        for (path, (mode, oid)) in files {
            index.upsert(self.index_entry_for(path, *mode, *oid));
        }
        self.write_index_file(&index)
    }

    pub(super) fn index_entry_for(&self, path: &str, mode: u32, oid: Oid) -> IndexEntry {
        let stat = match self.hash_work(path) {
            Ok((have_mode, have_oid)) if have_mode == mode && have_oid == oid => self
                .work_path(path)
                .ok()
                .and_then(|disk| index::stat_path(&disk).ok())
                .map(|(_, stat)| stat)
                .unwrap_or_default(),
            _ => FileStat::default(),
        };
        IndexEntry {
            path: path.to_string(),
            mode,
            oid,
            stat,
            stage: 0,
        }
    }

    pub fn restore(&self, specs: &[String], worktree: bool) -> anyhow::Result<Vec<String>> {
        self.restore_from(specs, RestoreSource::Head, true, worktree)
    }

    /// 从来源（HEAD、某个提交或暂存区）还原路径。`staged` 写暂存区，`worktree` 写工作区。
    pub fn restore_from(
        &self,
        specs: &[String],
        source: RestoreSource,
        staged: bool,
        worktree: bool,
    ) -> anyhow::Result<Vec<String>> {
        self.work_tree_required()?;
        if specs.is_empty() {
            bail!("请指定要还原的路径");
        }
        if !staged && !worktree {
            bail!("至少要还原暂存区或工作区之一");
        }
        if source == RestoreSource::Index && staged {
            bail!("从暂存区还原时不能再写暂存区，请用 --source 指定提交");
        }
        let mut index = self.read_index()?;
        let src: FileMap = match source {
            RestoreSource::Head => self.head_files()?,
            RestoreSource::Commit(oid) => self.files_of_commit(Some(oid))?,
            RestoreSource::Index => index
                .entries
                .iter()
                .filter(|entry| entry.stage == 0)
                .map(|entry| (entry.path.clone(), (entry.mode, entry.oid)))
                .collect(),
        };
        let mut targets = BTreeSet::new();
        for spec in specs {
            let rel = self.resolve_spec(spec)?;
            let covers =
                |path: &str| rel.is_empty() || path == rel || path.starts_with(&format!("{rel}/"));
            targets.extend(src.keys().filter(|path| covers(path)).cloned());
            targets.extend(
                index
                    .entries
                    .iter()
                    .filter(|entry| covers(&entry.path))
                    .map(|entry| entry.path.clone()),
            );
        }
        if targets.is_empty() {
            bail!("这些路径没有可还原的内容: {}", specs.join(", "));
        }
        if source == RestoreSource::Index {
            let unmerged: Vec<&String> = targets
                .iter()
                .filter(|path| index.entries.iter().any(|e| &e.path == *path && e.stage > 0))
                .collect();
            if !unmerged.is_empty() {
                bail!(
                    "这些路径还有冲突，请先解决: {}",
                    unmerged.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                );
            }
        }
        for path in &targets {
            let entry = src.get(path).copied();
            if worktree {
                match entry {
                    Some((mode, oid)) => self.materialize(path, mode, oid)?,
                    None => {
                        let disk = self.work_path(path)?;
                        if fs::symlink_metadata(&disk).is_ok() {
                            fs::remove_file(&disk)?;
                            self.prune_empty_dirs(path);
                        }
                    }
                }
            }
            if staged {
                index.remove(path);
                if let Some((mode, oid)) = entry {
                    index.upsert(self.index_entry_for(path, mode, oid));
                }
            } else if worktree {
                if let Some(existing) = index.get(path).cloned() {
                    if existing.stage == 0 {
                        index.upsert(self.index_entry_for(path, existing.mode, existing.oid));
                    }
                }
            }
        }
        self.write_index_file(&index)?;
        if staged {
            self.forget_conflicts(specs)?;
        }
        Ok(targets.into_iter().collect())
    }

    // ---------- rm / mv / clean ----------

    pub fn remove_paths(
        &self,
        specs: &[String],
        cached: bool,
        force: bool,
        recursive: bool,
    ) -> anyhow::Result<Vec<String>> {
        self.work_tree_required()?;
        if specs.is_empty() {
            bail!("请指定要删除的路径");
        }
        let mut index = self.read_index()?;
        let head = self.head_files()?;
        let mut targets = BTreeSet::new();
        for spec in specs {
            let rel = self.resolve_spec(spec)?;
            let exact: Vec<String> = index
                .entries
                .iter()
                .filter(|entry| entry.path == rel)
                .map(|entry| entry.path.clone())
                .collect();
            if !exact.is_empty() {
                targets.extend(exact);
                continue;
            }
            let nested: Vec<String> = index
                .entries
                .iter()
                .filter(|entry| rel.is_empty() || entry.path.starts_with(&format!("{rel}/")))
                .map(|entry| entry.path.clone())
                .collect();
            if nested.is_empty() {
                bail!("路径没有被跟踪: {spec}");
            }
            if !recursive {
                bail!("{spec} 是目录，请加 -r");
            }
            targets.extend(nested);
        }
        if !force {
            let mut dirty = Vec::new();
            for path in &targets {
                let Some(entry) = index.get(path) else { continue };
                let staged_differs = head.get(path) != Some(&(entry.mode, entry.oid));
                let disk = self.work_path(path)?;
                let work_differs = fs::symlink_metadata(&disk).is_ok()
                    && self.hash_work(path).ok() != Some((entry.mode, entry.oid));
                let blocked = if cached {
                    staged_differs && work_differs
                } else {
                    staged_differs || work_differs
                };
                if blocked {
                    dirty.push(path.clone());
                }
            }
            if !dirty.is_empty() {
                bail!("这些文件有未提交的修改，加 -f 强制删除: {}", dirty.join(", "));
            }
        }
        for path in &targets {
            index.remove(path);
            if !cached {
                let disk = self.work_path(path)?;
                if fs::symlink_metadata(&disk).is_ok() {
                    fs::remove_file(&disk)?;
                    self.prune_empty_dirs(path);
                }
            }
        }
        self.write_index_file(&index)?;
        Ok(targets.into_iter().collect())
    }

    pub fn move_path(&self, from: &str, to: &str, force: bool) -> anyhow::Result<(String, String)> {
        self.work_tree_required()?;
        let src = self.resolve_spec(from)?;
        let mut dst = self.resolve_spec(to)?;
        if src.is_empty() {
            bail!("不能移动仓库根目录");
        }
        let mut index = self.read_index()?;
        let tracked_file = index.get(&src).is_some();
        let tracked_dir = index
            .entries
            .iter()
            .any(|entry| entry.path.starts_with(&format!("{src}/")));
        if !tracked_file && !tracked_dir {
            bail!("路径没有被跟踪: {from}");
        }
        if self.work_path(&dst)?.is_dir() {
            let name = src.rsplit('/').next().unwrap_or(&src);
            dst = if dst.is_empty() {
                name.to_string()
            } else {
                format!("{dst}/{name}")
            };
        }
        if dst == src || dst.starts_with(&format!("{src}/")) {
            bail!("不能把 {from} 移到它自己里面");
        }
        let dst_disk = self.work_path(&dst)?;
        if fs::symlink_metadata(&dst_disk).is_ok() {
            if !force || dst_disk.is_dir() {
                bail!("目标已存在: {dst}");
            }
            fs::remove_file(&dst_disk)?;
        }
        if let Some(parent) = dst_disk.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(self.work_path(&src)?, &dst_disk)?;
        let moved: Vec<IndexEntry> = index
            .entries
            .iter()
            .filter(|entry| entry.path == src || entry.path.starts_with(&format!("{src}/")))
            .cloned()
            .collect();
        for entry in moved {
            index.remove(&entry.path);
            let path = format!("{dst}{}", &entry.path[src.len()..]);
            index.remove(&path);
            index.upsert(IndexEntry { path, ..entry });
        }
        self.write_index_file(&index)?;
        self.prune_empty_dirs(&src);
        Ok((src, dst))
    }

    /// 列出（或删除）未跟踪的文件。默认不进入完全未跟踪的目录，`dirs` 时连目录一起清理。
    pub fn clean(&self, options: &CleanOptions) -> anyhow::Result<Vec<String>> {
        let work = self.work_tree_required()?.to_path_buf();
        let index = self.read_index()?;
        let tracked: HashSet<&str> = index.entries.iter().map(|e| e.path.as_str()).collect();
        let mut tracked_dirs = HashSet::new();
        for path in &tracked {
            let mut current = *path;
            while let Some((parent, _)) = current.rsplit_once('/') {
                tracked_dirs.insert(parent.to_string());
                current = parent;
            }
        }
        let files = if options.ignored {
            let mut out = Vec::new();
            self.walk_dir(&work, "", &Ignore::default(), &mut out)?;
            out
        } else {
            self.scan_files("")?
        };
        let mut removed = Vec::new();
        let mut dirs_shown = BTreeSet::new();
        for path in files {
            if tracked.contains(path.as_str()) {
                continue;
            }
            let mut untracked_dir = None;
            let mut nested_repo = false;
            let mut prefix = String::new();
            for part in path.split('/').collect::<Vec<_>>().split_last().map(|x| x.1).unwrap_or(&[]) {
                prefix = if prefix.is_empty() {
                    part.to_string()
                } else {
                    format!("{prefix}/{part}")
                };
                if work.join(&prefix).join(".git").exists() {
                    nested_repo = true;
                    break;
                }
                if untracked_dir.is_none() && !tracked_dirs.contains(&prefix) {
                    untracked_dir = Some(prefix.clone());
                }
            }
            if nested_repo {
                continue;
            }
            if let Some(dir) = untracked_dir {
                if !options.dirs {
                    continue;
                }
                if dirs_shown.insert(dir.clone()) {
                    removed.push(format!("{dir}/"));
                    if !options.dry_run {
                        fs::remove_dir_all(work.join(&dir))?;
                    }
                }
                continue;
            }
            removed.push(path.clone());
            if !options.dry_run {
                fs::remove_file(self.work_path(&path)?)?;
            }
        }
        Ok(removed)
    }

    pub(super) fn prune_empty_dirs(&self, rel: &str) {
        let Ok(work) = self.work_tree_required() else {
            return;
        };
        let mut current = rel;
        while let Some((parent, _)) = current.rsplit_once('/') {
            if fs::remove_dir(work.join(parent)).is_err() {
                break;
            }
            current = parent;
        }
    }

    // ---------- 执行序列：cherry-pick / revert / rebase ----------

    pub(super) fn pick_head(&self) -> anyhow::Result<Option<Oid>> {
        self.read_oid_file("CHERRY_PICK_HEAD")
    }

    pub(super) fn revert_head(&self) -> anyhow::Result<Option<Oid>> {
        self.read_oid_file("REVERT_HEAD")
    }

    fn read_oid_file(&self, name: &str) -> anyhow::Result<Option<Oid>> {
        let path = self.git_dir.join(name);
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(path)?;
        Ok(Some(Oid::parse(text.trim())?))
    }

    pub(super) fn clear_pick(&self) -> anyhow::Result<()> {
        for name in ["CHERRY_PICK_HEAD", "REVERT_HEAD"] {
            let path = self.git_dir.join(name);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    fn seq_path(&self, name: &str) -> std::path::PathBuf {
        self.git_dir.join(SEQ_DIR).join(name)
    }

    fn seq_read(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.seq_path(name))
            .ok()
            .map(|text| text.trim_end().to_string())
    }

    fn seq_write(&self, name: &str, value: &str) -> anyhow::Result<()> {
        atomic_write(&self.seq_path(name), format!("{value}\n").as_bytes())
    }

    fn seq_remove(&self, name: &str) {
        let _ = fs::remove_file(self.seq_path(name));
    }

    /// 进行中的序列类型：`rebase`、`cherry-pick` 或 `revert`。
    pub fn sequence_kind(&self) -> Option<String> {
        self.seq_read("kind")
    }

    fn require_no_sequence(&self) -> anyhow::Result<()> {
        if let Some(kind) = self.sequence_kind() {
            bail!("已有进行中的 {kind}，请先 --continue、--skip 或 --abort");
        }
        if self.merge_head()?.is_some() {
            bail!("合并进行中，请先完成或 ait merge --abort");
        }
        Ok(())
    }

    pub fn cherry_pick(
        &self,
        commits: &[Oid],
        mainline: Option<usize>,
        record_origin: bool,
        no_commit: bool,
        committer: &Signature,
    ) -> anyhow::Result<SeqOutcome> {
        self.start_simple_sequence("cherry-pick", TodoAction::Pick, commits, mainline, record_origin, no_commit, committer)
    }

    pub fn revert(
        &self,
        commits: &[Oid],
        mainline: Option<usize>,
        no_commit: bool,
        committer: &Signature,
    ) -> anyhow::Result<SeqOutcome> {
        self.start_simple_sequence("revert", TodoAction::Revert, commits, mainline, false, no_commit, committer)
    }

    #[allow(clippy::too_many_arguments)]
    fn start_simple_sequence(
        &self,
        kind: &str,
        action: TodoAction,
        commits: &[Oid],
        mainline: Option<usize>,
        record_origin: bool,
        no_commit: bool,
        committer: &Signature,
    ) -> anyhow::Result<SeqOutcome> {
        self.work_tree_required()?;
        self.require_no_sequence()?;
        if commits.is_empty() {
            bail!("请指定提交");
        }
        self.require_clean()?;
        if no_commit {
            for oid in commits {
                let merged = self.apply_change(*oid, action == TodoAction::Revert, mainline)?;
                if !merged.conflicts.is_empty() {
                    return Ok(SeqOutcome::Stopped {
                        message: format!("应用 {} 时有冲突，解决后自行提交", oid.short()),
                        conflicts: merged.conflicts,
                    });
                }
            }
            return Ok(SeqOutcome::Done(format!(
                "已把 {} 个提交的改动放进暂存区，未提交",
                commits.len()
            )));
        }
        let head = self.head_oid()?.context("还没有提交")?;
        let todo: Vec<Todo> = commits
            .iter()
            .map(|oid| Todo {
                action,
                oid: Some(*oid),
                text: self.read_commit(*oid).map(|c| c.summary().to_string()).unwrap_or_default(),
            })
            .collect();
        fs::create_dir_all(self.git_dir.join(SEQ_DIR))?;
        self.seq_write("kind", kind)?;
        self.seq_write("orig-head", &head.to_hex())?;
        if let Some(m) = mainline {
            self.seq_write("mainline", &m.to_string())?;
        }
        if record_origin {
            self.seq_write("record-origin", "1")?;
        }
        self.write_todo(&todo)?;
        self.run_sequence(committer)
    }

    /// 把 `upstream..HEAD`（或 `branch`）上的提交依次重放到 `onto`。
    /// `todo` 给出时直接用作执行清单；`interactive` 时用编辑器修改清单。
    pub fn rebase(
        &self,
        upstream: Oid,
        onto: Option<Oid>,
        branch: Option<&str>,
        interactive: bool,
        todo: Option<&str>,
        committer: &Signature,
    ) -> anyhow::Result<SeqOutcome> {
        self.work_tree_required()?;
        self.require_no_sequence()?;
        if let Some(branch) = branch {
            if self.branch_name()?.as_deref() != Some(branch) {
                self.checkout(branch, false)?;
            }
        }
        self.require_clean()?;
        let head = self.head_oid()?.context("还没有提交")?;
        let onto = onto.unwrap_or(upstream);
        let head_name = match self.head()? {
            Head::Symbolic(name) => name,
            Head::Detached(_) => "detached".into(),
        };
        let mut steps = match todo {
            Some(text) => parse_todo(self, text)?,
            None => {
                let commits = self.commits_to_replay(head, upstream)?;
                commits
                    .into_iter()
                    .map(|oid| Todo {
                        action: TodoAction::Pick,
                        oid: Some(oid),
                        text: self
                            .read_commit(oid)
                            .map(|c| c.summary().to_string())
                            .unwrap_or_default(),
                    })
                    .collect()
            }
        };
        if interactive && todo.is_none() {
            let mut text = String::new();
            for step in &steps {
                text.push_str(&step.line());
                text.push('\n');
            }
            text.push_str(&format!(
                "\n# 变基 {}..{} 到 {}\n#\n# p, pick = 使用提交\n# r, reword = 使用提交，但修改说明\n# e, edit = 使用提交，然后停下来修改\n# s, squash = 并入前一个提交，合并说明\n# f, fixup = 并入前一个提交，丢弃本条说明\n# d, drop = 丢弃提交\n# x, exec = 执行命令\n# b, break = 在这里暂停\n#\n# 删掉所有行会取消变基。\n",
                upstream.short(),
                head.short(),
                onto.short()
            ));
            let edited = self.run_editor(&text, true, "git-rebase-todo")?;
            steps = parse_todo(self, &edited)?;
            if steps.is_empty() {
                return Ok(SeqOutcome::Done("执行清单为空，已取消变基".into()));
            }
        }
        check_first_step(&steps)?;
        if steps.is_empty() {
            if self.is_ancestor(head, onto)? && head != onto {
                self.switch_to(onto)?;
                self.update_head(onto, &format!("rebase (finish): {head_name} onto {onto}"))?;
                return Ok(SeqOutcome::Done(format!("已快进到 {}", onto.short())));
            }
            if self.is_ancestor(onto, head)? {
                return Ok(SeqOutcome::Done("当前分支已经是最新".into()));
            }
        }
        atomic_write(&self.git_dir.join("ORIG_HEAD"), format!("{head}\n").as_bytes())?;
        fs::create_dir_all(self.git_dir.join(SEQ_DIR))?;
        self.seq_write("kind", "rebase")?;
        self.seq_write("orig-head", &head.to_hex())?;
        self.seq_write("head-name", &head_name)?;
        self.seq_write("onto", &onto.to_hex())?;
        self.write_todo(&steps)?;
        self.switch_to(onto)?;
        self.set_head_detached(onto)?;
        self.append_reflog("HEAD", head, onto, &format!("rebase (start): checkout {}", onto.short()))?;
        self.run_sequence(committer)
    }

    pub fn sequence_continue(&self, committer: &Signature) -> anyhow::Result<SeqOutcome> {
        let kind = self.sequence_kind().context("没有进行中的 rebase、cherry-pick 或 revert")?;
        if let Some(line) = self.seq_read("current") {
            let step = parse_todo(self, &line)?
                .into_iter()
                .next()
                .context("当前步骤损坏")?;
            let marked = self.find_conflict_markers()?;
            if !marked.is_empty() {
                bail!("仍有冲突标记: {}", marked.join(", "));
            }
            let pending = self.read_conflict_list()?;
            if !pending.is_empty() {
                bail!("仍有未解决的冲突，修改后 ait add: {}", pending.join(", "));
            }
            let index = self.read_index()?;
            if index.entries.iter().any(|e| e.stage > 0) {
                bail!("暂存区仍有未合并的路径，请先 ait add");
            }
            let head = self.head_oid()?.context("没有当前提交")?;
            let tree = self.write_tree(&index_map(&index))?;
            let head_tree = self.read_commit(head)?.tree;
            let message = fs::read_to_string(self.git_dir.join("MERGE_MSG")).unwrap_or_default();
            if tree != head_tree {
                let amend_target = self.seq_read("amend");
                match step.action {
                    TodoAction::Squash | TodoAction::Fixup => {
                        self.squash_into_head(tree, &step, committer)?;
                    }
                    _ if amend_target.as_deref() == Some(head.to_hex().as_str()) => {
                        let old = self.read_commit(head)?;
                        let oid = self.write_commit(tree, old.parents.clone(), old.author.clone(), committer.clone(), &old.message)?;
                        self.update_head(oid, &format!("{kind} (amend): {}", first_line(&old.message)))?;
                    }
                    _ => {
                        let author = match (step.action, step.oid) {
                            (TodoAction::Revert, _) | (_, None) => committer.clone(),
                            (_, Some(oid)) => self.read_commit(oid)?.author,
                        };
                        let message = if message.trim().is_empty() {
                            step.oid
                                .map(|oid| self.read_commit(oid).map(|c| c.message))
                                .transpose()?
                                .unwrap_or_default()
                        } else {
                            message
                        };
                        let oid = self.write_commit(tree, vec![head], author, committer.clone(), &message)?;
                        self.update_head(oid, &format!("{kind}: {}", first_line(&message)))?;
                    }
                }
            } else if kind != "rebase" && self.seq_read("amend").is_none() {
                bail!("这一步的改动为空，可用 --skip 跳过");
            }
            self.finish_step()?;
        }
        self.run_sequence(committer)
    }

    pub fn sequence_skip(&self, committer: &Signature) -> anyhow::Result<SeqOutcome> {
        self.sequence_kind().context("没有进行中的 rebase、cherry-pick 或 revert")?;
        let head = self.head_oid()?.context("没有当前提交")?;
        self.discard_to(head)?;
        self.finish_step()?;
        self.run_sequence(committer)
    }

    pub fn sequence_abort(&self) -> anyhow::Result<String> {
        let kind = self.sequence_kind().context("没有进行中的 rebase、cherry-pick 或 revert")?;
        let orig = Oid::parse(&self.seq_read("orig-head").context("缺少 orig-head")?)?;
        self.discard_to(orig)?;
        let head_name = self.seq_read("head-name");
        match head_name.as_deref() {
            Some(name) if name.starts_with("refs/") => {
                self.set_ref_logged(name, orig, &format!("{kind} (abort): returning to {name}"))?;
                self.set_head_symbolic(name)?;
            }
            _ => self.update_head(orig, &format!("{kind} (abort)"))?,
        }
        self.clear_sequence()?;
        Ok(format!("已取消 {kind}，回到 {}", orig.short()))
    }

    /// 丢弃工作区和暂存区，回到 `oid` 的内容，但不移动分支。
    pub(super) fn discard_to(&self, oid: Oid) -> anyhow::Result<()> {
        let files = self.files_of_commit(Some(oid))?;
        let index = self.read_index()?;
        for entry in &index.entries {
            if !files.contains_key(&entry.path) {
                let disk = self.work_path(&entry.path)?;
                if fs::symlink_metadata(&disk).is_ok() {
                    let _ = fs::remove_file(&disk);
                    self.prune_empty_dirs(&entry.path);
                }
            }
        }
        self.checkout_files(&files)?;
        self.clear_merge()?;
        self.clear_pick()
    }

    fn finish_step(&self) -> anyhow::Result<()> {
        self.seq_remove("current");
        self.seq_remove("amend");
        self.clear_merge()?;
        self.clear_pick()
    }

    fn clear_sequence(&self) -> anyhow::Result<()> {
        let dir = self.git_dir.join(SEQ_DIR);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        self.clear_merge()?;
        self.clear_pick()
    }

    fn write_todo(&self, steps: &[Todo]) -> anyhow::Result<()> {
        let mut text = String::new();
        for step in steps {
            text.push_str(&step.line());
            text.push('\n');
        }
        atomic_write(&self.seq_path("todo"), text.as_bytes())
    }

    fn read_todo(&self) -> anyhow::Result<Vec<Todo>> {
        parse_todo(self, &self.seq_read("todo").unwrap_or_default())
    }

    /// 剩余的执行清单，供 `status` 显示。
    pub fn sequence_todo(&self) -> anyhow::Result<Vec<Todo>> {
        if self.sequence_kind().is_none() {
            return Ok(Vec::new());
        }
        self.read_todo()
    }

    fn run_sequence(&self, committer: &Signature) -> anyhow::Result<SeqOutcome> {
        let kind = self.sequence_kind().context("没有进行中的序列")?;
        let mainline = self.seq_read("mainline").and_then(|m| m.parse().ok());
        let record_origin = self.seq_read("record-origin").is_some();
        let mut applied = 0usize;
        loop {
            let mut todo = self.read_todo()?;
            if todo.is_empty() {
                return self.finish_sequence(&kind, applied);
            }
            let step = todo.remove(0);
            self.write_todo(&todo)?;
            let head = self.head_oid()?.context("没有当前提交")?;
            match step.action {
                TodoAction::Drop => continue,
                TodoAction::Break => {
                    return Ok(SeqOutcome::Stopped {
                        message: format!("已在 {} 暂停，ait {kind} --continue 继续", head.short()),
                        conflicts: Vec::new(),
                    });
                }
                TodoAction::Exec => {
                    let work = self.work_tree_required()?;
                    let status = std::process::Command::new("sh")
                        .arg("-c")
                        .arg(&step.text)
                        .current_dir(work)
                        .status()
                        .with_context(|| format!("执行 {} 失败", step.text))?;
                    if !status.success() {
                        return Ok(SeqOutcome::Stopped {
                            message: format!("命令失败: {}，修好后 ait {kind} --continue", step.text),
                            conflicts: Vec::new(),
                        });
                    }
                    continue;
                }
                _ => {}
            }
            let oid = step.oid.context("执行清单缺少提交")?;
            let commit = self.read_commit(oid)?;
            let revert = step.action == TodoAction::Revert;
            let fast = kind == "rebase"
                && matches!(step.action, TodoAction::Pick | TodoAction::Reword | TodoAction::Edit)
                && commit.parents.first() == Some(&head)
                && commit.parents.len() == 1;
            if fast {
                self.switch_to(oid)?;
                self.update_head(oid, &format!("rebase (pick): {}", commit.summary()))?;
            } else {
                if commit.parents.len() > 1 && mainline.is_none() && kind != "rebase" {
                    bail!("{} 是合并提交，请用 -m 指定主线父提交", oid.short());
                }
                if commit.parents.len() > 1 && kind == "rebase" {
                    continue;
                }
                let merged = self.apply_change(oid, revert, mainline)?;
                let message = if revert {
                    format!(
                        "Revert \"{}\"\n\nThis reverts commit {oid}.\n",
                        commit.summary()
                    )
                } else if record_origin {
                    format!(
                        "{}\n\n(cherry picked from commit {oid})\n",
                        commit.message.trim_end()
                    )
                } else {
                    commit.message.clone()
                };
                if !merged.conflicts.is_empty() {
                    self.seq_write("current", &step.line())?;
                    atomic_write(&self.git_dir.join("MERGE_MSG"), message.as_bytes())?;
                    let marker = if revert { "REVERT_HEAD" } else { "CHERRY_PICK_HEAD" };
                    atomic_write(&self.git_dir.join(marker), format!("{oid}\n").as_bytes())?;
                    return Ok(SeqOutcome::Stopped {
                        message: format!(
                            "应用 {} {} 时有冲突，解决后 ait add，再 ait {kind} --continue",
                            oid.short(),
                            commit.summary()
                        ),
                        conflicts: merged.conflicts,
                    });
                }
                let tree = self.write_tree(&merged.files)?;
                let head_tree = self.read_commit(head)?.tree;
                if matches!(step.action, TodoAction::Squash | TodoAction::Fixup) {
                    self.squash_into_head(tree, &step, committer)?;
                    applied += 1;
                    continue;
                }
                if tree == head_tree {
                    if kind == "rebase" {
                        continue;
                    }
                    self.seq_write("current", &step.line())?;
                    return Ok(SeqOutcome::Stopped {
                        message: format!(
                            "{} 的改动已经存在，结果为空；ait {kind} --skip 跳过",
                            oid.short()
                        ),
                        conflicts: Vec::new(),
                    });
                }
                let author = if revert { committer.clone() } else { commit.author.clone() };
                let new = self.write_commit(tree, vec![head], author, committer.clone(), &message)?;
                let label = if kind == "rebase" {
                    format!("rebase (pick): {}", first_line(&message))
                } else {
                    format!("{kind}: {}", first_line(&message))
                };
                self.update_head(new, &label)?;
            }
            applied += 1;
            match step.action {
                TodoAction::Reword => {
                    let current = self.head_oid()?.context("没有当前提交")?;
                    let old = self.read_commit(current)?;
                    match self.run_editor(&old.message, false, "COMMIT_EDITMSG") {
                        Ok(text) if !text.trim().is_empty() => {
                            let oid = self.write_commit(old.tree, old.parents.clone(), old.author.clone(), committer.clone(), &text)?;
                            self.update_head(oid, &format!("rebase (reword): {}", first_line(&text)))?;
                        }
                        _ => {
                            self.seq_write("current", &step.line())?;
                            self.seq_write("amend", &current.to_hex())?;
                            return Ok(SeqOutcome::Stopped {
                                message: format!(
                                    "停在 {}，用 ait commit --amend -m \"新说明\" 修改说明后 ait rebase --continue",
                                    current.short()
                                ),
                                conflicts: Vec::new(),
                            });
                        }
                    }
                }
                TodoAction::Edit => {
                    let current = self.head_oid()?.context("没有当前提交")?;
                    self.seq_write("current", &step.line())?;
                    self.seq_write("amend", &current.to_hex())?;
                    return Ok(SeqOutcome::Stopped {
                        message: format!(
                            "停在 {} 供修改：改好后 ait add，再 ait rebase --continue（或先 ait commit --amend）",
                            current.short()
                        ),
                        conflicts: Vec::new(),
                    });
                }
                _ => {}
            }
        }
    }

    fn finish_sequence(&self, kind: &str, applied: usize) -> anyhow::Result<SeqOutcome> {
        let head = self.head_oid()?.context("没有当前提交")?;
        if kind == "rebase" {
            let head_name = self.seq_read("head-name").unwrap_or_default();
            let onto = self.seq_read("onto").unwrap_or_default();
            if head_name.starts_with("refs/") {
                self.set_ref_logged(&head_name, head, &format!("rebase (finish): {head_name} onto {onto}"))?;
                self.set_head_symbolic(&head_name)?;
                self.append_reflog("HEAD", head, head, &format!("rebase (finish): returning to {head_name}"))?;
            }
            self.clear_sequence()?;
            let name = head_name.trim_start_matches("refs/heads/");
            return Ok(SeqOutcome::Done(format!("已变基 {name}，当前提交 {}", head.short())));
        }
        self.clear_sequence()?;
        Ok(SeqOutcome::Done(format!(
            "{kind} 完成，应用了 {applied} 个提交，当前提交 {}",
            head.short()
        )))
    }

    fn squash_into_head(&self, tree: Oid, step: &Todo, committer: &Signature) -> anyhow::Result<()> {
        let head = self.head_oid()?.context("没有当前提交")?;
        let old = self.read_commit(head)?;
        let mut message = old.message.trim_end().to_string();
        if step.action == TodoAction::Squash {
            if let Some(oid) = step.oid {
                let extra = self.read_commit(oid)?.message;
                message = format!("{message}\n\n{}", extra.trim_end());
            }
            if let Ok(text) = self.run_editor(&format!("{message}\n"), false, "COMMIT_EDITMSG") {
                if !text.trim().is_empty() {
                    message = text.trim_end().to_string();
                }
            }
        }
        message.push('\n');
        let oid = self.write_commit(tree, old.parents.clone(), old.author.clone(), committer.clone(), &message)?;
        let label = if step.action == TodoAction::Squash { "squash" } else { "fixup" };
        self.update_head(oid, &format!("rebase ({label}): {}", first_line(&message)))
    }

    pub(super) fn write_commit(
        &self,
        tree: Oid,
        parents: Vec<Oid>,
        author: Signature,
        committer: Signature,
        message: &str,
    ) -> anyhow::Result<Oid> {
        let mut message = message.to_string();
        if !message.ends_with('\n') {
            message.push('\n');
        }
        let commit = Commit {
            tree,
            parents,
            author,
            committer,
            message,
        };
        self.write_object("commit", &commit.encode())
    }

    /// 在当前暂存区上应用 `oid` 相对其父提交的改动（`revert` 时反向应用），写工作区和暂存区。
    fn apply_change(&self, oid: Oid, revert: bool, mainline: Option<usize>) -> anyhow::Result<MergedTree> {
        let commit = self.read_commit(oid)?;
        let parent = if commit.parents.len() > 1 {
            let m = mainline.context("合并提交需要 -m 指定主线")?;
            Some(*commit.parents.get(m.saturating_sub(1)).context("-m 超出父提交数")?)
        } else {
            commit.parents.first().copied()
        };
        let parent_files = self.files_of_commit(parent)?;
        let commit_files = self.tree_files(commit.tree)?;
        let (base, theirs) = if revert {
            (commit_files, parent_files)
        } else {
            (parent_files, commit_files)
        };
        let ours = index_map(&self.read_index()?);
        for (path, (mode, blob)) in &theirs {
            if ours.contains_key(path) {
                continue;
            }
            let disk = self.work_path(path)?;
            if fs::symlink_metadata(&disk).is_ok() {
                let (have_mode, have_oid) = self.hash_work(path)?;
                if have_mode != *mode || have_oid != *blob {
                    bail!("未跟踪文件会被覆盖: {path}");
                }
            }
        }
        let label = format!("{}... {}", oid.short(), commit.summary());
        let text = TextOptions {
            base_label: parent.map(|p| p.short()).unwrap_or_default(),
            ..TextOptions::default()
        };
        let merged = self.merge_file_maps(&base, &ours, &theirs, &label, &text)?;
        for path in ours.keys() {
            if !merged.files.contains_key(path) && !merged.conflicts.iter().any(|item| item == path) {
                let disk = self.work_path(path)?;
                if fs::symlink_metadata(&disk).is_ok() {
                    let _ = fs::remove_file(&disk);
                    self.prune_empty_dirs(path);
                }
            }
        }
        self.checkout_files(&merged.files)?;
        if !merged.conflicts.is_empty() {
            self.write_unmerged_index(&merged)?;
            self.write_conflict_list(&merged.conflicts)?;
        }
        Ok(merged)
    }

    /// `head` 有而 `upstream` 没有的非合并提交，从旧到新；改动已在 upstream 里的提交会被跳过。
    fn commits_to_replay(&self, head: Oid, upstream: Oid) -> anyhow::Result<Vec<Oid>> {
        let mut excluded = HashSet::new();
        self.mark_reachable_commits(upstream, &mut excluded)?;
        let mut picked = Vec::new();
        for view in self.log(head, usize::MAX)? {
            if excluded.contains(&view.oid) {
                continue;
            }
            if view.parents.len() > 1 {
                continue;
            }
            picked.push(view.oid);
        }
        picked.reverse();
        if picked.is_empty() {
            return Ok(picked);
        }
        let mut theirs = HashSet::new();
        let mut mine = HashSet::new();
        self.mark_reachable_commits(head, &mut mine)?;
        for view in self.log(upstream, 2000)? {
            if mine.contains(&view.oid) || view.parents.len() > 1 {
                continue;
            }
            if let Some(id) = self.patch_id(view.oid)? {
                theirs.insert(id);
            }
        }
        if theirs.is_empty() {
            return Ok(picked);
        }
        let mut out = Vec::new();
        for oid in picked {
            match self.patch_id(oid)? {
                Some(id) if theirs.contains(&id) => {}
                _ => out.push(oid),
            }
        }
        Ok(out)
    }

    pub(super) fn mark_reachable_commits(&self, start: Oid, out: &mut HashSet<Oid>) -> anyhow::Result<()> {
        let mut stack = vec![start];
        while let Some(oid) = stack.pop() {
            if oid.is_zero() || !out.insert(oid) {
                continue;
            }
            if let Ok(commit) = self.read_commit(oid) {
                stack.extend(commit.parents);
            }
        }
        Ok(())
    }

    /// 与行号无关的改动指纹，相同改动的两个提交得到同一个值。
    fn patch_id(&self, oid: Oid) -> anyhow::Result<Option<[u8; 20]>> {
        let commit = self.read_commit(oid)?;
        let diffs = self.diff_range(commit.parents.first().copied(), oid)?;
        if diffs.is_empty() {
            return Ok(None);
        }
        let mut hasher = Sha1::new();
        for diff in diffs {
            hasher.update(diff.path.as_bytes());
            hasher.update([0]);
            for line in diff.patch.lines() {
                if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
                    continue;
                }
                if line.starts_with('+') || line.starts_with('-') {
                    hasher.update(line.as_bytes());
                    hasher.update([b'\n']);
                }
            }
        }
        Ok(Some(hasher.finalize().into()))
    }

    /// 用编辑器修改一段文本，去掉 `#` 注释行。没有配置编辑器时返回错误。
    pub fn run_editor(&self, initial: &str, sequence: bool, name: &str) -> anyhow::Result<String> {
        let editor = self.editor_command(sequence).context("没有设置编辑器（GIT_EDITOR / EDITOR）")?;
        let path = self.git_dir.join(name);
        fs::write(&path, initial)?;
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(&path)
            .status()
            .with_context(|| format!("启动编辑器 {editor} 失败"))?;
        if !status.success() {
            bail!("编辑器退出码非零，已取消");
        }
        let text = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);
        Ok(text
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
            + "\n")
    }

    fn editor_command(&self, sequence: bool) -> Option<String> {
        let mut names = Vec::new();
        if sequence {
            names.extend(["AIT_SEQUENCE_EDITOR", "GIT_SEQUENCE_EDITOR"]);
        }
        names.extend(["AIT_EDITOR", "GIT_EDITOR"]);
        for name in names {
            if let Ok(value) = std::env::var(name) {
                if !value.trim().is_empty() {
                    return Some(value);
                }
            }
        }
        if sequence {
            if let Ok(Some(value)) = self.config_get("sequence.editor") {
                return Some(value);
            }
        }
        if let Ok(Some(value)) = self.config_get("core.editor") {
            return Some(value);
        }
        for name in ["VISUAL", "EDITOR"] {
            if let Ok(value) = std::env::var(name) {
                if !value.trim().is_empty() {
                    return Some(value);
                }
            }
        }
        None
    }
}

fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or("").trim()
}

pub(super) fn parse_todo(repo: &Repository, text: &str) -> anyhow::Result<Vec<Todo>> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        let action = match word {
            "p" | "pick" => TodoAction::Pick,
            "revert" => TodoAction::Revert,
            "r" | "reword" => TodoAction::Reword,
            "e" | "edit" => TodoAction::Edit,
            "s" | "squash" => TodoAction::Squash,
            "f" | "fixup" => TodoAction::Fixup,
            "d" | "drop" => TodoAction::Drop,
            "x" | "exec" => TodoAction::Exec,
            "b" | "break" => TodoAction::Break,
            other => bail!("执行清单里有未知指令: {other}"),
        };
        match action {
            TodoAction::Exec => out.push(Todo {
                action,
                oid: None,
                text: rest.to_string(),
            }),
            TodoAction::Break => out.push(Todo {
                action,
                oid: None,
                text: String::new(),
            }),
            _ => {
                let (name, summary) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                if name.is_empty() {
                    bail!("执行清单这一行缺少提交: {line}");
                }
                let oid = repo.rev_parse(name)?;
                out.push(Todo {
                    action,
                    oid: Some(oid),
                    text: summary.trim().to_string(),
                });
            }
        }
    }
    Ok(out)
}

fn check_first_step(steps: &[Todo]) -> anyhow::Result<()> {
    let first = steps.iter().find(|t| t.action != TodoAction::Drop);
    if matches!(first.map(|t| t.action), Some(TodoAction::Squash | TodoAction::Fixup)) {
        bail!("第一步不能是 squash 或 fixup");
    }
    Ok(())
}
