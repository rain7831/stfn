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

    let cli = Cli::parse();
    match cli.mode() {
        Mode::Gui { settings } => {
            if let Err(err) = stfn::ui::run(settings) {
                eprintln!("{err}");
                std::process::exit(1);
            }
        }
        Mode::Cli => {
            std::process::exit(stfn::cli::run(cli));
        }
    }
}
