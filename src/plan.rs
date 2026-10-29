use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Deserialize;

use crate::perm::{Duty, Policy, Rule};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub user: String,
    pub duty: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AssignmentFile {
    assignments: Vec<AssignmentJson>,
}

#[derive(Debug, Deserialize)]
struct AssignmentJson {
    user: String,
    duty: String,
    paths: Vec<String>,
}

/// 从规划文本里识别职责和目录。
///
/// 支持两种写法：
///
/// ```text
/// ## 账户 (@alice)
/// 职责: 登录与会话
/// 目录: src/account/**, src/session/**
/// ```
///
/// 以及一行一条：`@alice src/account/** 登录与会话`。
/// 若文本里有 JSON（可放在 ```json 代码块中），优先使用 JSON。
pub fn parse_plan(text: &str) -> anyhow::Result<Vec<Assignment>> {
    if let Some(items) = parse_embedded_json(text) {
        if !items.is_empty() {
            return Ok(items);
        }
    }
    let mut items = Vec::new();
    let mut current: Option<Assignment> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            if let Some(item) = current.take() {
                items.push(item);
            }
            let title = trimmed.trim_start_matches('#').trim();
            if let Some(user) = user_in(title) {
                current = Some(Assignment {
                    user,
                    duty: strip_user(title),
                    paths: Vec::new(),
                });
            }
            continue;
        }
        if let Some(item) = parse_at_line(trimmed) {
            if let Some(pending) = current.take() {
                items.push(pending);
            }
            items.push(item);
            continue;
        }
        if let Some(current) = current.as_mut() {
            if let Some(rest) = strip_prefix_label(trimmed, &["职责", "责任", "duty"]) {
                if !current.duty.is_empty() && !rest.is_empty() {
                    current.duty.push('；');
                }
                if !rest.is_empty() {
                    current.duty.push_str(rest);
                }
            } else if let Some(rest) = strip_prefix_label(trimmed, &["目录", "路径", "paths"]) {
                current.paths.extend(split_paths(rest));
            }
        }
    }
    if let Some(item) = current {
        items.push(item);
    }
    items.retain(|item| valid_user(&item.user));
    if items.is_empty() {
        bail!("没有识别出职责。请使用「## 模块 (@用户)」加「目录:」或「@用户 路径 职责」");
    }
    Ok(items)
}

pub fn policy_from_assignments(owners: &[String], items: &[Assignment]) -> Policy {
    let mut rules = Vec::new();
    let mut duties = Vec::new();
    for owner in owners {
        rules.push(Rule {
            user: owner.clone(),
            pattern: "**".into(),
            access: "write".into(),
        });
    }
    let mut grouped: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for item in items {
        let entry = grouped.entry(item.user.clone()).or_default();
        if !item.duty.is_empty() {
            if !entry.0.is_empty() {
                entry.0.push('；');
            }
            entry.0.push_str(&item.duty);
        }
        for path in &item.paths {
            if !entry.1.iter().any(|have| have == path) {
                entry.1.push(path.clone());
            }
        }
    }
    for owner in owners {
        if !grouped.contains_key(owner) {
            duties.push(Duty {
                user: owner.clone(),
                summary: "仓库所有者".into(),
                paths: vec!["**".into()],
            });
        }
    }
    for (user, (duty, paths)) in grouped {
        duties.push(Duty {
            user: user.clone(),
            summary: if duty.is_empty() {
                "未填写职责".into()
            } else {
                duty
            },
            paths: paths.clone(),
        });
        for path in paths {
            rules.push(Rule {
                user: user.clone(),
                pattern: path,
                access: "write".into(),
            });
        }
    }
    Policy {
        owners: owners.to_vec(),
        rules,
        duties,
        members: Vec::new(),
    }
}

pub fn assignments_from_llm(plan: &str, users: &[String]) -> anyhow::Result<Vec<Assignment>> {
    let base = std::env::var("AIT_LLM_BASE_URL").context("未配置环境变量 AIT_LLM_BASE_URL")?;
    let key = std::env::var("AIT_LLM_API_KEY").unwrap_or_default();
    let model = std::env::var("AIT_LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let known = if users.is_empty() {
        "（规划文本里的 @用户）".to_string()
    } else {
        users.join(", ")
    };
    let body = serde_json::json!({
        "model": model,
        "temperature": 0,
        "messages": [
            {
                "role": "system",
                "content": "你是仓库权限规划器。根据项目规划，为参与者分配目录写权限和一句职责。只输出 JSON，不要解释。格式：{\"assignments\":[{\"user\":\"名字\",\"duty\":\"职责\",\"paths\":[\"glob\"]}]}。目录使用 git glob，目录请以 /** 结尾。只使用已知用户。不要把整个仓库 ** 分给非所有者，除非规划明确要求。"
            },
            {
                "role": "user",
                "content": format!("已知用户：{known}\n\n规划：\n{plan}")
            }
        ]
    });
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()?;
    let mut request = client.post(url).json(&body);
    if !key.is_empty() {
        request = request.bearer_auth(key);
    }
    let response = request.send().context("调用模型接口失败")?;
    let status = response.status();
    let text = response.text().context("读取模型响应失败")?;
    if !status.is_success() {
        bail!("模型接口返回 {status}: {text}");
    }
    let value: serde_json::Value = serde_json::from_str(&text).context("模型响应不是 JSON")?;
    let content = value["choices"][0]["message"]["content"]
        .as_str()
        .context("模型没有返回文本")?;
    let items = parse_embedded_json(content).context("模型输出里没有合法的 assignments JSON")?;
    if items.is_empty() {
        bail!("模型没有给出任何职责分配");
    }
    Ok(items)
}

