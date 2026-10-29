use std::collections::BTreeMap;

use super::oid::Oid;

pub type FileMap = BTreeMap<String, (u32, Oid)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeAction {
    Take(u32, Oid),
    Delete,
    Conflict {
        ours: Option<(u32, Oid)>,
        theirs: Option<(u32, Oid)>,
    },
}

/// 按路径做三方合并。两边都改了同一路径时先记为冲突，调用方再对文本做行级合并。
pub fn merge_maps(
    base: &FileMap,
    ours: &FileMap,
    theirs: &FileMap,
) -> BTreeMap<String, MergeAction> {
    let mut paths = BTreeMap::<String, ()>::new();
    for key in base.keys().chain(ours.keys()).chain(theirs.keys()) {
        paths.insert(key.clone(), ());
    }
    let mut out = BTreeMap::new();
    for path in paths.into_keys() {
        let b = base.get(&path).copied();
        let o = ours.get(&path).copied();
        let t = theirs.get(&path).copied();
        let action = match (b, o, t) {
            (_, Some(o), Some(t)) if o == t => MergeAction::Take(o.0, o.1),
            (Some(b), Some(o), Some(t)) if b == o && b != t => MergeAction::Take(t.0, t.1),
            (Some(b), Some(o), Some(t)) if b == t && b != o => MergeAction::Take(o.0, o.1),
            (None, Some(o), None) => MergeAction::Take(o.0, o.1),
            (None, None, Some(t)) => MergeAction::Take(t.0, t.1),
            (Some(b), Some(o), None) if b == o => MergeAction::Delete,
            (Some(b), None, Some(t)) if b == t => MergeAction::Delete,
            (Some(_), None, None) => MergeAction::Delete,
            (None, None, None) => continue,
            (_, ours, theirs) => MergeAction::Conflict { ours, theirs },
        };
        out.insert(path, action);
    }
    out
}

/// 与 Git `diffcore-rename` 相同：先认完全相同的对象，再用 64 字节分块哈希估计相似度。
/// 默认阈值是 50%（`MAX_SCORE` 的一半）。返回需要人工确认的重命名/删除路径。
pub fn adjust_renames(
    base: &mut FileMap,
    ours: &mut FileMap,
    theirs: &mut FileMap,
    blobs: &dyn Fn(Oid) -> Vec<u8>,
) -> Vec<String> {
    let mut conflicts = Vec::new();
    conflicts.extend(move_renames(base, ours, theirs, blobs));
    conflicts.extend(move_renames(base, theirs, ours, blobs));
    conflicts.sort();
    conflicts.dedup();
    conflicts
}

fn move_renames(
    base: &mut FileMap,
    renamed: &mut FileMap,
    other: &mut FileMap,
    blobs: &dyn Fn(Oid) -> Vec<u8>,
) -> Vec<String> {
    let mut conflicts = Vec::new();
    let pairs = detect_renames(base, renamed, blobs);
    for (src, dst) in pairs {
        let Some(origin) = base.get(&src).copied() else {
            continue;
        };
        if other.contains_key(&src) && !other.contains_key(&dst) && !base.contains_key(&dst) {
            base.insert(dst.clone(), origin);
            base.remove(&src);
            if let Some(entry) = other.remove(&src) {
                other.insert(dst, entry);
            }
            continue;
        }
        if !other.contains_key(&src)
            && other.contains_key(&dst)
            && !base.contains_key(&dst)
            && other.get(&dst).map(|entry| entry.1) != Some(origin.1)
        {
            base.insert(dst.clone(), origin);
            base.remove(&src);
            continue;
        }
        if !other.contains_key(&src) && !other.contains_key(&dst) {
            conflicts.push(dst);
        }
    }
    conflicts
}

