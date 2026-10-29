#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Unmerged,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Unmerged => "unmerged",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Added => "新文件",
            ChangeKind::Modified => "修改",
            ChangeKind::Deleted => "删除",
            ChangeKind::Unmerged => "冲突",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PathChange {
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Clone, Debug)]
pub struct FileDiff {
    pub path: String,
    pub kind: ChangeKind,
    pub binary: bool,
    pub patch: String,
}

pub fn diff_bytes(path: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> FileDiff {
    let kind = match (old, new) {
        (None, Some(_)) => ChangeKind::Added,
        (Some(_), None) => ChangeKind::Deleted,
        _ => ChangeKind::Modified,
    };
    let binary = old.map(is_binary).unwrap_or(false) || new.map(is_binary).unwrap_or(false);
    if binary {
        return FileDiff {
            path: path.to_string(),
            kind,
            binary: true,
            patch: format!("diff --git a/{path} b/{path}\nBinary files differ\n"),
        };
    }
    let old_lines = split_lines(old.unwrap_or(b""));
    let new_lines = split_lines(new.unwrap_or(b""));
    let patch = if old_lines.len() * new_lines.len() > 1_500_000 {
        format!(
            "diff --git a/{path} b/{path}\n文件较大，省略逐行差异（{} -> {} 行）\n",
            old_lines.len(),
            new_lines.len()
        )
    } else {
        unified(
            path,
            &old_lines,
            &new_lines,
            !old.unwrap_or(b"").is_empty() && !old.unwrap_or(b"").ends_with(b"\n"),
            !new.unwrap_or(b"").is_empty() && !new.unwrap_or(b"").ends_with(b"\n"),
        )
    };
    FileDiff {
        path: path.to_string(),
        kind,
        binary: false,
        patch,
    }
}

pub fn unified_diff(files: &[FileDiff]) -> String {
    let mut out = String::new();
    for file in files {
        out.push_str(&file.patch);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

fn split_lines(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

#[derive(Clone, Copy)]
enum Edit {
    Eq(usize),
    Del(usize),
    Ins(usize),
}

fn unified(path: &str, old: &[String], new: &[String], old_no_nl: bool, new_no_nl: bool) -> String {
    let edits = edits(old, new);
    let mut hunks: Vec<Vec<(char, usize)>> = Vec::new();
    let mut current: Vec<(char, usize)> = Vec::new();
    let mut i = 0;
    while i < edits.len() {
        if let Edit::Eq(_) = edits[i] {
            let mut j = i;
            while j < edits.len() {
                if let Edit::Eq(_) = edits[j] {
                    j += 1;
                } else {
                    break;
                }
            }
            let eq_len = j - i;
            if current.is_empty() {
                if j < edits.len() {
                    let start = i + eq_len.saturating_sub(3);
                    for k in start..j {
                        if let Edit::Eq(n) = edits[k] {
                            current.push((' ', n));
                        }
                    }
                }
            } else if eq_len > 6 && j < edits.len() {
                for k in i..i + 3 {
                    if let Edit::Eq(n) = edits[k] {
                        current.push((' ', n));
                    }
                }
                hunks.push(std::mem::take(&mut current));
                let start = j - 3;
                for k in start..j {
                    if let Edit::Eq(n) = edits[k] {
                        current.push((' ', n));
                    }
                }
            } else if j >= edits.len() {
                for k in i..(i + 3).min(j) {
                    if let Edit::Eq(n) = edits[k] {
                        current.push((' ', n));
                    }
                }
                hunks.push(std::mem::take(&mut current));
            } else {
                for k in i..j {
                    if let Edit::Eq(n) = edits[k] {
                        current.push((' ', n));
                    }
                }
            }
            i = j;
        } else {
            match edits[i] {
                Edit::Del(n) => current.push(('-', n)),
                Edit::Ins(n) => current.push(('+', n)),
                Edit::Eq(_) => {}
            }
            i += 1;
        }
    }
    if !current.is_empty() {
        hunks.push(current);
    }

    let mut out = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n");
    if old.is_empty() && new.is_empty() {
        return out;
    }
    for hunk in hunks {
        if hunk.is_empty() {
            continue;
        }
        let mut old_start = 0usize;
        let mut new_start = 0usize;
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        let mut saw = false;
        for (mark, idx) in &hunk {
            match mark {
                ' ' | '-' => {
                    if !saw {
                        old_start = *idx;
                        saw = true;
                    }
                    old_count += 1;
                }
                _ => {}
            }
            match mark {
                ' ' | '+' => {
                    if new_count == 0 && *mark != '-' {
                        new_start = *idx;
                    }
                    new_count += 1;
                }
                _ => {}
            }
        }
        if old_count == 0 {
            old_start = 0;
        }
        if new_count == 0 {
            new_start = 0;
        }
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start + if old_count == 0 { 0 } else { 1 },
            old_count,
            new_start + if new_count == 0 { 0 } else { 1 },
            new_count
        ));
        for (mark, idx) in hunk {
            let line = if mark == '+' { &new[idx] } else { &old[idx] };
            out.push(mark);
            out.push_str(line);
            out.push('\n');
            if mark != '+' && idx + 1 == old.len() && old_no_nl && mark != '+' {
                out.push_str("\\ No newline at end of file\n");
            }
            if mark == '+' && idx + 1 == new.len() && new_no_nl {
                out.push_str("\\ No newline at end of file\n");
            }
        }
    }
    out
}

fn edits(a: &[String], b: &[String]) -> Vec<Edit> {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
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
        if a[i] == b[j] {
            out.push(Edit::Eq(i));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(Edit::Del(i));
            i += 1;
        } else {
            out.push(Edit::Ins(j));
            j += 1;
        }
    }
    while i < n {
        out.push(Edit::Del(i));
        i += 1;
    }
    while j < m {
        out.push(Edit::Ins(j));
        j += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_added_line() {
        let diff = diff_bytes("a.txt", Some(b"a\n"), Some(b"a\nb\n"));
        assert!(diff.patch.contains("+b"));
        assert!(diff.patch.contains("@@ "));
    }
}
