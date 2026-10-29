use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

struct Server {
    child: Child,
    origin: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_dir() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("ait-gov-{}", ait::identity::random_token()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn must(bin: &str, home: &Path, cwd: &Path, args: &[&str]) -> String {
    let output = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .env("AIT_HOME", home)
        .env("AIT_PASSWORD", "secret")
        .env("AIT_AUTHOR_NAME", "Tester")
        .env("AIT_AUTHOR_EMAIL", "tester@example.com")
        .output()
        .expect("启动 ait");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "命令 {args:?} 失败\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

fn start_server(bin: &str, data: &Path) -> Server {
    let mut child = Command::new(bin)
        .args([
            "serve",
            "--data",
            &data.to_string_lossy(),
            "--bind",
            "127.0.0.1:0",
        ])
        .env_remove("AIT_CHECK_RUNNER")
        .env_remove("AIT_LLM_BASE_URL")
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("启动服务");
    let stderr = child.stderr.take().expect("stderr");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let line = line.unwrap_or_default();
            if let Some(index) = line.find("http://") {
                let _ = tx.send(line[index..].trim().trim_end_matches('/').to_string());
            }
        }
    });
    let origin = rx.recv_timeout(Duration::from_secs(15)).expect("服务地址");
    for _ in 0..100 {
        if reqwest::blocking::get(format!("{origin}/api/health")).is_ok() {
            return Server { child, origin };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("服务没有就绪");
}

struct Api {
    origin: String,
    token: String,
}

impl Api {
    fn login(origin: &str, user: &str) -> Self {
        let value: Value = reqwest::blocking::Client::new()
            .post(format!("{origin}/api/login"))
            .json(&json!({ "name": user, "password": "secret" }))
            .send()
            .unwrap()
            .json()
            .unwrap();
        Self {
            origin: origin.to_string(),
            token: value["token"].as_str().expect("token").to_string(),
        }
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let client = reqwest::blocking::Client::new();
        let url = format!("{}{path}", self.origin);
        let mut request = client
            .request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), url)
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().unwrap();
        let status = response.status().as_u16();
        let text = response.text().unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.call(method, path, body);
        assert!(
            (200..300).contains(&status),
            "{method} {path} -> {status}: {value}"
        );
        value
    }
}