fn detect_renames(
    base: &FileMap,
    side: &FileMap,
    blobs: &dyn Fn(Oid) -> Vec<u8>,
) -> Vec<(String, String)> {
    let mut pairs = exact_renames(base, side);
    let mut used_src: BTreeMap<String, ()> =
        pairs.iter().map(|(src, _)| (src.clone(), ())).collect();
    let mut used_dst: BTreeMap<String, ()> =
        pairs.iter().map(|(_, dst)| (dst.clone(), ())).collect();
    let deleted: Vec<(String, Oid)> = base
        .iter()
        .filter(|(path, (mode, _))| {
            is_regular(*mode) && !side.contains_key(*path) && !used_src.contains_key(*path)
        })
        .map(|(path, (_, oid))| (path.clone(), *oid))
        .collect();
    let added: Vec<(String, Oid)> = side
        .iter()
        .filter(|(path, (mode, _))| {
            is_regular(*mode) && !base.contains_key(*path) && !used_dst.contains_key(*path)
        })
        .map(|(path, (_, oid))| (path.clone(), *oid))
        .collect();
    let mut scored = Vec::new();
    for (src, src_oid) in &deleted {
        let src_bytes = blobs(*src_oid);
        for (dst, dst_oid) in &added {
            let score = content_similarity(&src_bytes, &blobs(*dst_oid));
            if score >= RENAME_THRESHOLD {
                scored.push((score, src.clone(), dst.clone()));
            }
        }
    }
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then(left.1.cmp(&right.1))
            .then(left.2.cmp(&right.2))
    });
    for (_, src, dst) in scored {
        if used_src.contains_key(&src) || used_dst.contains_key(&dst) {
            continue;
        }
        used_src.insert(src.clone(), ());
        used_dst.insert(dst.clone(), ());
        pairs.push((src, dst));
    }
    pairs
}

fn is_regular(mode: u32) -> bool {
    mode == 0o100644 || mode == 0o100755
}

const HASHBASE: u32 = 107927;
const MAX_SCORE: u32 = 60000;
const RENAME_THRESHOLD: u32 = 30000;

/// Git `estimate_similarity`：返回 0 到 60000，30000 表示 50%。
pub fn content_similarity(src: &[u8], dst: &[u8]) -> u32 {
    if src.is_empty() && dst.is_empty() {
        return MAX_SCORE;
    }
    let max_size = src.len().max(dst.len()) as u64;
    let base_size = src.len().min(dst.len()) as u64;
    let delta_size = max_size - base_size;
    if max_size * u64::from(MAX_SCORE - RENAME_THRESHOLD) < delta_size * u64::from(MAX_SCORE) {
        return 0;
    }
    if dst.is_empty() {
        return 0;
    }
    let (copied, _) = count_spans(&hash_spans(src), &hash_spans(dst));
    (copied * u64::from(MAX_SCORE) / max_size) as u32
}

fn hash_spans(bytes: &[u8]) -> Vec<(u32, u32)> {
    let text = !is_binary(bytes);
    let mut counts = BTreeMap::<u32, u32>::new();
    let mut index = 0;
    let mut length = 0u32;
    let mut accum1 = 0u32;
    let mut accum2 = 0u32;
    while index < bytes.len() {
        let byte = bytes[index] as u32;
        index += 1;
        if text && byte == b'\r' as u32 && index < bytes.len() && bytes[index] == b'\n' {
            continue;
        }
        let old = accum1;
        accum1 = accum1.wrapping_shl(7) ^ (accum2 >> 25);
        accum2 = accum2.wrapping_shl(7) ^ (old >> 25);
        accum1 = accum1.wrapping_add(byte);
        length += 1;
        if length < 64 && byte != b'\n' as u32 {
            continue;
        }
        let hash = accum1.wrapping_add(accum2.wrapping_mul(0x61)) % HASHBASE;
        *counts.entry(hash).or_insert(0) += length;
        length = 0;
        accum1 = 0;
        accum2 = 0;
    }
    if length > 0 {
        let hash = accum1.wrapping_add(accum2.wrapping_mul(0x61)) % HASHBASE;
        *counts.entry(hash).or_insert(0) += length;
    }
    counts.into_iter().collect()
}

fn count_spans(src: &[(u32, u32)], dst: &[(u32, u32)]) -> (u64, u64) {
    let mut copied = 0u64;
    let mut added = 0u64;
    let mut left = 0;
    let mut right = 0;
    while left < src.len() {
        while right < dst.len() && dst[right].0 < src[left].0 {
            added += u64::from(dst[right].1);
            right += 1;
        }
        let src_count = u64::from(src[left].1);
        let mut dst_count = 0;
        if right < dst.len() && dst[right].0 == src[left].0 {
            dst_count = u64::from(dst[right].1);
            right += 1;
        }
        if src_count < dst_count {
            added += dst_count - src_count;
            copied += src_count;
        } else {
            copied += dst_count;
        }
        left += 1;
    }
    while right < dst.len() {
        added += u64::from(dst[right].1);
        right += 1;
    }
    (copied, added)
}

