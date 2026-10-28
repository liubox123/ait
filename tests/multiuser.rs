use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

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
    path.push(format!("ait-users-{}", ait::identity::random_token()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn run(bin: &str, home: &Path, cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .env("AIT_HOME", home)
        .env("AIT_PASSWORD", "secret")
        .env("AIT_AUTHOR_NAME", "Tester")
        .env("AIT_AUTHOR_EMAIL", "tester@example.com")
        .output()
        .expect("启动 ait");
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
    assert!(!ok, "命令 {args:?} 应该失败\nstdout:\n{stdout}");
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
    let host = origin.trim_start_matches("http://");
    let Ok(mut stream) = std::net::TcpStream::connect(host) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = write!(
        stream,
        "GET /api/health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    );
    let mut body = String::new();
    let _ = std::io::Read::read_to_string(&mut stream, &mut body);
    body.contains("\"ok\":true") || body.contains("\"ok\": true")
}

fn open_pr(list: &str, branch: &str) -> String {
    list.lines()
        .find(|line| line.contains(branch) && line.contains("open"))
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or_else(|| panic!("没有找到 {branch} 的打开合并请求:\n{list}"))
        .to_string()
}

#[test]
fn three_users_submit_review_and_merge_pull_requests() {
    let bin = env!("CARGO_BIN_EXE_ait");
    let root = temp_dir();
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

    let shop = root.join("shop");
    must(bin, &alice, &root, &["init", shop.to_str().unwrap()]);
    fs::create_dir_all(shop.join("src/account")).unwrap();
    fs::create_dir_all(shop.join("src/catalog")).unwrap();
    fs::create_dir_all(shop.join("web")).unwrap();
    fs::write(shop.join("src/account/login.rs"), "account v1\n").unwrap();
    fs::write(shop.join("src/catalog/item.rs"), "item v1\n").unwrap();
    fs::write(shop.join("web/index.html"), "page v1\n").unwrap();
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

    let plan = root.join("plan.md");
    fs::write(
        &plan,
        "## 账户 (@alice)\n职责: 登录\n目录: src/account/**\n\n## 商品 (@bob)\n职责: 商品\n目录: src/catalog/**\n\n## 页面 (@carol)\n职责: 页面\n目录: web/**\n",
    )
    .unwrap();
    must(
        bin,
        &alice,
        &shop,
        &["plan", "apply", "--file", plan.to_str().unwrap()],
    );

    let bob_shop = root.join("bob-shop");
    let carol_shop = root.join("carol-shop");
    must(
        bin,
        &bob,
        &root,
        &[
            "clone",
            &format!("{origin}/r/shop"),
            bob_shop.to_str().unwrap(),
        ],
    );
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

    must(bin, &bob, &bob_shop, &["checkout", "-b", "bob/price"]);
    fs::write(bob_shop.join("src/catalog/item.rs"), "item v2\n").unwrap();
    must(bin, &bob, &bob_shop, &["commit", "-a", "-m", "调整价格"]);
    let branch_push = must(bin, &bob, &bob_shop, &["push"]);
    assert!(branch_push.contains("已更新"), "{branch_push}");
    assert!(
        !branch_push.contains("已提交评审"),
        "分支推送应该先落在自己的分支上:\n{branch_push}"
    );
    let created = must(bin, &bob, &bob_shop, &["pr", "create", "调整价格"]);
    assert!(created.contains("已创建合并请求"), "{created}");
    assert!(created.contains("bob/price"), "{created}");
    let listed = must(bin, &alice, &shop, &["pr", "list"]);
    let price = open_pr(&listed, "bob/price");
    let outsider = must_fail(bin, &carol, &carol_shop, &["pr", "merge", &price]);
    assert!(
        outsider.contains("写权限") || outsider.contains("通过"),
        "{outsider}"
    );
    let merged = must(bin, &bob, &bob_shop, &["pr", "merge", &price]);
    assert!(merged.contains("已合并"), "{merged}");
    must(bin, &alice, &shop, &["pull"]);
    assert_eq!(
        fs::read_to_string(shop.join("src/catalog/item.rs")).unwrap(),
        "item v2\n"
    );

    must(bin, &carol, &carol_shop, &["pull"]);
    must(bin, &carol, &carol_shop, &["checkout", "-b", "carol/login"]);
    fs::write(carol_shop.join("src/account/login.rs"), "account v2\n").unwrap();
    fs::write(carol_shop.join("web/index.html"), "page v2\n").unwrap();
    must(
        bin,
        &carol,
        &carol_shop,
        &["commit", "-a", "-m", "登录和页面"],
    );
    let cross = must(bin, &carol, &carol_shop, &["push"]);
    assert!(cross.contains("已更新"), "{cross}");
    must(bin, &carol, &carol_shop, &["pr", "create", "改登录和页面"]);
    let listed = must(bin, &alice, &shop, &["pr", "list"]);
    assert!(listed.contains("src/account/login.rs"), "{listed}");
    let login = open_pr(&listed, "carol/login");
    let bob_no = must_fail(bin, &bob, &bob_shop, &["pr", "approve", &login]);
    assert!(bob_no.contains("写权限"), "{bob_no}");
    let carol_no = must_fail(bin, &carol, &carol_shop, &["pr", "merge", &login]);
    assert!(carol_no.contains("通过"), "{carol_no}");
    let approved = must(bin, &alice, &shop, &["pr", "approve", &login]);
    assert!(approved.contains("已通过"), "{approved}");
    let landed = must(bin, &alice, &shop, &["pr", "merge", &login]);
    assert!(landed.contains("已合并"), "{landed}");
    must(bin, &alice, &shop, &["pull"]);
    assert_eq!(
        fs::read_to_string(shop.join("src/account/login.rs")).unwrap(),
        "account v2\n"
    );
    assert_eq!(
        fs::read_to_string(shop.join("web/index.html")).unwrap(),
        "page v2\n"
    );

    must(bin, &carol, &carol_shop, &["checkout", "-b", "bob/price"]);
    fs::write(carol_shop.join("web/index.html"), "page stolen\n").unwrap();
    must(
        bin,
        &carol,
        &carol_shop,
        &["commit", "-a", "-m", "占用别人的分支"],
    );
    let stolen = must_fail(bin, &carol, &carol_shop, &["push"]);
    assert!(stolen.contains("不能更新别人的分支"), "{stolen}");

    must(bin, &bob, &bob_shop, &["pull"]);
    must(bin, &bob, &bob_shop, &["checkout", "-b", "bob/item3"]);
    fs::write(bob_shop.join("src/catalog/item.rs"), "item v3\n").unwrap();
    must(bin, &bob, &bob_shop, &["commit", "-a", "-m", "商品三"]);
    must(bin, &bob, &bob_shop, &["push"]);
    must(bin, &bob, &bob_shop, &["pr", "create", "商品三"]);

    must(bin, &carol, &carol_shop, &["checkout", "main"]);
    must(bin, &carol, &carol_shop, &["pull"]);
    must(bin, &carol, &carol_shop, &["checkout", "-b", "carol/item"]);
    fs::write(carol_shop.join("src/catalog/item.rs"), "item v4\n").unwrap();
    must(bin, &carol, &carol_shop, &["commit", "-a", "-m", "商品四"]);
    must(bin, &carol, &carol_shop, &["push"]);
    must(bin, &carol, &carol_shop, &["pr", "create", "商品四"]);

    let listed = must(bin, &alice, &shop, &["pr", "list"]);
    let item3 = open_pr(&listed, "bob/item3");
    let item4 = open_pr(&listed, "carol/item");
    let bob_landed = must(bin, &bob, &bob_shop, &["pr", "merge", &item3]);
    assert!(bob_landed.contains("已合并"), "{bob_landed}");
    let approved = must(bin, &alice, &shop, &["pr", "approve", &item4]);
    assert!(approved.contains("已通过"), "{approved}");
    let conflict = must_fail(bin, &alice, &shop, &["pr", "merge", &item4]);
    assert!(conflict.contains("冲突"), "{conflict}");
    let closed = must(bin, &carol, &carol_shop, &["pr", "close", &item4]);
    assert!(closed.contains("已关闭"), "{closed}");
    let again = must_fail(bin, &carol, &carol_shop, &["pr", "merge", &item4]);
    assert!(
        again.contains("closed") || again.contains("已是"),
        "{again}"
    );

    must(bin, &carol, &carol_shop, &["checkout", "main"]);
    must(bin, &carol, &carol_shop, &["pull"]);
    fs::write(carol_shop.join("web/index.html"), "page v3\n").unwrap();
    must(bin, &carol, &carol_shop, &["commit", "-a", "-m", "页面三"]);
    let direct = must(bin, &carol, &carol_shop, &["push"]);
    assert!(direct.contains("已更新"), "{direct}");
    must(bin, &alice, &shop, &["pull"]);
    assert_eq!(
        fs::read_to_string(shop.join("web/index.html")).unwrap(),
        "page v3\n"
    );
    assert_eq!(
        fs::read_to_string(shop.join("src/catalog/item.rs")).unwrap(),
        "item v3\n"
    );

    let _ = fs::remove_dir_all(root);
}
