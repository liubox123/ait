use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Write,
    Review,
    Deny,
}

impl Access {
    pub fn label(self) -> &'static str {
        match self {
            Access::Write => "可直接修改",
            Access::Review => "推送时需要评审",
            Access::Deny => "禁止修改",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rule {
    pub user: String,
    /// Git 风格路径，`**` 可跨目录。
    pub pattern: String,
    /// `write` 或 `deny`。没有命中写规则的路径，推送时进入评审。
    pub access: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Duty {
    pub user: String,
    pub summary: String,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Policy {
    pub owners: Vec<String>,
    pub rules: Vec<Rule>,
    pub duties: Vec<Duty>,
}

impl Policy {
    pub fn owner(user: &str) -> Self {
        Self {
            owners: vec![user.to_string()],
            rules: vec![Rule {
                user: user.to_string(),
                pattern: "**".into(),
                access: "write".into(),
            }],
            duties: vec![Duty {
                user: user.to_string(),
                summary: "仓库所有者".into(),
                paths: vec!["**".into()],
            }],
        }
    }

    pub fn is_owner(&self, user: &str, admin: bool) -> bool {
        admin || self.owners.iter().any(|owner| owner == user)
    }

    pub fn manifest_for(&self, user: &str, admin: bool) -> Manifest {
        let duty = self
            .duties
            .iter()
            .filter(|item| item.user == user)
            .map(|item| item.summary.as_str())
            .collect::<Vec<_>>()
            .join("；");
        if self.is_owner(user, admin) {
            return Manifest {
                user: user.to_string(),
                duty: if duty.is_empty() {
                    "仓库所有者".into()
                } else {
                    duty
                },
                write: vec!["**".into()],
                deny: Vec::new(),
            };
        }
        let write = self
            .rules
            .iter()
            .filter(|rule| user_matches(&rule.user, user) && rule.access == "write")
            .map(|rule| rule.pattern.clone())
            .collect();
        let deny = self
            .rules
            .iter()
            .filter(|rule| user_matches(&rule.user, user) && rule.access == "deny")
            .map(|rule| rule.pattern.clone())
            .collect();
        Manifest {
            user: user.to_string(),
            duty: if duty.is_empty() {
                "未分配职责，修改会进入评审".into()
            } else {
                duty
            },
            write,
            deny,
        }
    }
}

fn user_matches(rule_user: &str, user: &str) -> bool {
    rule_user == user || rule_user == "*"
}

/// 拉下来的修改权限清单。写权限之外的路径可以改，但推送时要等人评审。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub user: String,
    pub duty: String,
    pub write: Vec<String>,
    pub deny: Vec<String>,
}

impl Manifest {
    pub fn decide(&self, path: &str) -> Access {
        if self.deny.iter().any(|pattern| path_match(pattern, path)) {
            return Access::Deny;
        }
        if self.write.iter().any(|pattern| path_match(pattern, path)) {
            return Access::Write;
        }
        Access::Review
    }

    pub fn render(&self) -> String {
        let mut out = format!("权限清单（{}）\n职责: {}\n", self.user, self.duty);
        if self.write.is_empty() {
            out.push_str("可直接修改: （无，推送都会进入评审）\n");
        } else {
            out.push_str("可直接修改:\n");
            for pattern in &self.write {
                out.push_str(&format!("  {pattern}\n"));
            }
        }
        if !self.deny.is_empty() {
            out.push_str("禁止修改:\n");
            for pattern in &self.deny {
                out.push_str(&format!("  {pattern}\n"));
            }
        }
        out.push_str("其他路径可以在本地修改，推送时需要有权限的人评审。\n");
        out
    }
}

pub fn path_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim().trim_matches('/');
    let path = path.trim().trim_matches('/');
    if pattern.is_empty() || pattern == "**" {
        return !path.is_empty() || pattern == "**";
    }
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let segs: Vec<&str> = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').filter(|s| !s.is_empty()).collect()
    };
    match_parts(&pat, &segs)
}

fn match_parts(pat: &[&str], segs: &[&str]) -> bool {
    if pat.is_empty() {
        return segs.is_empty();
    }
    if pat[0] == "**" {
        if pat.len() == 1 {
            return true;
        }
        for index in 0..=segs.len() {
            if match_parts(&pat[1..], &segs[index..]) {
                return true;
            }
        }
        return false;
    }
    if segs.is_empty() || !segment_match(pat[0], segs[0]) {
        return false;
    }
    match_parts(&pat[1..], &segs[1..])
}

fn segment_match(pattern: &str, text: &str) -> bool {
    fn rec(pattern: &[u8], text: &[u8]) -> bool {
        if pattern.is_empty() {
            return text.is_empty();
        }
        if pattern[0] == b'*' {
            return rec(&pattern[1..], text) || (!text.is_empty() && rec(pattern, &text[1..]));
        }
        if pattern[0] == b'?' {
            return !text.is_empty() && rec(&pattern[1..], &text[1..]);
        }
        !text.is_empty() && pattern[0] == text[0] && rec(&pattern[1..], &text[1..])
    }
    rec(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_covers_directory_and_children() {
        assert!(path_match("src/auth/**", "src/auth/login.rs"));
        assert!(path_match("src/auth/**", "src/auth"));
        assert!(path_match("src/auth/**", "src/auth/oidc/mod.rs"));
        assert!(!path_match("src/auth/**", "src/bill/mod.rs"));
        assert!(path_match("**/*.rs", "src/main.rs"));
        assert!(!path_match("*.rs", "src/main.rs"));
        assert!(path_match("*.rs", "main.rs"));
    }

    #[test]
    fn deny_beats_write_and_owner_writes_all() {
        let policy = Policy {
            owners: vec!["alice".into()],
            rules: vec![
                Rule {
                    user: "bob".into(),
                    pattern: "src/shop/**".into(),
                    access: "write".into(),
                },
                Rule {
                    user: "bob".into(),
                    pattern: "src/shop/secret/**".into(),
                    access: "deny".into(),
                },
            ],
            duties: vec![Duty {
                user: "bob".into(),
                summary: "商品".into(),
                paths: vec!["src/shop/**".into()],
            }],
        };
        let bob = policy.manifest_for("bob", false);
        assert_eq!(bob.decide("src/shop/item.rs"), Access::Write);
        assert_eq!(bob.decide("src/shop/secret/key.rs"), Access::Deny);
        assert_eq!(bob.decide("src/pay/mod.rs"), Access::Review);
        let alice = policy.manifest_for("alice", false);
        assert_eq!(alice.decide("src/shop/secret/key.rs"), Access::Write);
    }
}
