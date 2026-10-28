//! 给 agent 用的 MCP 服务。权限内的修改可以直接提交；越权修改必须走 `propose`，推送后等人工评审。

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::Context;
use serde_json::{json, Value};

use crate::git::{unified_diff, Repository};
use crate::identity::load_manifest;
use crate::perm::{path_match, Access};
use crate::remote;

pub fn serve(repo: Option<PathBuf>) -> anyhow::Result<()> {
    let repo = match repo {
        Some(path) => Repository::discover(&path)
            .or_else(|_| Repository::open(&path))
            .ok(),
        None => std::env::current_dir()
            .ok()
            .and_then(|dir| Repository::discover(&dir).ok()),
    };
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout();
    loop {
        let Some(message) = read_message(&mut reader)? else {
            break;
        };
        if let Some(response) = dispatch(repo.as_ref(), &message) {
            write_message(&mut stdout, &response)?;
        }
    }
    Ok(())
}

fn dispatch(repo: Option<&Repository>, message: &Value) -> Option<Value> {
    let Some(id) = message.get("id").cloned() else {
        return None;
    };
    let method = message
        .get("method")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "ait", "version": env!("CARGO_PKG_VERSION") }
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_specs() })),
        "tools/call" => match call_tool(repo, &params) {
            Ok(text) => Ok(tool_result(&text, false)),
            Err(err) => Ok(tool_result(&format!("{err:#}"), true)),
        },
        _ => Err(format!("未知方法 {method}")),
    };
    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err(error) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": error } })
        }
    })
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error
    })
}

fn tool_specs() -> Vec<Value> {
    vec![
        tool(
            "status",
            "查看分支、暂存、未暂存文件，以及哪些文件超出当前用户的写权限。",
            json!({"type":"object","properties":{}}),
        ),
        tool(
            "diff",
            "查看统一差异。cached 为 true 时比较暂存区和 HEAD。",
            json!({"type":"object","properties":{"cached":{"type":"boolean"}}}),
        ),
        tool(
            "log",
            "查看最近提交。",
            json!({"type":"object","properties":{"limit":{"type":"integer"}}}),
        ),
        tool(
            "permissions",
            "读取拉取时下发的修改权限清单和本人职责。",
            json!({"type":"object","properties":{}}),
        ),
        tool(
            "check_paths",
            "判断路径是可直接修改、需要评审，还是禁止修改。写代码前先调用。",
            json!({"type":"object","properties":{"paths":{"type":"array","items":{"type":"string"}}},"required":["paths"]}),
        ),
        tool(
            "commit",
            "只提交写权限之内的文件。若包含越权文件会失败，请改用 propose。",
            json!({"type":"object","properties":{"message":{"type":"string"},"paths":{"type":"array","items":{"type":"string"}},"all":{"type":"boolean"}},"required":["message"]}),
        ),
        tool(
            "propose",
            "提交并推送越权修改，生成人工评审，不直接进入分支。禁止修改的路径仍会失败。",
            json!({"type":"object","properties":{"message":{"type":"string"},"paths":{"type":"array","items":{"type":"string"}}},"required":["message"]}),
        ),
        tool(
            "push",
            "推送当前分支。越权提交会返回评审编号。",
            json!({"type":"object","properties":{"force":{"type":"boolean"}}}),
        ),
        tool(
            "plan",
            "读取项目规划，以及每个人负责的目录。",
            json!({"type":"object","properties":{}}),
        ),
        tool(
            "owners",
            "查询某个路径由谁负责。修改共享代码前先看同事和别的 agent 的职责。",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        ),
    ]
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": schema })
}

