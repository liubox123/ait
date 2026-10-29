//! 对象维护：暂存区隔离（推送通过前对象只在内存里）、写 pack、gc、fsck、pack-refs。

use std::collections::HashSet;
use std::fs;
use std::time::{Duration, SystemTime};

use anyhow::Context;

use super::{atomic_write, Repository};
use crate::git::object::{hash_payload, split_raw, Object, TagObject};
use crate::git::odb;
use crate::git::oid::Oid;
use crate::git::pack::{self, static_kind, PackItem};

#[derive(Clone, Debug, Default)]
pub struct GcReport {
    pub packed: usize,
    pub loose_removed: usize,
    pub pruned: usize,
    pub packs_removed: usize,
    pub refs_packed: usize,
}

#[derive(Clone, Debug, Default)]
pub struct FsckReport {
    pub checked: usize,
    pub problems: Vec<String>,
    pub dangling: Vec<Oid>,
}

#[derive(Clone, Debug, Default)]
pub struct ObjectCount {
    pub loose: usize,
    pub loose_bytes: u64,
    pub packs: usize,
    pub packed: usize,
    pub pack_bytes: u64,
}

/// 超过这个数量的对象写成 pack，否则写 loose 对象。
const PACK_THRESHOLD: usize = 64;

impl Repository {
    // ---------- 隔离区 ----------

    /// 把对象放进内存隔离区：之后能读到，但在 `persist_staged` 之前不落盘。
    pub fn stage_object(&self, kind: &str, payload: &[u8]) -> anyhow::Result<Oid> {
        let kind = static_kind(kind).with_context(|| format!("不能写入未知对象类型 {kind}"))?;
        Object::parse(kind, payload).with_context(|| format!("{kind} 对象格式错误"))?;
        let (oid, _) = hash_payload(kind, payload);
        self.staged
            .borrow_mut()
            .entry(oid)
            .or_insert_with(|| (kind, payload.to_vec()));
        Ok(oid)
    }

    /// 解开一个 pack 放进隔离区。允许 thin pack，基对象从仓库读取。
    pub fn stage_pack(&self, data: &[u8]) -> anyhow::Result<Vec<Oid>> {
        let objects = pack::unpack(data, &|oid| {
            let object = self.read_object(oid).ok()?;
            Some((static_kind(object.kind())?, object.payload()))
        })?;
        let mut out = Vec::with_capacity(objects.len());
        for (oid, kind, payload) in objects {
            Object::parse(kind, &payload).with_context(|| format!("{oid} 格式错误"))?;
            self.staged.borrow_mut().insert(oid, (kind, payload));
            out.push(oid);
        }
        Ok(out)
    }

    pub fn discard_staged(&self) {
        self.staged.borrow_mut().clear();
    }

