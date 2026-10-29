//! 行级对齐：给出新版本每一行在旧版本里对应的行号，供 blame 使用。

use std::collections::HashMap;

/// `result[j]` 是 `new[j]` 在 `old` 里对应的行号；新增或改过的行是 `None`。
pub fn match_lines(old: &[&[u8]], new: &[&[u8]]) -> Vec<Option<usize>> {
    let mut out = vec![None; new.len()];
    let mut start = 0;
    while start < old.len() && start < new.len() && old[start] == new[start] {
        out[start] = Some(start);
        start += 1;
    }
    let mut old_end = old.len();
    let mut new_end = new.len();
    while old_end > start && new_end > start && old[old_end - 1] == new[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
        out[new_end] = Some(old_end);
    }
    let a = &old[start..old_end];
    let b = &new[start..new_end];
    if a.is_empty() || b.is_empty() {
        return out;
    }
    let pairs = if a.len().saturating_mul(b.len()) <= 4_000_000 {
        lcs_pairs(a, b)
    } else {
        unique_pairs(a, b)
    };
    for (i, j) in pairs {
        out[start + j] = Some(start + i);
    }
    out
}

fn lcs_pairs(a: &[&[u8]], b: &[&[u8]]) -> Vec<(usize, usize)> {
    let mut ids = HashMap::new();
    let mut intern = |line: &[u8]| {
        let next = ids.len() as u32;
        *ids.entry(line.to_vec()).or_insert(next)
    };
    let a: Vec<u32> = a.iter().map(|line| intern(line)).collect();
    let b: Vec<u32> = b.iter().map(|line| intern(line)).collect();
    let n = a.len();
    let m = b.len();
    let width = m + 1;
    let mut dp = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * width + j] = if a[i] == b[j] {
                dp[(i + 1) * width + j + 1] + 1
            } else {
                dp[(i + 1) * width + j].max(dp[i * width + j + 1])
            };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * width + j] >= dp[i * width + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// 大文件退化为只对齐两边都只出现一次的行，再取最长递增子序列。
fn unique_pairs(a: &[&[u8]], b: &[&[u8]]) -> Vec<(usize, usize)> {
    let mut count: HashMap<&[u8], (usize, usize, usize)> = HashMap::new();
    for (i, line) in a.iter().enumerate() {
        let entry = count.entry(line).or_insert((0, 0, 0));
        entry.0 += 1;
        entry.2 = i;
    }
    for line in b {
        if let Some(entry) = count.get_mut(line) {
            entry.1 += 1;
        }
    }
    let mut candidates = Vec::new();
    for (j, line) in b.iter().enumerate() {
        if let Some((1, 1, i)) = count.get(line).copied() {
            candidates.push((i, j));
        }
    }
    let mut tails: Vec<usize> = Vec::new();
    let mut prev = vec![usize::MAX; candidates.len()];
    for (k, (i, _)) in candidates.iter().enumerate() {
        let pos = tails.partition_point(|&t| candidates[t].0 < *i);
        if pos > 0 {
            prev[k] = tails[pos - 1];
        }
        if pos == tails.len() {
            tails.push(k);
        } else {
            tails[pos] = k;
        }
    }
    let mut out = Vec::new();
    let mut cursor = tails.last().copied();
    while let Some(k) = cursor {
        out.push(candidates[k]);
        cursor = (prev[k] != usize::MAX).then_some(prev[k]);
    }
    out.reverse();
    out
}

pub fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    if bytes.ends_with(b"\n") {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_unchanged_lines() {
        let old = split_lines(b"a\nb\nc\nd\n");
        let new = split_lines(b"a\nx\nc\nd\ne\n");
        assert_eq!(match_lines(&old, &new), vec![Some(0), None, Some(2), Some(3), None]);
    }
}