fn parse_embedded_json(text: &str) -> Option<Vec<Assignment>> {
    let fenced = text
        .split("```")
        .enumerate()
        .filter_map(|(index, part)| if index % 2 == 1 { Some(part) } else { None })
        .find_map(|part| {
            let part = part.trim_start_matches("json").trim();
            parse_json_assignments(part)
        });
    if fenced.is_some() {
        return fenced;
    }
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        return parse_json_assignments(trimmed);
    }
    if let Some(start) = text.find("{\"assignments\"") {
        return parse_json_assignments(&text[start..]);
    }
    None
}

fn parse_json_assignments(text: &str) -> Option<Vec<Assignment>> {
    let file: AssignmentFile = serde_json::from_str(text).ok()?;
    let items = file
        .assignments
        .into_iter()
        .filter(|item| valid_user(&item.user))
        .map(|item| Assignment {
            user: item.user,
            duty: item.duty,
            paths: item
                .paths
                .into_iter()
                .map(|path| path.trim().trim_matches('/').to_string())
                .filter(|path| !path.is_empty())
                .collect(),
        })
        .collect::<Vec<_>>();
    Some(items)
}

fn parse_at_line(line: &str) -> Option<Assignment> {
    let rest = line.strip_prefix('@')?;
    let mut parts = rest.split_whitespace();
    let user = parts.next()?.to_string();
    if !valid_user(&user) {
        return None;
    }
    let path = parts.next()?.trim_matches('/').to_string();
    if path.is_empty() {
        return None;
    }
    let duty = parts.collect::<Vec<_>>().join(" ");
    Some(Assignment {
        user,
        duty,
        paths: vec![path],
    })
}

fn user_in(title: &str) -> Option<String> {
    let at = title.rfind('@')?;
    let name: String = title[at + 1..]
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        .collect();
    if valid_user(&name) {
        Some(name)
    } else {
        None
    }
}

fn strip_user(title: &str) -> String {
    let mut text = title.to_string();
    if let Some(at) = text.rfind('@') {
        text.truncate(at);
    }
    text.replace(['(', ')', '（', '）'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_prefix_label<'a>(line: &'a str, labels: &[&str]) -> Option<&'a str> {
    let lower = line.to_ascii_lowercase();
    for label in labels {
        let label_lower = label.to_ascii_lowercase();
        if lower.starts_with(&label_lower) {
            let rest = &line[label.len()..];
            return Some(rest.trim().trim_start_matches([':', '：']).trim());
        }
    }
    None
}

fn split_paths(text: &str) -> Vec<String> {
    text.split([',', '，', ';', '；', '、'])
        .map(|part| part.trim().trim_matches('/'))
        .filter(|part| !part.is_empty() && !part.contains(' '))
        .map(|part| part.to_string())
        .collect()
}

fn valid_user(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_and_compact_lines() {
        let text = r#"
# 商城

## 账户 (@alice)
职责: 登录、注册与会话
目录: src/account/**, src/session/**

## 商品 (@bob)
职责: 商品与库存
目录: src/catalog/**

@carol web/** 页面与样式
"#;
        let items = parse_plan(text).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].user, "alice");
        assert_eq!(items[0].paths, vec!["src/account/**", "src/session/**"]);
        assert!(items[0].duty.contains("登录"));
        assert_eq!(items[2].user, "carol");
        assert_eq!(items[2].paths, vec!["web/**"]);
        let policy = policy_from_assignments(&["owner".into()], &items);
        let bob = policy.manifest_for("bob", false);
        assert!(bob.write.iter().any(|path| path == "src/catalog/**"));
        assert_eq!(
            bob.decide("src/catalog/item.rs"),
            crate::perm::Access::Write
        );
        assert_eq!(
            bob.decide("src/account/login.rs"),
            crate::perm::Access::Review
        );
    }
}
