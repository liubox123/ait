//! 与 Git 相同格式的 stash：W 提交记工作区，父提交是 HEAD 和记暂存区的 I，可选第三个父提交记未跟踪文件。
//! 栈保存在 `refs/stash` 的 reflog 里，`stash@{n}` 对应第 n 条。

use std::fs;

use anyhow::{bail, Context};

use super::{atomic_write, index_map, Repository};
use crate::git::diff::FileDiff;
use crate::git::merge::{FileMap, TextOptions};
use crate::git::object::Signature;
use crate::git::oid::Oid;

const STASH_REF: &str = "refs/stash";

#[derive(Clone, Debug)]
pub struct StashEntry {
    pub index: usize,
    pub oid: Oid,
    pub message: String,
}

impl Repository {
    pub fn stash_push(
        &self,
        message: Option<&str>,
        include_untracked: bool,
        keep_index: bool,
        who: &Signature,
    ) -> anyhow::Result<Option<Oid>> {
        self.work_tree_required()?;
        if self.merge_head()?.is_some() || self.sequence_kind().is_some() {
            bail!("合并或变基进行中，不能储藏");
        }
        let head = self.head_oid()?.context("还没有提交，不能储藏")?;
        let head_commit = self.read_commit(head)?;
        let index = self.read_index()?;
        if index.entries.iter().any(|e| e.stage > 0) {
            bail!("还有未解决的冲突，不能储藏");
        }
        let index_files = index_map(&index);
        let mut work_files = FileMap::new();
        for (path, (mode, _)) in &index_files {
            let disk = self.work_path(path)?;
            if fs::symlink_metadata(&disk).is_err() {
                continue;
            }
            let (have_mode, _) = self.hash_work(path).unwrap_or((*mode, Oid::ZERO));
            let bytes = self.read_work_bytes(path)?;
            let blob = self.write_object("blob", &bytes)?;
            work_files.insert(path.clone(), (have_mode, blob));
        }
        let untracked = if include_untracked {
            self.status()?.untracked
        } else {
            Vec::new()
        };
        let index_tree = self.write_tree(&index_files)?;
        let work_tree = self.write_tree(&work_files)?;
        if index_tree == head_commit.tree && work_tree == head_commit.tree && untracked.is_empty() {
            return Ok(None);
        }
        let branch = self.branch_name()?.unwrap_or_else(|| "(no branch)".into());
        let base_label = format!("{branch}: {} {}", head.short(), head_commit.summary());
        let index_commit = self.write_commit(
            index_tree,
            vec![head],
            who.clone(),
            who.clone(),
            &format!("index on {base_label}\n"),
        )?;
        let mut parents = vec![head, index_commit];
        if !untracked.is_empty() {
            let mut files = FileMap::new();
            for path in &untracked {
                let (mode, _) = self.hash_work(path)?;
                let blob = self.write_object("blob", &self.read_work_bytes(path)?)?;
                files.insert(path.clone(), (mode, blob));
            }
            let tree = self.write_tree(&files)?;
            parents.push(self.write_commit(
                tree,
                Vec::new(),
                who.clone(),
                who.clone(),
                &format!("untracked files on {base_label}\n"),
            )?);
        }
        let text = match message {
            Some(text) if !text.trim().is_empty() => format!("On {branch}: {}", text.trim()),
            _ => format!("WIP on {base_label}"),
        };
        let stash = self.write_commit(work_tree, parents, who.clone(), who.clone(), &format!("{text}\n"))?;
        let log = self.git_dir.join("logs").join(STASH_REF);
        if let Some(parent) = log.parent() {
            fs::create_dir_all(parent)?;
        }
        if !log.exists() {
            fs::write(&log, "")?;
        }
        self.set_ref_logged(STASH_REF, stash, &text)?;
        self.discard_to(head)?;
        for path in &untracked {
            let disk = self.work_path(path)?;
            if fs::symlink_metadata(&disk).is_ok() {
                fs::remove_file(&disk)?;
                self.prune_empty_dirs(path);
            }
        }
        if keep_index {
            let files = self.tree_files(index_tree)?;
            for (path, (mode, oid)) in &files {
                self.materialize(path, *mode, *oid)?;
            }
            let head_files = self.tree_files(head_commit.tree)?;
            for path in head_files.keys() {
                if !files.contains_key(path) {
                    let disk = self.work_path(path)?;
                    if fs::symlink_metadata(&disk).is_ok() {
                        fs::remove_file(&disk)?;
                    }
                }
            }
            self.write_index_from_map(&files)?;
        }
        Ok(Some(stash))
    }

    pub fn stash_list(&self) -> anyhow::Result<Vec<StashEntry>> {
        if self.resolve_ref(STASH_REF)?.is_none() {
            return Ok(Vec::new());
        }
        Ok(self
            .reflog(STASH_REF)?
            .into_iter()
            .enumerate()
            .map(|(index, entry)| StashEntry {
                index,
                oid: entry.new,
                message: entry.message,
            })
            .collect())
    }