fn exact_renames(base: &FileMap, side: &FileMap) -> Vec<(String, String)> {
    let deleted: Vec<(String, Oid)> = base
        .iter()
        .filter(|(path, _)| !side.contains_key(*path))
        .map(|(path, (_, oid))| (path.clone(), *oid))
        .collect();
    let added: Vec<(String, Oid)> = side
        .iter()
        .filter(|(path, _)| !base.contains_key(*path))
        .map(|(path, (_, oid))| (path.clone(), *oid))
        .collect();
    let mut pairs = Vec::new();
    for (path, oid) in &deleted {
        let sources = deleted.iter().filter(|(_, item)| item == oid).count();
        let dests: Vec<_> = added.iter().filter(|(_, item)| item == oid).collect();
        if sources == 1 && dests.len() == 1 {
            pairs.push((path.clone(), dests[0].0.clone()));
        }
    }
    pairs
}

/// 文件和目录不能占用同一个路径。返回互相挡住的路径。
pub fn directory_file_conflicts(files: &FileMap) -> Vec<String> {
    let paths: Vec<&str> = files.keys().map(String::as_str).collect();
    let mut bad = Vec::new();
    for path in &paths {
        let as_dir = format!("{path}/");
        if paths.iter().any(|other| other.starts_with(&as_dir)) {
            bad.push((*path).to_string());
        }
    }
    bad.sort();
    bad
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedText {
    pub bytes: Vec<u8>,
    pub conflict: bool,
}

#[derive(Clone, Debug)]
pub struct TextOptions {
    pub diff3: bool,
    /// 1 表示冲突块采用当前侧，-1 表示采用并入侧，0 表示写出冲突标记。
    pub favor: i8,
    pub base_label: String,
}

impl Default for TextOptions {
    fn default() -> Self {
        Self {
            diff3: false,
            favor: 0,
            base_label: "base".into(),
        }
    }
}

/// 行级三方合并，重叠判定与 Git `xdl_do_merge` 的默认层级一致：
/// 改动区间在祖先文件里不相接就两边都保留；相接或重叠则只标出冲突区域。
pub fn merge_text(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    our_label: &str,
    their_label: &str,
    options: &TextOptions,
) -> MergedText {
    if ours == theirs {
        return MergedText {
            bytes: ours.to_vec(),
            conflict: false,
        };
    }
    if is_binary(base) || is_binary(ours) || is_binary(theirs) {
        return MergedText {
            bytes: ours.to_vec(),
            conflict: true,
        };
    }
    if ours == base {
        return MergedText {
            bytes: theirs.to_vec(),
            conflict: false,
        };
    }
    if theirs == base {
        return MergedText {
            bytes: ours.to_vec(),
            conflict: false,
        };
    }
    let (base_lines, _) = split_lines(base);
    let (our_lines, _) = split_lines(ours);
    let (their_lines, _) = split_lines(theirs);
    let our_hunks = hunks(&base_lines, &our_lines);
    let their_hunks = hunks(&base_lines, &their_lines);
    let (lines, conflict) = render(
        &base_lines,
        &our_lines,
        &their_lines,
        &our_hunks,
        &their_hunks,
        our_label,
        their_label,
        options,
    );
    let mut bytes = lines.join("\n").into_bytes();
    if !bytes.is_empty() {
        bytes.push(b'\n');
    }
    MergedText { bytes, conflict }
}

/// 三方选择文件模式。只有一边相对祖先改了模式时采用那一边。
pub fn merge_mode(base: Option<u32>, ours: Option<u32>, theirs: Option<u32>) -> (u32, bool) {
    match (base, ours, theirs) {
        (_, Some(our), Some(their)) if our == their => (our, false),
        (Some(base), Some(our), Some(their)) if our == base => (their, false),
        (Some(base), Some(our), Some(their)) if their == base => (our, false),
        (_, Some(our), Some(_)) => (our, true),
        (_, Some(our), None) => (our, false),
        (_, None, Some(their)) => (their, false),
        _ => (0o100644, false),
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|byte| *byte == 0)
}

fn split_lines(bytes: &[u8]) -> (Vec<String>, bool) {
    if bytes.is_empty() {
        return (Vec::new(), true);
    }
    let text = String::from_utf8_lossy(bytes);
    let ended = text.ends_with('\n');
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    if ended {
        lines.pop();
    }
    (lines, ended)
}

#[derive(Clone, Debug)]
struct Hunk {
    base_lo: usize,
    base_hi: usize,
    side_lo: usize,
    side_hi: usize,
}

enum Edit {
    Eq,
    Del,
    Ins,
}

fn hunks(base: &[String], side: &[String]) -> Vec<Hunk> {
    let edits = edits(base, side);
    let mut out = Vec::new();
    let mut index = 0;
    let mut base_at = 0;
    let mut side_at = 0;
    while index < edits.len() {
        match edits[index] {
            Edit::Eq => {
                base_at += 1;
                side_at += 1;
                index += 1;
            }
            Edit::Del | Edit::Ins => {
                let base_lo = base_at;
                let side_lo = side_at;
                while index < edits.len() {
                    match edits[index] {
                        Edit::Eq => break,
                        Edit::Del => {
                            base_at += 1;
                            index += 1;
                        }
                        Edit::Ins => {
                            side_at += 1;
                            index += 1;
                        }
                    }
                }
                out.push(Hunk {
                    base_lo,
                    base_hi: base_at,
                    side_lo,
                    side_hi: side_at,
                });
            }
        }
    }
    out
}

fn edits(base: &[String], side: &[String]) -> Vec<Edit> {
    let n = base.len();
    let m = side.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if base[i] == side[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < n && j < m {
        if base[i] == side[j] {
            out.push(Edit::Eq);
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(Edit::Del);
            i += 1;
        } else {
            out.push(Edit::Ins);
            j += 1;
        }
    }
    while i < n {
        out.push(Edit::Del);
        i += 1;
    }
    while j < m {
        out.push(Edit::Ins);
        j += 1;
    }
    out
}

fn side_pos(hunks: &[Hunk], base_pos: usize) -> usize {
    let mut shift: isize = 0;
    for hunk in hunks {
        if hunk.base_hi <= base_pos {
            shift += hunk.side_hi as isize - hunk.side_lo as isize;
            shift -= hunk.base_hi as isize - hunk.base_lo as isize;
        } else {
            break;
        }
    }
    (base_pos as isize + shift) as usize
}

enum Take {
    Ours,
    Theirs,
    Conflict,
}

struct Region {
    base_lo: usize,
    base_hi: usize,
    take: Take,
}

fn render(
    base: &[String],
    ours: &[String],
    theirs: &[String],
    our_hunks: &[Hunk],
    their_hunks: &[Hunk],
    our_label: &str,
    their_label: &str,
    options: &TextOptions,
) -> (Vec<String>, bool) {
    let regions = regions(our_hunks, their_hunks);
    let mut lines = Vec::new();
    let mut cursor = 0;
    let mut conflict = false;
    for region in regions {
        lines.extend(base[cursor..region.base_lo].iter().cloned());
        let our_span = slice_side(ours, our_hunks, region.base_lo, region.base_hi);
        let their_span = slice_side(theirs, their_hunks, region.base_lo, region.base_hi);
        match region.take {
            Take::Ours => lines.extend(our_span),
            Take::Theirs => lines.extend(their_span),
            Take::Conflict if our_span == their_span => lines.extend(our_span),
            Take::Conflict if options.favor > 0 => lines.extend(our_span),
            Take::Conflict if options.favor < 0 => lines.extend(their_span),
            Take::Conflict => {
                conflict = true;
                lines.push(format!("<<<<<<< {our_label}"));
                lines.extend(our_span);
                if options.diff3 {
                    lines.push(format!("||||||| {}", options.base_label));
                    lines.extend(base[region.base_lo..region.base_hi].iter().cloned());
                }
                lines.push("=======".to_string());
                lines.extend(their_span);
                lines.push(format!(">>>>>>> {their_label}"));
            }
        }
        cursor = region.base_hi;
    }
    lines.extend(base[cursor..].iter().cloned());
    (lines, conflict)
}

fn slice_side(lines: &[String], hunks: &[Hunk], base_lo: usize, base_hi: usize) -> Vec<String> {
    let start = side_pos(hunks, base_lo).min(lines.len());
    let end = side_pos(hunks, base_hi).min(lines.len());
    if start <= end {
        lines[start..end].to_vec()
    } else {
        Vec::new()
    }
}

fn regions(ours: &[Hunk], theirs: &[Hunk]) -> Vec<Region> {
    let mut regions = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < ours.len() && j < theirs.len() {
        let our = &ours[i];
        let their = &theirs[j];
        if our.base_hi < their.base_lo {
            regions.push(Region {
                base_lo: our.base_lo,
                base_hi: our.base_hi,
                take: Take::Ours,
            });
            i += 1;
            continue;
        }
        if their.base_hi < our.base_lo {
            regions.push(Region {
                base_lo: their.base_lo,
                base_hi: their.base_hi,
                take: Take::Theirs,
            });
            j += 1;
            continue;
        }
        let mut base_lo = our.base_lo.min(their.base_lo);
        let mut base_hi = our.base_hi.max(their.base_hi);
        i += 1;
        j += 1;
        loop {
            let more_ours = i < ours.len() && ours[i].base_lo <= base_hi;
            let more_theirs = j < theirs.len() && theirs[j].base_lo <= base_hi;
            if !more_ours && !more_theirs {
                break;
            }
            if more_ours {
                base_lo = base_lo.min(ours[i].base_lo);
                base_hi = base_hi.max(ours[i].base_hi);
                i += 1;
            }
            if more_theirs {
                base_lo = base_lo.min(theirs[j].base_lo);
                base_hi = base_hi.max(theirs[j].base_hi);
                j += 1;
            }
        }
        regions.push(Region {
            base_lo,
            base_hi,
            take: Take::Conflict,
        });
    }
    while i < ours.len() {
        regions.push(Region {
            base_lo: ours[i].base_lo,
            base_hi: ours[i].base_hi,
            take: Take::Ours,
        });
        i += 1;
    }
    while j < theirs.len() {
        regions.push(Region {
            base_lo: theirs[j].base_lo,
            base_hi: theirs[j].base_hi,
            take: Take::Theirs,
        });
        j += 1;
    }
    regions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(n: u8) -> (u32, Oid) {
        let mut bytes = [0u8; 20];
        bytes[19] = n;
        (0o100644, Oid(bytes))
    }

    #[test]
    fn takes_one_sided_edit() {
        let mut base = FileMap::new();
        base.insert("a".into(), file(1));
        let mut ours = base.clone();
        ours.insert("a".into(), file(2));
        let theirs = base.clone();
        let merged = merge_maps(&base, &ours, &theirs);
        assert_eq!(merged["a"], MergeAction::Take(file(2).0, file(2).1));
    }

    #[test]
    fn conflicts_when_both_change_oids() {
        let mut base = FileMap::new();
        base.insert("a".into(), file(1));
        let mut ours = base.clone();
        ours.insert("a".into(), file(2));
        let mut theirs = base.clone();
        theirs.insert("a".into(), file(3));
        let merged = merge_maps(&base, &ours, &theirs);
        assert!(matches!(merged["a"], MergeAction::Conflict { .. }));
    }

    #[test]
    fn merges_separated_lines_and_marks_only_the_overlap() {
        let merged = merge_text(
            b"alpha\nkeep\nomega\n",
            b"ALPHA\nkeep\nomega\n",
            b"alpha\nkeep\nOMEGA\n",
            "HEAD",
            "theirs",
            &TextOptions::default(),
        );
        assert!(!merged.conflict);
        assert_eq!(merged.bytes, b"ALPHA\nkeep\nOMEGA\n");

        let clash = merge_text(
            b"one\n",
            b"ONE\n",
            b"TWO\n",
            "HEAD",
            "theirs",
            &TextOptions::default(),
        );
        assert!(clash.conflict);
        assert_eq!(
            clash.bytes,
            b"<<<<<<< HEAD\nONE\n=======\nTWO\n>>>>>>> theirs\n"
        );

        let adjacent = merge_text(
            b"a\nb\nc\n",
            b"A\nb\nc\n",
            b"a\nB\nc\n",
            "HEAD",
            "theirs",
            &TextOptions::default(),
        );
        assert!(adjacent.conflict);
        assert_eq!(
            adjacent.bytes,
            b"<<<<<<< HEAD\nA\nb\n=======\na\nB\n>>>>>>> theirs\nc\n"
        );

        let same = merge_text(
            b"same\n",
            b"BOTH\n",
            b"BOTH\n",
            "HEAD",
            "theirs",
            &TextOptions::default(),
        );
        assert!(!same.conflict);
        assert_eq!(same.bytes, b"BOTH\n");
    }

    #[test]
    fn keeps_ours_for_binary_conflict() {
        let merged = merge_text(
            b"a\0b\n",
            b"a\0B\n",
            b"A\0b\n",
            "HEAD",
            "theirs",
            &TextOptions::default(),
        );
        assert!(merged.conflict);
        assert_eq!(merged.bytes, b"a\0B\n");
    }

    #[test]
    fn moves_an_exact_rename_onto_the_other_edit() {
        let mut base = FileMap::new();
        base.insert("old.txt".into(), file(1));
        let mut ours = FileMap::new();
        ours.insert("new.txt".into(), file(1));
        let mut theirs = FileMap::new();
        theirs.insert("old.txt".into(), file(2));
        let conflicts = adjust_renames(&mut base, &mut ours, &mut theirs, &|_| Vec::new());
        assert!(conflicts.is_empty());
        assert!(!base.contains_key("old.txt"));
        assert_eq!(base["new.txt"], file(1));
        assert_eq!(theirs["new.txt"], file(2));
        let merged = merge_maps(&base, &ours, &theirs);
        assert_eq!(merged["new.txt"], MergeAction::Take(file(2).0, file(2).1));
    }
}
