use runway::cli::{self, Cli};
use runway::output::print_error;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let cli = Cli::parse_branded();
    let level = match cli.verbose {
        0 => "warn",
        1 => "runway=debug,info",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let format = cli.output;
    if let Err(err) = cli::run(cli).await {
        if !err.reported {
            print_error(&err, format);
        }
        std::process::exit(err.exit_code());
    }
}