    fn stash_at(&self, n: usize) -> anyhow::Result<Oid> {
        let list = self.stash_list()?;
        if list.is_empty() {
            bail!("没有储藏");
        }
        list.get(n)
            .map(|entry| entry.oid)
            .with_context(|| format!("没有 stash@{{{n}}}"))
    }

    /// 把储藏应用到当前工作区。返回冲突路径；为空表示干净应用。
    pub fn stash_apply(&self, n: usize, restore_index: bool) -> anyhow::Result<Vec<String>> {
        self.work_tree_required()?;
        let stash = self.stash_at(n)?;
        let commit = self.read_commit(stash)?;
        let base_oid = *commit.parents.first().context("储藏提交缺少父提交")?;
        let base = self.files_of_commit(Some(base_oid))?;
        let theirs = self.tree_files(commit.tree)?;
        let index = self.read_index()?;
        if index.entries.iter().any(|e| e.stage > 0) {
            bail!("还有未解决的冲突，不能应用储藏");
        }
        let ours = index_map(&index);
        let status = self.status()?;
        for entry in &status.unstaged {
            if base.get(&entry.path) != theirs.get(&entry.path) {
                bail!("{} 有未暂存的修改，会被储藏覆盖，请先提交或储藏", entry.path);
            }
        }
        let untracked_files = match commit.parents.get(2) {
            Some(oid) => self.files_of_commit(Some(*oid))?,
            None => FileMap::new(),
        };
        for path in theirs.keys().chain(untracked_files.keys()) {
            if !ours.contains_key(path) && status.untracked.iter().any(|item| item == path) {
                bail!("未跟踪文件会被覆盖: {path}");
            }
        }
        let text = TextOptions {
            base_label: "stash base".into(),
            ..TextOptions::default()
        };
        let merged = self.merge_file_maps(&base, &ours, &theirs, "Stashed changes", &text)?;
        for path in ours.keys() {
            if !merged.files.contains_key(path) && !merged.conflicts.iter().any(|p| p == path) {
                let disk = self.work_path(path)?;
                if fs::symlink_metadata(&disk).is_ok() {
                    fs::remove_file(&disk)?;
                    self.prune_empty_dirs(path);
                }
            }
        }
        for (path, (mode, oid)) in &merged.files {
            if ours.get(path) != Some(&(*mode, *oid)) {
                self.materialize(path, *mode, *oid)?;
            }
        }
        let mut new_index = index.clone();
        for (path, (mode, oid)) in &merged.files {
            if !ours.contains_key(path) && !base.contains_key(path) {
                new_index.upsert(self.index_entry_for(path, *mode, *oid));
            }
        }
        for path in ours.keys() {
            if !merged.files.contains_key(path) && base.contains_key(path) && !theirs.contains_key(path) {
                new_index.remove(path);
            }
        }
        if restore_index {
            let index_commit = *commit.parents.get(1).context("储藏缺少暂存区提交")?;
            let staged = self.files_of_commit(Some(index_commit))?;
            let wanted = self.merge_file_maps(&base, &ours, &staged, "Stashed index", &text)?;
            if !wanted.conflicts.is_empty() {
                bail!("暂存区状态无法还原（有冲突），可去掉 --index 重试");
            }
            for (path, (mode, oid)) in &wanted.files {
                if ours.get(path) != Some(&(*mode, *oid)) {
                    new_index.remove(path);
                    new_index.upsert(self.index_entry_for(path, *mode, *oid));
                }
            }
            for path in ours.keys() {
                if !wanted.files.contains_key(path) {
                    new_index.remove(path);
                }
            }
        }
        self.write_index_file(&new_index)?;
        for (path, (mode, oid)) in &untracked_files {
            self.materialize(path, *mode, *oid)?;
        }
        if !merged.conflicts.is_empty() {
            self.write_unmerged_index(&merged)?;
            self.write_conflict_list(&merged.conflicts)?;
        }
        Ok(merged.conflicts)
    }

    pub fn stash_drop(&self, n: usize) -> anyhow::Result<Oid> {
        let mut log = self.reflog(STASH_REF)?;
        if log.is_empty() {
            bail!("没有储藏");
        }
        if n >= log.len() {
            bail!("没有 stash@{{{n}}}");
        }
        let dropped = log.remove(n).new;
        if log.is_empty() {
            self.delete_ref(STASH_REF)?;
        } else {
            atomic_write(
                &self.git_dir.join(STASH_REF),
                format!("{}\n", log[0].new).as_bytes(),
            )?;
            if n == log.len() {
                log.last_mut().unwrap().old = Oid::ZERO;
            }
            self.write_reflog(STASH_REF, &log)?;
        }
        Ok(dropped)
    }

    pub fn stash_clear(&self) -> anyhow::Result<()> {
        if self.resolve_ref(STASH_REF)?.is_some() {
            self.delete_ref(STASH_REF)?;
        }
        Ok(())
    }

    pub fn stash_show(&self, n: usize) -> anyhow::Result<Vec<FileDiff>> {
        let stash = self.stash_at(n)?;
        let commit = self.read_commit(stash)?;
        self.diff_range(commit.parents.first().copied(), stash)
    }
}
