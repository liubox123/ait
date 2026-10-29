use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use ait::git::Repository;
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

fn temp_dir(prefix: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("{prefix}-{}", ait::identity::random_token()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn run(bin: &str, home: &Path, cwd: &Path, args: &[&str]) -> (bool, String, String) {
    run_env(bin, home, cwd, args, &[])
}

fn run_env(
    bin: &str,
    home: &Path,
    cwd: &Path,
    args: &[&str],
    extra: &[(&str, &str)],
) -> (bool, String, String) {
    let mut command = Command::new(bin);
    command
        .args(args)
        .current_dir(cwd)
        .env("AIT_HOME", home)
        .env("AIT_PASSWORD", "secret")
        .env("AIT_AUTHOR_NAME", "Tester")
        .env("AIT_AUTHOR_EMAIL", "tester@example.com");
    for (key, value) in extra {
        command.env(key, value);
    }
    let output = command.output().expect("启动 ait");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn must(bin: &str, home: &Path, cwd: &Path, args: &[&str]) -> String {
    let (ok, stdout, stderr) = run(bin, home, cwd, args);
    assert!(
        ok,
        "命令 {args:?} 失败\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
}

fn must_fail(bin: &str, home: &Path, cwd: &Path, args: &[&str]) -> String {
    let (ok, stdout, stderr) = run(bin, home, cwd, args);
    assert!(ok == false, "命令 {args:?} 应该失败\nstdout:\n{stdout}");
    format!("{stdout}\n{stderr}")
}

fn start_server(bin: &str, data: &Path) -> Server {
    let data_arg = data.to_string_lossy().to_string();
    let mut child = Command::new(bin)
        .args(["serve", "--data", &data_arg, "--bind", "127.0.0.1:0"])
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
                let origin = line[index..].trim().trim_end_matches('/').to_string();
                let _ = tx.send(origin);
            }
        }
    });
    let origin = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("服务没有打印地址");
    for _ in 0..50 {
        if health(&origin) {
            return Server { child, origin };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("服务没有在 {origin} 就绪");
}

fn health(origin: &str) -> bool {
    let (status, body) = http("GET", origin, "/api/health", None, None);
    status == 200 && body.contains("\"ok\":true")
}

fn http(
    method: &str,
    origin: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let host = origin.trim_start_matches("http://");
    let mut stream = TcpStream::connect(host).unwrap_or_else(|err| panic!("连接 {host}: {err}"));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let payload = body.unwrap_or("");
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if body.is_some() {
        request.push_str("Content-Type: application/json\r\n");
        request.push_str(&format!("Content-Length: {}\r\n", payload.len()));
    }
    request.push_str("\r\n");
    request.push_str(payload);
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    let (head, content) = raw.split_once("\r\n\r\n").unwrap_or(("", raw.as_str()));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, content.to_string())
}

fn token(home: &Path) -> String {
    let text = fs::read_to_string(home.join("credentials.json")).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    value["servers"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

fn parents(dir: &Path) -> usize {
    let repo = Repository::discover(dir).unwrap();
    let oid = repo.rev_parse("HEAD").unwrap();
    repo.read_commit(oid).unwrap().parents.len()
}

#[test]
fn local_branch_merge_conflict_and_dirty_tree() {
    let bin = env!("CARGO_BIN_EXE_ait");
    let root = temp_dir("ait-local");
    let home = root.join("home");
    let repo = root.join("repo");
    fs::create_dir_all(&home).unwrap();

    must(bin, &home, &root, &["init", repo.to_str().unwrap()]);
    let empty = must_fail(bin, &home, &repo, &["commit", "-m", "空"]);
    assert!(empty.contains("没有需要提交的内容"), "{empty}");

    fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    fs::create_dir_all(repo.join("target")).unwrap();
    fs::write(repo.join("target/skip.log"), "noise\n").unwrap();
    fs::write(repo.join("note.txt"), "base\n").unwrap();
    must(bin, &home, &repo, &["add"]);
    let status = must(bin, &home, &repo, &["status"]);
    assert!(status.contains("note.txt"), "{status}");
    assert!(!status.contains("skip.log"), "{status}");
    let cached = must(bin, &home, &repo, &["diff", "--cached"]);
    assert!(cached.contains("+base"), "{cached}");

    let committed = must(bin, &home, &repo, &["commit", "-m", "基础"]);
    assert!(committed.contains("基础"), "{committed}");
    let clean = must(bin, &home, &repo, &["status"]);
    assert!(clean.contains("干净"), "{clean}");

    let named = must(bin, &home, &repo, &["config", "user.name", "Ada"]);
    assert!(named.contains("Ada"), "{named}");

    must(bin, &home, &repo, &["branch", "side"]);
    let branches = must(bin, &home, &repo, &["branch"]);
    assert!(branches.contains("* main"), "{branches}");
    assert!(branches.contains("side"), "{branches}");

    must(bin, &home, &repo, &["checkout", "-b", "feature"]);
    fs::write(repo.join("note.txt"), "feature\n").unwrap();
    must(bin, &home, &repo, &["commit", "-a", "-m", "功能线"]);
    must(bin, &home, &repo, &["switch", "main"]);
    fs::write(repo.join("note.txt"), "main\n").unwrap();
    must(bin, &home, &repo, &["commit", "-a", "-m", "主线"]);

    let conflict = must_fail(bin, &home, &repo, &["merge", "feature"]);
    assert!(conflict.contains("有冲突"), "{conflict}");
    let marked = fs::read_to_string(repo.join("note.txt")).unwrap();
    assert!(marked.contains("<<<<<<< HEAD"), "{marked}");
    assert!(marked.contains("feature"), "{marked}");
    let blocked = must_fail(bin, &home, &repo, &["commit", "-m", "还没解决"]);
    assert!(blocked.contains("冲突标记"), "{blocked}");

    fs::write(repo.join("note.txt"), "both\n").unwrap();
    must(bin, &home, &repo, &["add", "note.txt"]);
    let merged = must(bin, &home, &repo, &["commit", "-m", "合并两边"]);
    assert!(merged.contains("合并两边"), "{merged}");
    assert_eq!(parents(&repo), 2);
    assert_eq!(fs::read_to_string(repo.join("note.txt")).unwrap(), "both\n");

    let history = must(bin, &home, &repo, &["log", "-n", "5"]);
    assert!(history.contains("合并两边"), "{history}");
    assert!(history.contains("主线"), "{history}");
    let head = must(bin, &home, &repo, &["rev-parse", "HEAD"]);
    let main = must(bin, &home, &repo, &["rev-parse", "main"]);
    assert_eq!(head.trim(), main.trim());

    fs::write(repo.join("note.txt"), "dirty\n").unwrap();
    let dirty = must_fail(bin, &home, &repo, &["switch", "feature"]);
    assert!(dirty.contains("本地修改"), "{dirty}");
    fs::write(repo.join("note.txt"), "both\n").unwrap();

    let current = must_fail(bin, &home, &repo, &["branch", "-d", "main"]);
    assert!(current.contains("当前所在分支"), "{current}");
    must(bin, &home, &repo, &["switch", "feature"]);
    let removed = must(bin, &home, &repo, &["branch", "-d", "side"]);
    assert!(removed.contains("已删除分支 side"), "{removed}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn permissions_reviews_force_api_and_mcp() {
    let bin = env!("CARGO_BIN_EXE_ait");
    let root = temp_dir("ait-full");
    let server = start_server(bin, &root.join("data"));
    let origin = &server.origin;
    let alice = root.join("alice-home");
    let bob = root.join("bob-home");
    let carol = root.join("carol-home");
    for home in [&alice, &bob, &carol] {
        fs::create_dir_all(home).unwrap();
    }

    must(
        bin,
        &alice,
        &root,
        &["register", "--server", origin, "--user", "alice"],
    );
    let dup = must_fail(
        bin,
        &alice,
        &root,
        &["register", "--server", origin, "--user", "alice"],
    );
    assert!(dup.contains("已存在"), "{dup}");
    let (bad_login, _, bad_err) = run_env(
        bin,
        &alice,
        &root,
        &["login", "--server", origin, "--user", "alice"],
        &[("AIT_PASSWORD", "wrong")],
    );
    assert!(!bad_login, "{bad_err}");
    assert!(bad_err.contains("密码"), "{bad_err}");

    must(
        bin,
        &bob,
        &root,
        &["register", "--server", origin, "--user", "bob"],
    );
    must(
        bin,
        &carol,
        &root,
        &["register", "--server", origin, "--user", "carol"],
    );
    must(
        bin,
        &alice,
        &root,
        &["repo", "create", "shop", "--description", "商城"],
    );
    let listed = must(bin, &bob, &root, &["repo", "list", "--server", origin]);
    assert!(listed.contains("shop"), "{listed}");

    let shop = root.join("shop");
    must(bin, &alice, &root, &["init", shop.to_str().unwrap()]);
    fs::create_dir_all(shop.join("src/account")).unwrap();
    fs::create_dir_all(shop.join("src/catalog")).unwrap();
    fs::write(shop.join("src/account/login.rs"), "account v1\n").unwrap();
    fs::write(shop.join("src/catalog/item.rs"), "item v1\n").unwrap();
    fs::write(shop.join("README.md"), "readme v1\n").unwrap();
    must(bin, &alice, &shop, &["add"]);
    must(bin, &alice, &shop, &["commit", "-m", "初始"]);
    must(
        bin,
        &alice,
        &shop,
        &["remote", "add", "origin", &format!("{origin}/r/shop")],
    );
    let pushed = must(bin, &alice, &shop, &["push"]);
    assert!(pushed.contains("已更新"), "{pushed}");

    let (status, blob) = http(
        "GET",
        origin,
        "/api/repos/shop/blob?path=README.md",
        Some(&token(&alice)),
        None,
    );
    assert_eq!(status, 200, "{blob}");
    assert!(blob.contains("readme v1"), "{blob}");
    let (open_status, open_body) = http(
        "GET",
        origin,
        "/api/repos/shop/blob?path=README.md",
        None,
        None,
    );
    assert_eq!(open_status, 401, "{open_body}");
    let (tree_status, tree) = http(
        "GET",
        origin,
        "/api/repos/shop/tree?path=",
        Some(&token(&alice)),
        None,
    );
    assert_eq!(tree_status, 200, "{tree}");
    assert!(tree.contains("README.md"), "{tree}");
    assert!(tree.contains("src"), "{tree}");

    let plan = root.join("plan.md");
    fs::write(
        &plan,
        "## 账户 (@alice)\n职责: 登录\n目录: src/account/**\n\n## 商品 (@bob)\n职责: 商品\n目录: src/catalog/**\n",
    )
    .unwrap();
    let applied = must(
        bin,
        &alice,
        &shop,
        &["plan", "apply", "--file", plan.to_str().unwrap()],
    );
    assert!(applied.contains("src/catalog/**"), "{applied}");
    let shown = must(bin, &alice, &shop, &["plan", "show"]);
    assert!(shown.contains("商品"), "{shown}");

    let bob_shop = root.join("bob-shop");
    let cloned = must(
        bin,
        &bob,
        &root,
        &[
            "clone",
            &format!("{origin}/r/shop"),
            bob_shop.to_str().unwrap(),
        ],
    );
    assert!(cloned.contains("src/catalog/**"), "{cloned}");
    fs::write(bob_shop.join("src/catalog/item.rs"), "item v2\n").unwrap();
    must(bin, &bob, &bob_shop, &["commit", "-a", "-m", "商品二"]);
    let own = must(bin, &bob, &bob_shop, &["push"]);
    assert!(own.contains("已更新"), "{own}");
    let pulled = must(bin, &alice, &shop, &["pull"]);
    assert!(pulled.contains("快进"), "{pulled}");
    assert_eq!(
        fs::read_to_string(shop.join("src/catalog/item.rs")).unwrap(),
        "item v2\n"
    );

    let (policy_status, policy_body) = http(
        "GET",
        origin,
        "/api/repos/shop/policy",
        Some(&token(&alice)),
        None,
    );
    assert_eq!(policy_status, 200, "{policy_body}");
    let mut policy: Value = serde_json::from_str(&policy_body).unwrap();
    policy["rules"].as_array_mut().unwrap().push(json!({
        "user": "bob",
        "pattern": "secrets/**",
        "access": "deny"
    }));
    let (put_status, put_body) = http(
        "PUT",
        origin,
        "/api/repos/shop/policy",
        Some(&token(&alice)),
        Some(&policy.to_string()),
    );
    assert_eq!(put_status, 200, "{put_body}");

    let bob_deny = root.join("bob-deny");
    must(
        bin,
        &bob,
        &root,
        &[
            "clone",
            &format!("{origin}/r/shop"),
            bob_deny.to_str().unwrap(),
        ],
    );
    let perm = must(bin, &bob, &bob_deny, &["perm"]);
    assert!(perm.contains("secrets/**"), "{perm}");
    fs::create_dir_all(bob_deny.join("secrets")).unwrap();
    fs::write(bob_deny.join("secrets/key.txt"), "secret\n").unwrap();
    must(bin, &bob, &bob_deny, &["add", "secrets/key.txt"]);
    must(bin, &bob, &bob_deny, &["commit", "-m", "密钥"]);
    let denied = must_fail(bin, &bob, &bob_deny, &["push"]);
    assert!(denied.contains("禁止修改"), "{denied}");
    assert!(denied.contains("secrets/key.txt"), "{denied}");

    let refreshed = must(bin, &bob, &bob_shop, &["pull"]);
    assert!(
        refreshed.contains("最新") || refreshed.contains("快进"),
        "{refreshed}"
    );
    fs::write(bob_shop.join("src/account/login.rs"), "account v2\n").unwrap();
    let preview = must(bin, &bob, &bob_shop, &["status"]);
    assert!(preview.contains("src/account/login.rs"), "{preview}");
    assert!(preview.contains("评审"), "{preview}");
    must(bin, &bob, &bob_shop, &["commit", "-a", "-m", "越权登录"]);
    let review_push = must(bin, &bob, &bob_shop, &["push"]);
    assert!(review_push.contains("评审"), "{review_push}");
    let reviews = must(bin, &alice, &shop, &["review", "list"]);
    let id = reviews.split_whitespace().next().unwrap();
    assert!(id.starts_with("r-"), "{reviews}");

    let carol_shop = root.join("carol-shop");
    must(
        bin,
        &carol,
        &root,
        &[
            "clone",
            &format!("{origin}/r/shop"),
            carol_shop.to_str().unwrap(),
        ],
    );
    let carol_denied = must_fail(bin, &carol, &carol_shop, &["review", "approve", id]);
    assert!(carol_denied.contains("写权限"), "{carol_denied}");

    let rejected = must(
        bin,
        &alice,
        &shop,
        &["review", "reject", id, "--reason", "先别改登录"],
    );
    assert!(rejected.contains("已拒绝"), "{rejected}");
    let again = must_fail(bin, &alice, &shop, &["review", "approve", id]);
    assert!(
        again.contains("rejected") || again.contains("已是"),
        "{again}"
    );
    must(bin, &alice, &shop, &["fetch"]);
    let (login_status, login_blob) = http(
        "GET",
        origin,
        "/api/repos/shop/blob?path=src/account/login.rs",
        Some(&token(&alice)),
        None,
    );
    assert_eq!(login_status, 200, "{login_blob}");
    assert!(login_blob.contains("account v1"), "{login_blob}");
    assert!(!login_blob.contains("account v2"), "{login_blob}");
    assert_eq!(
        fs::read_to_string(shop.join("src/account/login.rs")).unwrap(),
        "account v1\n"
    );
    assert!(!shop.join("secrets/key.txt").exists());

    let second = must(bin, &bob, &bob_shop, &["push"]);
    assert!(second.contains("评审"), "{second}");
    let reviews = must(bin, &alice, &shop, &["review", "list"]);
    let id = reviews
        .lines()
        .find(|line| line.contains("pending"))
        .and_then(|line| line.split_whitespace().next())
        .expect(&reviews);
    let approved = must(bin, &alice, &shop, &["review", "approve", id]);
    assert!(approved.contains("已通过"), "{approved}");
    must(bin, &alice, &shop, &["pull"]);
    assert_eq!(
        fs::read_to_string(shop.join("src/account/login.rs")).unwrap(),
        "account v2\n"
    );
    must(bin, &bob, &bob_shop, &["pull"]);

    mcp_commit_and_propose(bin, &bob, &bob_shop);
    let reviews = must(bin, &alice, &shop, &["review", "list"]);
    let id = reviews
        .lines()
        .find(|line| line.contains("pending"))
        .and_then(|line| line.split_whitespace().next())
        .expect(&reviews);
    must(bin, &alice, &shop, &["review", "approve", id]);
    must(bin, &bob, &bob_shop, &["pull"]);
    must(bin, &alice, &shop, &["pull"]);
    assert_eq!(
        fs::read_to_string(shop.join("src/catalog/note.txt")).unwrap(),
        "note\n"
    );

    fs::write(shop.join("README.md"), "alice readme\n").unwrap();
    must(bin, &alice, &shop, &["commit", "-a", "-m", "说明"]);
    fs::write(bob_shop.join("src/catalog/item.rs"), "item v3\n").unwrap();
    must(bin, &bob, &bob_shop, &["commit", "-a", "-m", "商品三"]);
    let bob_ok = must(bin, &bob, &bob_shop, &["push"]);
    assert!(bob_ok.contains("已更新"), "{bob_ok}");
    let not_ff = must_fail(bin, &alice, &shop, &["push"]);
    assert!(not_ff.contains("不是快进"), "{not_ff}");
    let forced = must_fail(bin, &bob, &bob_shop, &["push", "--force"]);
    assert!(forced.contains("所有者"), "{forced}");
    let owner_force = must(bin, &alice, &shop, &["push", "--force"]);
    assert!(owner_force.contains("已更新"), "{owner_force}");
    let (blob_status, blob) = http(
        "GET",
        origin,
        "/api/repos/shop/blob?path=README.md",
        Some(&token(&alice)),
        None,
    );
    assert_eq!(blob_status, 200, "{blob}");
    assert!(blob.contains("alice readme"), "{blob}");

    let merged = must(bin, &bob, &bob_shop, &["pull"]);
    assert!(merged.contains("已合并"), "{merged}");
    assert_eq!(
        fs::read_to_string(bob_shop.join("README.md")).unwrap(),
        "alice readme\n"
    );
    assert_eq!(
        fs::read_to_string(bob_shop.join("src/catalog/item.rs")).unwrap(),
        "item v3\n"
    );
    let published = must(bin, &bob, &bob_shop, &["push"]);
    assert!(published.contains("已更新"), "{published}");
    must(bin, &alice, &shop, &["pull"]);
    assert_eq!(
        fs::read_to_string(shop.join("src/catalog/item.rs")).unwrap(),
        "item v3\n"
    );
    assert_eq!(
        fs::read_to_string(shop.join("README.md")).unwrap(),
        "alice readme\n"
    );

    fs::write(shop.join("README.md"), "alice readme v2\n").unwrap();
    fs::write(shop.join("src/catalog/item.rs"), "item alice\n").unwrap();
    must(bin, &alice, &shop, &["commit", "-a", "-m", "两处都改"]);
    must(bin, &alice, &shop, &["push"]);
    fs::write(bob_shop.join("src/catalog/item.rs"), "item bob\n").unwrap();
    must(bin, &bob, &bob_shop, &["commit", "-a", "-m", "商品四"]);
    mcp_merge_keeps_incoming_paths(bin, &bob, &bob_shop);
    must(bin, &alice, &shop, &["pull"]);
    assert_eq!(
        fs::read_to_string(shop.join("src/catalog/item.rs")).unwrap(),
        "item merged\n"
    );
    assert_eq!(
        fs::read_to_string(shop.join("README.md")).unwrap(),
        "alice readme v2\n"
    );

    let _ = fs::remove_dir_all(root);
}

fn mcp_merge_keeps_incoming_paths(bin: &str, home: &Path, dir: &Path) {
    let mut child = Command::new(bin)
        .args(["mcp", "--repo", dir.to_str().unwrap()])
        .current_dir(std::env::temp_dir())
        .env("AIT_HOME", home)
        .env("AIT_AUTHOR_NAME", "Tester")
        .env("AIT_AUTHOR_EMAIL", "tester@example.com")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut call = |id: i64, name: &str, arguments: Value| {
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        });
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
        read_mcp(&mut stdout)
    };
    let text = |value: &Value| {
        value["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };

    let rejected = call(1, "push", json!({}));
    assert!(text(&rejected).contains("不是快进"), "{rejected}");
    let pulled = call(2, "pull", json!({}));
    assert_eq!(pulled["result"]["isError"], true, "{pulled}");
    assert!(text(&pulled).contains("src/catalog/item.rs"), "{pulled}");
    let readme = call(3, "check_paths", json!({"paths": ["README.md"]}));
    assert!(text(&readme).contains("review"), "{readme}");

    fs::write(dir.join("src/catalog/item.rs"), "item merged\n").unwrap();
    let committed = call(
        4,
        "commit",
        json!({"message": "合并 origin/main", "paths": ["src/catalog/item.rs"]}),
    );
    assert_eq!(committed["result"]["isError"], false, "{committed}");
    let pushed = call(5, "push", json!({}));
    assert!(text(&pushed).contains("已更新"), "{pushed}");
    let reviews = call(6, "review", json!({"action": "list"}));
    assert_eq!(reviews["result"]["isError"], false, "{reviews}");

    let _ = child.kill();
    let _ = child.wait();
}

fn mcp_commit_and_propose(bin: &str, home: &Path, dir: &Path) {
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(dir)
        .env("AIT_HOME", home)
        .env("AIT_AUTHOR_NAME", "Tester")
        .env("AIT_AUTHOR_EMAIL", "tester@example.com")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut call = |id: i64, name: &str, arguments: Value| {
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        });
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
        read_mcp(&mut stdout)
    };

    let checked = call(
        1,
        "check_paths",
        json!({"paths": ["src/catalog/note.txt", "src/account/login.rs", "secrets/key.txt"]}),
    );
    let text = checked["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("write"), "{checked}");
    assert!(text.contains("review"), "{checked}");
    assert!(text.contains("deny"), "{checked}");
    assert_eq!(checked["result"]["isError"], false);

    fs::write(dir.join("src/catalog/note.txt"), "note\n").unwrap();
    let committed = call(
        2,
        "commit",
        json!({"message": "目录说明", "paths": ["src/catalog/note.txt"]}),
    );
    assert_eq!(committed["result"]["isError"], false, "{committed}");
    let pushed = call(3, "push", json!({}));
    let pushed_text = pushed["result"]["content"][0]["text"].as_str().unwrap();
    assert!(pushed_text.contains("已更新"), "{pushed}");

    fs::write(dir.join("src/account/login.rs"), "account v3\n").unwrap();
    let refused = call(
        4,
        "commit",
        json!({"message": "越权", "paths": ["src/account/login.rs"]}),
    );
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    let refused_text = refused["result"]["content"][0]["text"].as_str().unwrap();
    assert!(refused_text.contains("propose"), "{refused}");

    let proposed = call(
        5,
        "propose",
        json!({"message": "申请改登录", "paths": ["src/account/login.rs"]}),
    );
    assert_eq!(proposed["result"]["isError"], false, "{proposed}");
    let proposed_text = proposed["result"]["content"][0]["text"].as_str().unwrap();
    assert!(proposed_text.contains("评审"), "{proposed}");

    let _ = child.kill();
    let _ = child.wait();
}

fn read_mcp(reader: &mut impl BufRead) -> Value {
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
