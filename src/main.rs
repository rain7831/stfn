//! 二进制入口。
//!
//! 只负责初始化日志、解析参数，然后把控制权交给 GUI 或 CLI。
//! GUI 模式每次启动都是独立进程/窗口（NON_UNIQUE）；CLI 模式完全不初始化 GTK。
use clap::Parser;
use stfn::cli::{Cli, Mode};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("stfn=info")),
        )
        .with_target(false)
        .init();

    // 参数分派：无参数 / settings / selection 走 GUI，其余走 CLI。
    let cli = Cli::parse();
    match cli.mode() {
        // GUI 启动失败时直接把错误写到 stderr，避免静默退出。
        Mode::Gui { intent } => {
            if let Err(err) = stfn::ui::run(intent) {
                eprintln!("{err}");
                std::process::exit(1);
            }
        }
        Mode::Cli => {
            std::process::exit(stfn::cli::run(cli));
        }
    }
}