fn call_tool(repo: Option<&Repository>, params: &Value) -> anyhow::Result<String> {
    let name = params
        .get("name")
        .and_then(|value| value.as_str())
        .context("缺少工具名")?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let repo = repo.context("当前目录不是 ait/git 仓库")?;
    match name {
        "status" => {
            let status = repo.status()?;
            Ok(serde_json::to_string_pretty(&json!({
                "branch": status.branch,
                "head": status.head.map(|oid| oid.to_string()),
                "staged": status.staged.iter().map(|entry| json!({"path": entry.path, "kind": entry.kind.as_str(), "access": access_name(repo, &entry.path)})).collect::<Vec<_>>(),
                "unstaged": status.unstaged.iter().map(|entry| json!({"path": entry.path, "kind": entry.kind.as_str(), "access": access_name(repo, &entry.path)})).collect::<Vec<_>>(),
                "untracked": status.untracked,
            }))?)
        }
        "diff" => {
            let cached = args.get("cached").and_then(|value| value.as_bool()).unwrap_or(false);
            let files = if cached { repo.diff_staged()? } else { repo.diff_unstaged()? };
            let text = unified_diff(&files);
            Ok(if text.is_empty() { "没有差异".into() } else { text })
        }
        "log" => {
            let limit = args.get("limit").and_then(|value| value.as_u64()).unwrap_or(20) as usize;
            let Some(head) = repo.head_oid()? else {
                return Ok("还没有提交".into());
            };
            let commits: Vec<_> = repo
                .log(head, limit)?
                .into_iter()
                .map(|commit| {
                    json!({
                        "oid": commit.oid.to_string(),
                        "summary": commit.summary,
                        "author": commit.author_name,
                        "time": commit.time,
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&commits)?)
        }
        "permissions" => match load_manifest(repo.git_dir())? {
            Some(manifest) => Ok(manifest.render()),
            None => Ok("还没有权限清单。请先 ait pull。未连接服务端时，本地提交不会被拦住，推送时服务端仍会检查。".into()),
        },
        "check_paths" => {
            let paths = string_list(&args, "paths")?;
            let rows: Vec<_> = paths
                .into_iter()
                .map(|path| json!({"path": path, "access": access_name(repo, &path)}))
                .collect();
            Ok(serde_json::to_string_pretty(&rows)?)
        }
        "commit" => commit_tool(repo, &args, false),
        "propose" => commit_tool(repo, &args, true),
        "push" => {
            let force = args.get("force").and_then(|value| value.as_bool()).unwrap_or(false);
            let response = remote::push_repo(repo, force)?;
            Ok(format!("{}\n{}", response.message, response.manifest.render()))
        }
        "plan" => {
            let text = remote::plan_text(repo).unwrap_or_default();
            let policy = remote::policy(repo)?;
            Ok(serde_json::to_string_pretty(&json!({
                "plan": text,
                "owners": policy.owners,
                "duties": policy.duties,
            }))?)
        }
        "owners" => {
            let path = args.get("path").and_then(|value| value.as_str()).context("缺少 path")?.to_string();
            let policy = remote::policy(repo)?;
            let duties: Vec<_> = policy
                .duties
                .iter()
                .filter(|duty| duty.paths.iter().any(|pattern| pattern != "**" && path_match(pattern, &path)))
                .cloned()
                .collect();
            Ok(serde_json::to_string_pretty(&json!({
                "path": path,
                "owners": policy.owners,
                "duties": duties,
            }))?)
        }
        other => anyhow::bail!("未知工具 {other}"),
    }
}

fn commit_tool(repo: &Repository, args: &Value, propose: bool) -> anyhow::Result<String> {
    let message = args
        .get("message")
        .and_then(|value| value.as_str())
        .context("缺少 message")?
        .to_string();
    let paths = args
        .get("paths")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let all = args
        .get("all")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !paths.is_empty() {
        repo.add(&paths)?;
    }
    let status = repo.status()?;
    let take_unstaged = all || (propose && paths.is_empty());
    let mut committing: Vec<String> = status
        .staged
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    if take_unstaged {
        for entry in &status.unstaged {
            if !committing.iter().any(|path| path == &entry.path) {
                committing.push(entry.path.clone());
            }
        }
    }
    if committing.is_empty() {
        anyhow::bail!("没有需要提交的内容");
    }
    let mut reviews = Vec::new();
    for path in &committing {
        match decide(repo, path) {
            Access::Deny => anyhow::bail!("{path} 被禁止修改"),
            Access::Review if !propose => {
                anyhow::bail!("{path} 不在你的写权限内。请不要把它留在这次提交里；如果确实要改，改用 propose，推送后会进入人工评审")
            }
            Access::Review => reviews.push(path.clone()),
            Access::Write => {}
        }
    }
    let (name, email) = crate::identity::load_identity();
    let oid = repo.commit(
        &message,
        take_unstaged,
        &crate::identity::signature_now(&name, &email),
    )?;
    if propose {
        let response = remote::push_repo(repo, false)?;
        return Ok(format!(
            "已提交 {} 并推送。{}\n越权路径: {}\n{}",
            oid.short(),
            response.message,
            if reviews.is_empty() {
                "无".into()
            } else {
                reviews.join(", ")
            },
            response.manifest.render()
        ));
    }
    Ok(format!(
        "已提交 {}。这些文件都在写权限内，可以用 push 直接进入分支。",
        oid.short()
    ))
}

fn decide(repo: &Repository, path: &str) -> Access {
    match load_manifest(repo.git_dir()) {
        Ok(Some(manifest)) => manifest.decide(path),
        _ => Access::Write,
    }
}

fn access_name(repo: &Repository, path: &str) -> &'static str {
    match decide(repo, path) {
        Access::Write => "write",
        Access::Review => "review",
        Access::Deny => "deny",
    }
}

fn string_list(args: &Value, key: &str) -> anyhow::Result<Vec<String>> {
    let items = args
        .get(key)
        .and_then(|value| value.as_array())
        .context("缺少路径列表")?;
    Ok(items
        .iter()
        .filter_map(|item| item.as_str().map(|s| s.to_string()))
        .collect())
}

fn read_message(reader: &mut impl BufRead) -> anyhow::Result<Option<Value>> {
    let mut line = String::new();
    let count = reader.read_line(&mut line)?;
    if count == 0 {
        return Ok(None);
    }
    if line.trim_start().starts_with('{') {
        return Ok(Some(serde_json::from_str(line.trim())?));
    }
    let mut content_length = None;
    loop {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(rest.trim().parse::<usize>()?);
        }
        line.clear();
        let count = reader.read_line(&mut line)?;
        if count == 0 {
            return Ok(None);
        }
    }
    let length = content_length.context("MCP 消息缺少 Content-Length")?;
    let mut buffer = vec![0u8; length];
    reader.read_exact(&mut buffer)?;
    Ok(Some(serde_json::from_slice(&buffer)?))
}

fn write_message(stdout: &mut impl Write, value: &Value) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(stdout, "Content-Length: {}\r\n\r\n", body.len())?;
    stdout.write_all(&body)?;
    stdout.flush()?;
    Ok(())
}
