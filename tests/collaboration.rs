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
    path.push(format!("ait-flow-{}", ait::identity::random_token()));
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

#[test]
fn owners_push_directly_and_cross_module_edits_need_review() {
    let bin = env!("CARGO_BIN_EXE_ait");
    let root = temp_dir();
    let server = start_server(bin, &root.join("data"));
    let origin = &server.origin;
    let alice = root.join("alice-home");
    let bob = root.join("bob-home");
    let shop = root.join("shop");
    fs::create_dir_all(&alice).unwrap();
    fs::create_dir_all(&bob).unwrap();

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
    let created = must(
        bin,
        &alice,
        &root,
        &["repo", "create", "shop", "--description", "商城"],
    );
    assert!(created.contains("shop"), "{created}");

    must(bin, &alice, &root, &["init", shop.to_str().unwrap()]);
    fs::create_dir_all(shop.join("src/account")).unwrap();
    fs::create_dir_all(shop.join("src/catalog")).unwrap();
    fs::write(shop.join("src/account/login.rs"), "account v1\n").unwrap();
    fs::write(shop.join("src/catalog/item.rs"), "item v1\n").unwrap();
    must(bin, &alice, &shop, &["add"]);
    must(bin, &alice, &shop, &["commit", "-m", "初始模块"]);
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
        "## 账户 (@alice)\n职责: 登录与会话\n目录: src/account/**\n\n## 商品 (@bob)\n职责: 商品与库存\n目录: src/catalog/**\n",
    )
    .unwrap();
    let applied = must(
        bin,
        &alice,
        &shop,
        &["plan", "apply", "--file", plan.to_str().unwrap()],
    );
    assert!(applied.contains("bob"), "{applied}");
    assert!(applied.contains("src/catalog/**"), "{applied}");

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
    assert_eq!(
        fs::read_to_string(bob_shop.join("src/catalog/item.rs")).unwrap(),
        "item v1\n"
    );

    fs::write(bob_shop.join("src/catalog/item.rs"), "item v2\n").unwrap();
    must(bin, &bob, &bob_shop, &["add"]);
    must(bin, &bob, &bob_shop, &["commit", "-m", "调整商品"]);
    let own_push = must(bin, &bob, &bob_shop, &["push"]);
    assert!(
        own_push.contains("已更新"),
        "自己的目录应该直接推送:\n{own_push}"
    );

    fs::write(bob_shop.join("src/account/login.rs"), "account v2\n").unwrap();
    must(bin, &bob, &bob_shop, &["add"]);
    must(bin, &bob, &bob_shop, &["commit", "-m", "越权改登录"]);
    let cross_push = must(bin, &bob, &bob_shop, &["push"]);
    assert!(
        cross_push.contains("评审"),
        "越权推送应该进入评审:\n{cross_push}"
    );
    assert!(cross_push.contains("src/account/login.rs"), "{cross_push}");

    let reviews = must(bin, &alice, &shop, &["review", "list"]);
    let id = reviews.split_whitespace().next().expect("评审编号");
    assert!(id.starts_with("r-"), "{reviews}");
    let approved = must(bin, &alice, &shop, &["review", "approve", id]);
    assert!(approved.contains("已通过"), "{approved}");

    let pulled = must(bin, &alice, &shop, &["pull"]);
    assert!(
        pulled.contains("快进") || pulled.contains("最新") || pulled.contains("合并"),
        "{pulled}"
    );
    assert_eq!(
        fs::read_to_string(shop.join("src/account/login.rs")).unwrap(),
        "account v2\n"
    );
    assert_eq!(
        fs::read_to_string(shop.join("src/catalog/item.rs")).unwrap(),
        "item v2\n"
    );

    let _ = fs::remove_dir_all(root);
}