#[test]
fn limits_checks_ai_review_and_claims_gate_a_pull_request() {
    let bin = env!("CARGO_BIN_EXE_ait");
    let root = temp_dir();
    let server = start_server(bin, &root.join("data"));
    let origin = server.origin.clone();
    let homes: Vec<PathBuf> = ["alice", "bob", "carol"]
        .iter()
        .map(|user| {
            let home = root.join(format!("{user}-home"));
            fs::create_dir_all(&home).unwrap();
            must(
                bin,
                &home,
                &root,
                &["register", "--server", &origin, "--user", user],
            );
            home
        })
        .collect();
    let (alice_home, bob_home) = (&homes[0], &homes[1]);

    must(bin, alice_home, &root, &["repo", "create", "lab"]);
    let lab = root.join("lab");
    must(bin, alice_home, &root, &["init", lab.to_str().unwrap()]);
    fs::create_dir_all(lab.join("src")).unwrap();
    fs::create_dir_all(lab.join("docs")).unwrap();
    fs::create_dir_all(lab.join(".ait")).unwrap();
    fs::write(lab.join("src/a.rs"), "fn a() {}\n").unwrap();
    fs::write(lab.join("docs/guide.md"), "# 指南\n").unwrap();
    fs::write(
        lab.join(".ait/checks.toml"),
        "[[check]]\nname = \"unit\"\nrun = \"cargo test\"\nrequired = true\npaths = [\"src/**\"]\n",
    )
    .unwrap();
    must(bin, alice_home, &lab, &["add"]);
    must(bin, alice_home, &lab, &["commit", "-m", "初始"]);
    must(
        bin,
        alice_home,
        &lab,
        &["remote", "add", "origin", &format!("{origin}/r/lab")],
    );
    must(bin, alice_home, &lab, &["push"]);

    let alice = Api::login(&origin, "alice");
    let bob = Api::login(&origin, "bob");
    let carol = Api::login(&origin, "carol");

    let plan = json!({
        "rules": [
            { "user": "alice", "pattern": "**", "access": "write" },
            { "user": "bob", "pattern": "src/**", "access": "write" }
        ],
        "duties": [
            { "user": "alice", "summary": "所有者", "paths": ["**"] },
            { "user": "bob", "summary": "核心模块", "paths": ["src/**"] }
        ],
        "members": [
            { "user": "bob", "kind": "agent", "supervisor": "alice", "limits": { "max_files": 1 } },
            { "user": "carol", "kind": "ci" }
        ]
    });
    let preview = alice.ok("POST", "/api/repos/lab/policy/preview", Some(plan.clone()));
    let src = preview["dirs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|dir| dir["path"] == "src")
        .expect("src 目录");
    assert_eq!(src["writers"], json!(["bob"]));
    let warnings = preview["warnings"].to_string();
    assert!(warnings.contains("docs/"), "{warnings}");
    let (status, _) = bob.call("PUT", "/api/repos/lab/policy", Some(plan.clone()));
    assert_eq!(status, 403, "非所有者不能改规划");
    alice.ok("PUT", "/api/repos/lab/policy", Some(plan));
    let manifest = bob.ok("GET", "/api/repos/lab/manifest", None);
    assert_eq!(manifest["kind"], "agent");
    assert_eq!(manifest["limits"]["max_files"], 1);

    let bob_lab = root.join("bob-lab");
    must(
        bin,
        bob_home,
        &root,
        &[
            "clone",
            &format!("{origin}/r/lab"),
            bob_lab.to_str().unwrap(),
        ],
    );
    must(bin, bob_home, &bob_lab, &["checkout", "-b", "feature"]);
    fs::write(bob_lab.join("src/a.rs"), "fn a() { b() }\n").unwrap();
    fs::write(bob_lab.join("src/b.rs"), "fn b() {}\n").unwrap();
    must(bin, bob_home, &bob_lab, &["add"]);
    must(bin, bob_home, &bob_lab, &["commit", "-m", "拆出 b"]);
    must(bin, bob_home, &bob_lab, &["push"]);

    let alice_claim = alice.ok(
        "POST",
        "/api/repos/lab/claims",
        Some(json!({ "paths": ["src/b.rs"], "intent": "设计 b 的接口" })),
    );
    let bob_claim = bob.ok(
        "POST",
        "/api/repos/lab/claims",
        Some(json!({ "paths": ["src/**"], "intent": "拆分模块", "branch": "feature" })),
    );
    assert_eq!(bob_claim["overlaps"][0]["user"], "alice", "{bob_claim}");

    let pr = bob.ok(
        "POST",
        "/api/repos/lab/pulls",
        Some(json!({ "title": "拆出 b", "base": "main", "head": "feature" })),
    );
    let id = pr["id"].as_str().unwrap().to_string();
    assert!(!pr["limit_reasons"].as_array().unwrap().is_empty(), "{pr}");
    assert_eq!(
        pr["blocked"].as_array().unwrap().len(),
        2,
        "超出限制后所有改动都要别人通过"
    );
    assert!(pr["checks_blocking"].to_string().contains("unit"), "{pr}");
    assert_eq!(pr["author_kind"], "agent");
    assert_eq!(pr["can_merge"], false);

    let ai = bob.ok(
        "POST",
        &format!("/api/repos/lab/pulls/{id}/ai-review"),
        Some(json!({})),
    );
    let rules: Vec<&str> = ai["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["rule"].as_str())
        .collect();
    assert!(
        rules.contains(&"limits") && rules.contains(&"claim"),
        "{ai}"
    );
    assert_eq!(ai["verdict"], "comment");

    let head = pr["head_oid"].as_str().unwrap().to_string();
    let reported = bob.ok(
        "POST",
        "/api/repos/lab/checks",
        Some(json!({ "reference": head, "name": "unit", "status": "success" })),
    );
    assert_eq!(reported["trusted"], false);
    let view = bob.ok("GET", "/api/repos/lab/pulls", None);
    assert!(
        view[0]["checks_blocking"].to_string().contains("unit"),
        "agent 自报不能放行"
    );

    let (status, _) = bob.call(
        "POST",
        &format!("/api/repos/lab/pulls/{id}/approve"),
        Some(json!({})),
    );
    assert_eq!(status, 403, "不能通过自己的合并请求");
    alice.ok(
        "POST",
        &format!("/api/repos/lab/pulls/{id}/approve"),
        Some(json!({})),
    );
    let (status, body) = alice.call(
        "POST",
        &format!("/api/repos/lab/pulls/{id}/merge"),
        Some(json!({})),
    );
    assert_eq!(status, 409, "校验没过不能合并: {body}");

    let reported = carol.ok(
        "POST",
        "/api/repos/lab/checks",
        Some(json!({ "reference": head, "name": "unit", "status": "success", "summary": "12 passed" })),
    );
    assert_eq!(reported["trusted"], true);
    let checks = bob.ok(
        "GET",
        &format!("/api/repos/lab/checks?reference={head}"),
        None,
    );
    assert_eq!(checks["gates"][0]["status"], "success", "{checks}");
    let merged = alice.ok(
        "POST",
        &format!("/api/repos/lab/pulls/{id}/merge"),
        Some(json!({})),
    );
    assert_eq!(merged["status"], "merged");

    let (status, _) = bob.call(
        "DELETE",
        &format!(
            "/api/repos/lab/claims/{}",
            alice_claim["claim"]["id"].as_str().unwrap()
        ),
        None,
    );
    assert_eq!(status, 403, "不能结束别人的工作意图");
    let events = alice.ok("GET", "/api/repos/lab/events", None);
    let kinds = events.to_string();
    for kind in ["\"pull\"", "\"check\"", "\"claim\"", "\"ai\"", "\"policy\""] {
        assert!(kinds.contains(kind), "缺少动态 {kind}: {kinds}");
    }

    let ai_cfg = alice.ok(
        "PUT",
        "/api/repos/lab/ai",
        Some(json!({ "enabled": true, "auto": true, "base_url": "http://127.0.0.1:9/v1", "model": "m", "api_key": "sk-test" })),
    );
    assert_eq!(ai_cfg["has_api_key"], true);
    assert!(ai_cfg.get("api_key").is_none(), "密钥不能读回");
    let (status, _) = bob.call(
        "PUT",
        "/api/repos/lab/ai",
        Some(json!({ "enabled": false })),
    );
    assert_eq!(status, 403);

    drop(server);
    let _ = fs::remove_dir_all(root);
}
