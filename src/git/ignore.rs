/// 根目录 `.gitignore` 的一个够用子集：注释、`!`、目录斜杠、`*`、`?`、`**`。

#[derive(Clone, Debug)]
struct Rule {
    negated: bool,
    dir_only: bool,
    pattern: String,
    has_slash: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Ignore {
    rules: Vec<Rule>,
}

impl Ignore {
    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let negated = line.starts_with('!');
            let line = line.trim_start_matches('!');
            let dir_only = line.ends_with('/');
            let line = line.trim_start_matches('/').trim_end_matches('/');
            if line.is_empty() {
                continue;
            }
            rules.push(Rule {
                negated,
                dir_only,
                has_slash: line.contains('/'),
                pattern: line.to_string(),
            });
        }
        Ignore { rules }
    }

    pub fn ignored(&self, path: &str, is_dir: bool) -> bool {
        let path = path.trim_matches('/');
        if path.is_empty() {
            return false;
        }
        let mut ignored = false;
        for rule in &self.rules {
            if rule.matches(path, is_dir) {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

impl Rule {
    fn matches(&self, path: &str, is_dir: bool) -> bool {
        if self.has_slash {
            if crate::perm::path_match(&self.pattern, path) {
                return !self.dir_only || is_dir;
            }
            let mut parent = path;
            while let Some((pre, _)) = parent.rsplit_once('/') {
                if crate::perm::path_match(&self.pattern, pre) {
                    return true;
                }
                parent = pre;
            }
            return false;
        }
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let Some(last) = parts.last() else {
            return false;
        };
        if glob_segment(&self.pattern, last) && (!self.dir_only || is_dir) {
            return true;
        }
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            if glob_segment(&self.pattern, part) {
                return true;
            }
        }
        false
    }
}

fn glob_segment(pattern: &str, text: &str) -> bool {
    fn rec(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        if p[0] == b'*' {
            return rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..]));
        }
        if p[0] == b'?' {
            return !t.is_empty() && rec(&p[1..], &t[1..]);
        }
        !t.is_empty() && p[0] == t[0] && rec(&p[1..], &t[1..])
    }
    rec(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_target_and_keeps_negation() {
        let ig = Ignore::parse("target/\n*.log\n!keep.log\n");
        assert!(ig.ignored("target", true));
        assert!(ig.ignored("target/a.rs", false));
        assert!(ig.ignored("src/debug.log", false));
        assert!(!ig.ignored("src/keep.log", false));
        assert!(!ig.ignored("src/lib.rs", false));
    }
}