    /// 把隔离区里的对象写进对象库，数量多时写成一个 pack。
    pub fn persist_staged(&self) -> anyhow::Result<usize> {
        let staged: Vec<(Oid, &'static str, Vec<u8>)> = self
            .staged
            .borrow_mut()
            .drain()
            .map(|(oid, (kind, payload))| (oid, kind, payload))
            .collect();
        let fresh: Vec<_> = staged
            .into_iter()
            .filter(|(oid, _, _)| !self.has_stored(*oid))
            .collect();
        let count = fresh.len();
        if count >= PACK_THRESHOLD {
            self.write_pack_items(
                fresh
                    .into_iter()
                    .map(|(oid, kind, payload)| PackItem {
                        oid,
                        kind,
                        payload,
                        hint: String::new(),
                    })
                    .collect(),
            )?;
        } else {
            for (_, kind, payload) in fresh {
                odb::write_loose(&self.git_dir, kind, &payload)?;
            }
        }
        Ok(count)
    }

    /// 对象已经在磁盘上（loose 或 pack）。
    pub fn has_stored(&self, oid: Oid) -> bool {
        if odb::loose_path(&self.git_dir, oid).exists() {
            return true;
        }
        self.with_packs(|packs| packs.contains(oid)).unwrap_or(false)
    }

    pub fn has_object(&self, oid: Oid) -> bool {
        self.staged.borrow().contains_key(&oid) || self.has_stored(oid)
    }

    pub(super) fn with_packs<T>(&self, f: impl FnOnce(&pack::PackCache) -> T) -> anyhow::Result<T> {
        {
            let mut slot = self.packs.borrow_mut();
            if slot.is_none() {
                *slot = Some(pack::PackCache::load(&self.git_dir)?);
            }
        }
        let slot = self.packs.borrow();
        Ok(f(slot.as_ref().expect("刚加载")))
    }

    fn reset_pack_cache(&self) {
        *self.packs.borrow_mut() = None;
    }

    /// 把对象写成 `objects/pack/pack-<校验和>.pack/.idx`，返回 pack 名。
    pub fn write_pack_items(&self, items: Vec<PackItem>) -> anyhow::Result<String> {
        let written = pack::write_pack(items, true)?;
        let name = format!("pack-{}", hex::encode(written.checksum));
        let dir = self.git_dir.join("objects/pack");
        fs::create_dir_all(&dir)?;
        let pack_path = dir.join(format!("{name}.pack"));
        let idx_path = dir.join(format!("{name}.idx"));
        if !pack_path.exists() {
            atomic_write(&pack_path, &written.pack)?;
        }
        atomic_write(&idx_path, &written.idx)?;
        self.reset_pack_cache();
        Ok(name)
    }

    /// 生成包含 `tips` 可达、但 `have` 不可达对象的 pack 字节流，供网络传输。
    pub fn pack_for(&self, tips: &[Oid], have: &[Oid]) -> anyhow::Result<Vec<u8>> {
        let mut known = HashSet::new();
        for oid in have {
            if self.has_object(*oid) {
                let _ = self.mark_reachable(*oid, &mut known);
            }
        }
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        for tip in tips {
            self.collect_objects(*tip, "", &known, &mut seen, &mut items)?;
        }
        Ok(pack::write_pack(items, true)?.pack)
    }

    fn collect_objects(
        &self,
        start: Oid,
        hint: &str,
        known: &HashSet<Oid>,
        seen: &mut HashSet<Oid>,
        out: &mut Vec<PackItem>,
    ) -> anyhow::Result<()> {
        let mut stack = vec![(start, hint.to_string())];
        while let Some((oid, hint)) = stack.pop() {
            if oid.is_zero() || known.contains(&oid) || !seen.insert(oid) {
                continue;
            }
            let object = self.read_object(oid)?;
            match &object {
                Object::Commit(commit) => {
                    stack.push((commit.tree, String::new()));
                    for parent in &commit.parents {
                        stack.push((*parent, String::new()));
                    }
                }
                Object::Tree(entries) => {
                    for entry in entries {
                        if entry.mode == 0o160000 {
                            continue;
                        }
                        stack.push((entry.oid, entry.name.clone()));
                    }
                }
                Object::Tag(payload) => {
                    if let Ok(tag) = TagObject::parse(payload) {
                        stack.push((tag.object, String::new()));
                    }
                }
                Object::Blob(_) => {}
            }
            out.push(PackItem {
                oid,
                kind: static_kind(object.kind()).expect("已知类型"),
                payload: object.payload(),
                hint,
            });
        }
        Ok(())
    }

    /// gc 和 fsck 的根：所有引用、HEAD 类文件、reflog、暂存区和进行中的序列。
    fn root_objects(&self) -> anyhow::Result<Vec<Oid>> {
        let mut roots: Vec<Oid> = self.list_refs("refs/")?.into_iter().map(|(_, o)| o).collect();
        for name in ["HEAD", "ORIG_HEAD", "MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "FETCH_HEAD", "ait/MERGE_HEAD"] {
            if let Ok(text) = fs::read_to_string(self.git_dir.join(name)) {
                for word in text.split_whitespace() {
                    if let Ok(oid) = Oid::parse(word) {
                        roots.push(oid);
                    }
                }
            }
        }
        if let Some(head) = self.head_oid().ok().flatten() {
            roots.push(head);
        }
        let logs = self.git_dir.join("logs");
        if logs.is_dir() {
            let mut stack = vec![logs];
            while let Some(dir) = stack.pop() {
                for entry in fs::read_dir(&dir)?.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if let Ok(text) = fs::read_to_string(&path) {
                        for line in text.lines() {
                            for word in line.split_whitespace().take(2) {
                                if let Ok(oid) = Oid::parse(word) {
                                    roots.push(oid);
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Ok(index) = self.read_index() {
            roots.extend(index.entries.iter().map(|e| e.oid));
        }
        let todo = self.git_dir.join("ait/sequencer");
        if todo.is_dir() {
            for entry in fs::read_dir(&todo)?.flatten() {
                if let Ok(text) = fs::read_to_string(entry.path()) {
                    for word in text.split_whitespace() {
                        if word.len() == 40 {
                            if let Ok(oid) = Oid::parse(word) {
                                roots.push(oid);
                            }
                        }
                    }
                }
            }
        }
        roots.retain(|o| !o.is_zero());
        roots.sort();
        roots.dedup();
        Ok(roots)
    }

    fn loose_objects(&self) -> anyhow::Result<Vec<(Oid, std::path::PathBuf)>> {
        let mut out = Vec::new();
        let objects = self.git_dir.join("objects");
        if !objects.is_dir() {
            return Ok(out);
        }
        for dir in fs::read_dir(&objects)?.flatten() {
            let name = dir.file_name().to_string_lossy().to_string();
            if name.len() != 2 || !dir.path().is_dir() {
                continue;
            }
            for file in fs::read_dir(dir.path())?.flatten() {
                let rest = file.file_name().to_string_lossy().to_string();
                if let Ok(oid) = Oid::parse(&format!("{name}{rest}")) {
                    out.push((oid, file.path()));
                }
            }
        }
        Ok(out)
    }

    /// 把可达对象打成一个 pack，删除已打包的 loose 对象和旧 pack，并打包引用。
    /// `prune_now` 时立即丢弃不可达对象，否则只丢弃两周以前的不可达 loose 对象。
    pub fn gc(&self, prune_now: bool) -> anyhow::Result<GcReport> {
        let mut report = GcReport::default();
        let roots = self.root_objects()?;
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        let known = HashSet::new();
        for root in &roots {
            if self.has_object(*root) {
                self.collect_objects(*root, "", &known, &mut seen, &mut items)?;
            }
        }
        let old_packs = self.with_packs(|packs| packs.pack_paths())?;
        if !prune_now {
            let packed: Vec<Oid> = self.with_packs(|packs| packs.oids())?;
            for oid in packed {
                if seen.insert(oid) {
                    let object = self.read_object(oid)?;
                    items.push(PackItem {
                        oid,
                        kind: static_kind(object.kind()).expect("已知类型"),
                        payload: object.payload(),
                        hint: String::new(),
                    });
                }
            }
        }
        report.packed = items.len();
        let new_name = if items.is_empty() {
            None
        } else {
            Some(self.write_pack_items(items)?)
        };
        for path in old_packs {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if Some(stem) == new_name.as_deref() {
                continue;
            }
            let _ = fs::remove_file(path.with_extension("idx"));
            let _ = fs::remove_file(&path);
            report.packs_removed += 1;
        }
        self.reset_pack_cache();
        let cutoff = SystemTime::now() - Duration::from_secs(14 * 24 * 3600);
        for (oid, path) in self.loose_objects()? {
            if seen.contains(&oid) {
                if self.with_packs(|packs| packs.contains(oid))? {
                    fs::remove_file(&path)?;
                    report.loose_removed += 1;
                }
                continue;
            }
            let old = fs::metadata(&path)
                .and_then(|m| m.modified())
                .map(|t| t < cutoff)
                .unwrap_or(false);
            if prune_now || old {
                fs::remove_file(&path)?;
                report.pruned += 1;
            }
        }
        let objects = self.git_dir.join("objects");
        for dir in fs::read_dir(&objects)?.flatten() {
            if dir.file_name().len() == 2 {
                let _ = fs::remove_dir(dir.path());
            }
        }
        report.refs_packed = self.pack_refs()?;
        Ok(report)
    }

    /// 把 refs/ 下的引用写进 packed-refs，附注标签带上剥开后的提交，然后删掉对应的 loose 引用文件。
    pub fn pack_refs(&self) -> anyhow::Result<usize> {
        let refs = self.list_refs("refs/")?;
        if refs.is_empty() {
            return Ok(0);
        }
        let mut text = String::from("# pack-refs with: peeled fully-peeled sorted \n");
        for (name, oid) in &refs {
            if name == "refs/stash" {
                continue;
            }
            text.push_str(&format!("{oid} {name}\n"));
            if name.starts_with("refs/tags/") {
                if let Ok(Object::Tag(_)) = self.read_object(*oid) {
                    if let Ok(peeled) = self.peel_tags(*oid) {
                        text.push_str(&format!("^{peeled}\n"));
                    }
                }
            }
        }
        atomic_write(&self.git_dir.join("packed-refs"), text.as_bytes())?;
        let mut count = 0;
        for (name, oid) in &refs {
            if name == "refs/stash" {
                continue;
            }
            let path = self.git_dir.join(name);
            if let Ok(content) = fs::read_to_string(&path) {
                if content.trim() == oid.to_hex() {
                    fs::remove_file(&path)?;
                    count += 1;
                }
            }
        }
        let refs_dir = self.git_dir.join("refs");
        prune_empty(&refs_dir, &refs_dir);
        Ok(count)
    }

    /// 校验每个对象的哈希和可达性。
    pub fn fsck(&self) -> anyhow::Result<FsckReport> {
        let mut report = FsckReport::default();
        let mut all = HashSet::new();
        for (oid, path) in self.loose_objects()? {
            report.checked += 1;
            all.insert(oid);
            let ok = fs::read(&path)
                .ok()
                .and_then(|bytes| {
                    use std::io::Read;
                    let mut raw = Vec::new();
                    flate2::read::ZlibDecoder::new(&bytes[..]).read_to_end(&mut raw).ok()?;
                    let (kind, payload) = split_raw(&raw).ok()?;
                    Object::parse(kind, payload).ok()?;
                    Some(hash_payload(kind, payload).0 == oid)
                })
                .unwrap_or(false);
            if !ok {
                report.problems.push(format!("loose 对象损坏: {oid}"));
            }
        }
        for oid in self.with_packs(|packs| packs.oids())? {
            if !all.insert(oid) {
                continue;
            }
            report.checked += 1;
            match self.with_packs(|packs| packs.raw(oid))? {
                Some(Ok((kind, payload))) => {
                    if hash_payload(kind, &payload).0 != oid || Object::parse(kind, &payload).is_err() {
                        report.problems.push(format!("pack 对象损坏: {oid}"));
                    }
                }
                Some(Err(err)) => report.problems.push(format!("pack 对象无法读取 {oid}: {err:#}")),
                None => {}
            }
        }
        let mut reachable = HashSet::new();
        let mut stack = self.root_objects()?;
        while let Some(oid) = stack.pop() {
            if !reachable.insert(oid) {
                continue;
            }
            match self.read_object(oid) {
                Ok(Object::Commit(commit)) => {
                    stack.push(commit.tree);
                    stack.extend(commit.parents);
                }
                Ok(Object::Tree(entries)) => {
                    stack.extend(entries.into_iter().filter(|e| e.mode != 0o160000).map(|e| e.oid));
                }
                Ok(Object::Tag(payload)) => {
                    if let Ok(tag) = TagObject::parse(&payload) {
                        stack.push(tag.object);
                    }
                }
                Ok(Object::Blob(_)) => {}
                Err(_) => report.problems.push(format!("缺少对象: {oid}")),
            }
        }
        let mut dangling: Vec<Oid> = all
            .into_iter()
            .filter(|oid| !reachable.contains(oid))
            .filter(|oid| matches!(self.read_object(*oid), Ok(Object::Commit(_))))
            .collect();
        dangling.sort();
        report.dangling = dangling;
        Ok(report)
    }

    pub fn count_objects(&self) -> anyhow::Result<ObjectCount> {
        let mut count = ObjectCount::default();
        for (_, path) in self.loose_objects()? {
            count.loose += 1;
            count.loose_bytes += fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        }
        let paths = self.with_packs(|packs| packs.pack_paths())?;
        count.packs = paths.len();
        for path in paths {
            count.pack_bytes += fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        }
        count.packed = self.with_packs(|packs| packs.oids().len())?;
        Ok(count)
    }

    /// 读对象的原始内容（隔离区、loose、pack 依次查找）。
    pub fn read_raw(&self, oid: Oid) -> anyhow::Result<(&'static str, Vec<u8>)> {
        let object = self.read_object(oid)?;
        Ok((static_kind(object.kind()).expect("已知类型"), object.payload()))
    }
}

fn prune_empty(dir: &std::path::Path, root: &std::path::Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                prune_empty(&entry.path(), root);
            }
        }
    }
    if dir != root {
        let keep = ["refs/heads", "refs/tags"]
            .iter()
            .any(|k| dir.ends_with(k));
        if !keep {
            let _ = fs::remove_dir(dir);
        }
    }
}