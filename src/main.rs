fn main() {
    // Windows 主线程只有 1 MiB 栈，调试构建下解析几十个子命令的 clap 枚举就会溢出。
    let worker = std::thread::Builder::new()
        .name("ait".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(ait::cli::execute)
        .expect("启动主线程");
    let result = match worker.join() {
        Ok(result) => result,
        Err(_) => std::process::exit(101),
    };
    if let Err(err) = result {
        eprintln!("错误: {err:#}");
        std::process::exit(1);
    }
}
