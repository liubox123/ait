use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::json;

fn temp_dir() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("ait-mcp-{}", ait::identity::random_token()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn read_message(reader: &mut impl BufRead) -> serde_json::Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    if line.trim_start().starts_with('{') {
        return serde_json::from_str(line.trim()).unwrap();
    }
    let mut length = 0usize;
    loop {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            length = rest.trim().parse().unwrap();
        }
        line.clear();
        reader.read_line(&mut line).unwrap();
    }
    let mut buffer = vec![0u8; length];
    reader.read_exact(&mut buffer).unwrap();
    serde_json::from_slice(&buffer).unwrap()
}

#[test]
fn mcp_lists_permission_tools_and_reports_status() {
    let bin = env!("CARGO_BIN_EXE_ait");
    let dir = temp_dir();
    let init = Command::new(bin)
        .args(["init", dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    for args in [vec!["add"], vec!["commit", "-m", "readme"]] {
        let output = Command::new(bin)
            .args(&args)
            .current_dir(&dir)
            .env("AIT_AUTHOR_NAME", "Tester")
            .env("AIT_AUTHOR_EMAIL", "tester@example.com")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let mut call = |id: i64, method: &str, params: serde_json::Value| {
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
        read_message(&mut stdout)
    };

    let init = call(
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "ait", "{init}");
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18", "{init}");
    let tools = call(2, "tools/list", json!({}));
    let names = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    for required in [
        "status",
        "check_paths",
        "commit",
        "propose",
        "permissions",
        "owners",
        "pull",
        "merge",
        "branch",
        "checkout",
        "pr",
        "review",
        "restore",
    ] {
        assert!(
            names.iter().any(|name| name == required),
            "缺少工具 {required}: {names:?}"
        );
    }
    let status = call(3, "tools/call", json!({"name": "status", "arguments": {}}));
    let text = status["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(text.contains("\"branch\": \"main\""), "{status}");
    assert_eq!(status["result"]["isError"], false);
    let log = call(
        4,
        "tools/call",
        json!({"name": "log", "arguments": {"limit": 5}}),
    );
    let history = log["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(history.contains("readme"), "{log}");

    let text_of = |value: &serde_json::Value| {
        value["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    let created = call(
        5,
        "tools/call",
        json!({"name": "checkout", "arguments": {"target": "side", "create": true}}),
    );
    assert_eq!(created["result"]["isError"], false, "{created}");
    fs::write(dir.join("side.txt"), "side\n").unwrap();
    let committed = call(
        6,
        "tools/call",
        json!({"name": "commit", "arguments": {"message": "side", "paths": ["side.txt"]}}),
    );
    assert_eq!(committed["result"]["isError"], false, "{committed}");
    let back = call(
        7,
        "tools/call",
        json!({"name": "checkout", "arguments": {"target": "main"}}),
    );
    assert_eq!(back["result"]["isError"], false, "{back}");
    let merged = call(
        8,
        "tools/call",
        json!({"name": "merge", "arguments": {"branch": "side"}}),
    );
    assert!(text_of(&merged).contains("快进"), "{merged}");
    assert_eq!(fs::read_to_string(dir.join("side.txt")).unwrap(), "side\n");
    let branches = call(9, "tools/call", json!({"name": "branch", "arguments": {}}));
    assert!(text_of(&branches).contains("\"side\""), "{branches}");

    fs::write(dir.join("README.md"), "changed\n").unwrap();
    fs::write(dir.join("extra.txt"), "extra\n").unwrap();
    let staged = Command::new(bin)
        .args(["add", "README.md", "extra.txt"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(staged.status.success());
    let unstaged = call(
        10,
        "tools/call",
        json!({"name": "restore", "arguments": {"paths": ["extra.txt"]}}),
    );
    assert_eq!(unstaged["result"]["isError"], false, "{unstaged}");
    assert!(dir.join("extra.txt").exists());
    let reverted = call(
        11,
        "tools/call",
        json!({"name": "restore", "arguments": {"paths": ["README.md"], "worktree": true}}),
    );
    assert_eq!(reverted["result"]["isError"], false, "{reverted}");
    assert_eq!(
        fs::read_to_string(dir.join("README.md")).unwrap(),
        "hello\n"
    );
    let after = call(12, "tools/call", json!({"name": "status", "arguments": {}}));
    let after_text = text_of(&after);
    assert!(after_text.contains("\"staged\": []"), "{after}");
    assert!(after_text.contains("extra.txt"), "{after}");

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(dir);
}
