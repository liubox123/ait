fn main() {
    if let Err(err) = ait::cli::execute() {
        eprintln!("错误: {err:#}");
        std::process::exit(1);
    }
}
