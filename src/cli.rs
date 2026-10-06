//! Command-line interface definition and dispatch.

use crate::commands;
use crate::error::Result;
use crate::output::{OutputFormat, Progress};
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, CommandFactory, Parser, Subcommand};
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

/// Colors for `--help` and usage errors; shown only when colors are enabled.
const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default())
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD));

/// Styles section labels (`Exit codes:`, the part before the first `:` of
/// every non-indented line) like clap's own section headers.
fn styled_help(text: &str) -> String {
    let h = HELP_STYLES.get_header();
    text.lines()
        .map(|line| match line.split_once(':') {
            Some((label, rest)) if !line.starts_with(' ') => format!("{h}{label}:{h:#}{rest}"),
            _ => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const AFTER_HELP: &str = "\
Exit codes:
  0    success
  1    unexpected error
  2    invalid command-line usage
  3    invalid configuration
  4    missing credentials, permissions, APIs or prerequisite resources
  5    Cloud Build failed
  6    Cloud Run rejected the service or the revision is unhealthy
  7    refused to modify a resource runway does not own
  8    timed out
  9    service not found
  130  interrupted

Documentation: https://runway.echaouchna.dev/docs";

/// Deploy applications to Google Cloud Run from a single runway.yaml.
#[derive(Debug, Parser)]
#[command(
    name = "runway",
    version = crate::VERSION,
    about,
    long_about = None,
    styles = HELP_STYLES,
    before_help = crate::branding::logo(false),
    after_help = styled_help(AFTER_HELP),
)]
pub struct Cli {
    /// Configuration file (any name, e.g. `deploy/api.yaml`), or a directory
    /// containing `runway.yaml` / `runway.yml`. Default: `./runway.yaml`, then
    /// `./runway.yml`. Relative paths in the file are relative to its directory.
    #[arg(
        short,
        long,
        visible_alias = "file",
        global = true,
        env = "RUNWAY_CONFIG",
        value_name = "PATH"
    )]
    pub config: Option<PathBuf>,

    /// Output format for results. Progress is always written to stderr.
    #[arg(short, long, global = true, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,

    /// Colors: `auto` (terminals and CI logs; honours NO_COLOR), `always` or `never`.
    #[arg(long, global = true, value_enum, env = "RUNWAY_COLOR", default_value_t = crate::style::ColorMode::Auto)]
    pub color: crate::style::ColorMode,

    /// Suppress progress messages.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Impersonate a service account for every Google Cloud call
    /// (`SA` or `DELEGATE,...,SA`). Overrides `provider.impersonate_service_account`.
    #[arg(
        long,
        global = true,
        env = "RUNWAY_IMPERSONATE_SERVICE_ACCOUNT",
        value_name = "EMAIL"
    )]
    pub impersonate_service_account: Option<String>,

    /// Increase log verbosity (-v debug, -vv trace). RUST_LOG overrides.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn parse_branded() -> Self {
        let args: Vec<_> = std::env::args_os().collect();
        match Self::try_parse_from(&args) {
            Ok(cli) => cli,
            Err(err) => exit_with(err, &args),
        }
    }
}

/// Prints help, version and usage errors with runway's color detection
/// (`--color`, `RUNWAY_COLOR`, CI logs) instead of clap's, then exits.
fn exit_with(err: clap::Error, args: &[OsString]) -> ! {
    let (color, json) = color_preferences(args);
    crate::style::init(color, json);
    let enabled = if err.use_stderr() {
        crate::style::err().enabled
    } else {
        crate::style::out().enabled
    };
    let styled = err.render();
    let plain = styled.to_string();
    let text = if enabled {
        let plain_logo = crate::branding::logo(false);
        let ansi = styled.ansi().to_string();
        if plain.starts_with(&plain_logo) {
            ansi.replacen(&plain_logo, &crate::branding::logo(true), 1)
        } else {
            ansi
        }
    } else {
        plain
    };
    let _ = if err.use_stderr() {
        std::io::stderr().lock().write_all(text.as_bytes())
    } else {
        std::io::stdout().lock().write_all(text.as_bytes())
    };
    std::process::exit(err.exit_code())
}

/// Reads `--color` and `--output` from arguments that failed to parse (or
/// asked for help), on either side of `--help` and in any subcommand.
fn color_preferences(args: &[OsString]) -> (crate::style::ColorMode, bool) {
    let Ok(matches) = without_help(Cli::command())
        .disable_version_flag(true)
        .ignore_errors(true)
        .try_get_matches_from(args)
    else {
        return (crate::style::ColorMode::default(), false);
    };
    let mut m = &matches;
    while let Some((_, sub)) = m.subcommand() {
        m = sub;
    }
    let color = m
        .get_one::<crate::style::ColorMode>("color")
        .copied()
        .unwrap_or_default();
    let json = m.get_one::<OutputFormat>("output") == Some(&OutputFormat::Json);
    (color, json)
}

/// Replaces the help flag and subcommand at every level by a plain flag, so
/// parsing continues past `--help` instead of printing it.
fn without_help(cmd: clap::Command) -> clap::Command {
    cmd.disable_help_flag(true)
        .disable_help_subcommand(true)
        .subcommand_required(false)
        .arg_required_else_help(false)
        .arg(
            clap::Arg::new("branding_help")
                .short('h')
                .long("help")
                .action(clap::ArgAction::SetTrue),
        )
        .mut_subcommands(without_help)
}

#[derive(Debug, Args, Clone)]
pub struct StageArg {
    /// Stage to operate on (must be defined under `stages`).
    #[arg(short, long, env = "RUNWAY_STAGE")]
    pub stage: String,
    /// Only these services or jobs: names (the main service is named after
    /// the app), or paths that select what is built inside them. Repeat or
    /// separate with commas.
    #[arg(long, value_delimiter = ',', env = "RUNWAY_ONLY")]
    pub only: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create runway.yaml and a small runnable example without overwriting files.
    Init(InitArgs),
    /// Validate the configuration locally (no credentials needed).
    Validate(ValidateArgs),
    /// Check credentials, APIs, permissions and prerequisite resources.
    Doctor(DoctorArgs),
    /// Show what deploy would change, without modifying anything.
    Plan(PlanArgs),
    /// Build if needed, deploy to Cloud Run and wait until ready.
    Deploy(DeployArgs),
    /// Show the service URL, revision and status.
    Info(InfoArgs),
    /// Show recent application logs.
    Logs(LogsArgs),
    /// Draw the stack (ASCII art or Mermaid) and explain it, from the configuration only.
    Describe(DescribeArgs),
    /// Show or change the traffic split: promote a canary, split, remove preview URLs.
    Traffic(TrafficArgs),
    /// Branch previews: list them, delete some, or prune those of merged or deleted branches.
    #[command(subcommand)]
    Preview(PreviewAction),
    /// Remove the service (and the runtime identity runway created); keep data and APIs.
    Undeploy(UndeployArgs),
    /// Run a Cloud Run job of the stage now (optionally wait for it to finish).
    RunJob(RunJobArgs),
    /// Print a shell completion script (bash, zsh, fish, nushell, xonsh, elvish, powershell).
    Completions(CompletionsArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Nushell,
    Xonsh,
    Elvish,
    Powershell,
}

const COMPLETIONS_HELP: &str = "\
Install:
  bash        runway completions bash > ~/.local/share/bash-completion/completions/runway
  zsh         runway completions zsh > \"${fpath[1]}/_runway\"   (then restart zsh)
  fish        runway completions fish > ~/.config/fish/completions/runway.fish
  nushell     runway completions nushell | save -f ~/.config/nushell/runway.nu
              then add `use ~/.config/nushell/runway.nu *` to config.nu
  xonsh       runway completions xonsh > ~/.config/xonsh/runway.xsh
              then add `source ~/.config/xonsh/runway.xsh` to ~/.xonshrc
  elvish      runway completions elvish >> ~/.config/elvish/rc.elv
  powershell  runway completions powershell >> $PROFILE";

#[derive(Debug, Args)]
#[command(after_help = styled_help(COMPLETIONS_HELP))]
pub struct CompletionsArgs {
    /// Target shell.
    #[arg(value_enum)]
    pub shell: Shell,
}

#[derive(Debug, Subcommand)]
pub enum PreviewAction {
    /// List preview URLs (and other tags) with their revisions.
    List(PreviewListArgs),
    /// Delete preview URLs, by branch or tag name.
    Delete(PreviewDeleteArgs),
    /// Delete the previews of branches merged into the base branch or deleted
    /// from the remote (uses git in the configuration's directory).
    Prune(PreviewPruneArgs),
}

#[derive(Debug, Args)]
pub struct PreviewListArgs {
    #[command(flatten)]
    pub stage: StageArg,
}

#[derive(Debug, Args)]
pub struct PreviewDeleteArgs {
    #[command(flatten)]
    pub stage: StageArg,
    /// Branch (`feature/login`) or tag (`feature-login`) names.
    #[arg(required = true, value_name = "NAME")]
    pub names: Vec<String>,
    /// Apply (without it, only print what would be removed).
    #[arg(long)]
    pub yes: bool,
    /// Also delete the preview revisions once nothing points at them.
    #[arg(long)]
    pub delete_revisions: bool,
    /// Maximum time to wait for Cloud Run.
    #[arg(long, default_value = "5m", value_parser = humantime::parse_duration)]
    pub timeout: std::time::Duration,
}

#[derive(Debug, Args)]
pub struct PreviewPruneArgs {
    #[command(flatten)]
    pub stage: StageArg,
    /// Branch previews are merged into (default: the remote's default branch,
    /// then $CI_DEFAULT_BRANCH, then `main`).
    #[arg(long)]
    pub base: Option<String>,
    /// Git remote to read branches from.
    #[arg(long, default_value = "origin")]
    pub remote: String,
    /// Only prune merged branches (keep previews whose branch was deleted
    /// without being merged, or whose name is not a branch).
    #[arg(long)]
    pub only_merged: bool,
    /// Do not run `git fetch --prune` first.
    #[arg(long)]
    pub no_fetch: bool,
    /// Apply (without it, only print what would be removed).
    #[arg(long)]
    pub yes: bool,
    /// Also delete the preview revisions once nothing points at them.
    #[arg(long)]
    pub delete_revisions: bool,
    /// Maximum time to wait for Cloud Run.
    #[arg(long, default_value = "5m", value_parser = humantime::parse_duration)]
    pub timeout: std::time::Duration,
}

#[derive(Debug, Args)]
pub struct TrafficArgs {
    #[command(flatten)]
    pub stage: StageArg,
    /// Send all traffic to the canary revision (deployed with `deploy --traffic`).
    #[arg(long, conflicts_with_all = ["set", "remove_tag"])]
    pub promote: bool,
    /// Explicit split, repeatable: `--set latest=90 --set canary=10`. Targets are
    /// `latest`, a tag or a revision name; percentages must add up to 100.
    #[arg(long, value_name = "TARGET=PERCENT", conflicts_with = "remove_tag")]
    pub set: Vec<String>,
    /// Remove a tag and its URL (for example a merged branch's preview), repeatable.
    #[arg(long, value_name = "NAME")]
    pub remove_tag: Vec<String>,
    /// Maximum time to wait for the change to be applied.
    #[arg(long, default_value = "5m", value_parser = humantime::parse_duration)]
    pub timeout: std::time::Duration,
}

#[derive(Debug, Args)]
pub struct UndeployArgs {
    #[command(flatten)]
    pub stage: StageArg,
    /// Apply the plan (without it, only print what would be deleted and kept).
    #[arg(long)]
    pub yes: bool,
    /// Only remove this preview's URL (tag) from the service; delete nothing.
    #[arg(long, value_name = "NAME")]
    pub preview: Option<String>,
    /// Also delete this app's container images from the repository.
    #[arg(long)]
    pub delete_images: bool,
    /// Only delete the services and jobs runway deployed for this stage that
    /// runway.yaml no longer lists.
    #[arg(long, conflicts_with = "preview")]
    pub orphans: bool,
    /// Maximum time to wait for the service deletion.
    #[arg(long, default_value = "5m", value_parser = humantime::parse_duration)]
    pub timeout: std::time::Duration,
    /// Retries per failed step (overrides `retry.attempts`; 0 disables retries).
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=19))]
    pub retries: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DiagramFormat {
    Ascii,
    Mermaid,
}

#[derive(Debug, Args)]
pub struct DescribeArgs {
    /// Stage to describe (optional when only one stage is defined).
    #[arg(short, long, env = "RUNWAY_STAGE")]
    pub stage: Option<String>,
    /// Diagram format.
    #[arg(long, value_enum, default_value_t = DiagramFormat::Ascii)]
    pub format: DiagramFormat,
    /// Print only the diagram (no explanation).
    #[arg(long)]
    pub diagram_only: bool,
    /// Only these services or jobs: names (the main service is named after
    /// the app), or paths that select what is built inside them. Repeat or
    /// separate with commas.
    #[arg(long, value_delimiter = ',', env = "RUNWAY_ONLY")]
    pub only: Vec<String>,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Directory to initialize.
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
    /// Application name (default: directory name).
    #[arg(long)]
    pub app: Option<String>,
    /// GCP project ID (default: $GOOGLE_CLOUD_PROJECT or $CLOUDSDK_CORE_PROJECT).
    #[arg(long)]
    pub project: Option<String>,
    /// Cloud Run region.
    #[arg(long, default_value = "europe-west1")]
    pub region: String,
    /// Generate an image-based configuration instead of a source build.
    #[arg(long)]
    pub image: Option<String>,
}

#[derive(Debug, Args)]
pub struct RunJobArgs {
    #[command(flatten)]
    pub stage: StageArg,
    /// The job (a key of `jobs`).
    pub name: String,
    /// Wait until the execution finishes, and fail if it fails.
    #[arg(long)]
    pub wait: bool,
    /// How long to wait with `--wait`.
    #[arg(long, default_value = "1h", value_parser = humantime::parse_duration)]
    pub timeout: std::time::Duration,
}

#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// Validate only this stage (default: all stages).
    #[arg(short, long, env = "RUNWAY_STAGE")]
    pub stage: Option<String>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[command(flatten)]
    pub stage: StageArg,
}

#[derive(Debug, Args)]
pub struct PlanArgs {
    #[command(flatten)]
    pub stage: StageArg,
    /// Deploy this image instead of the configured image or source build.
    #[arg(long)]
    pub image: Option<String>,
    /// Do not contact Google Cloud; show the desired state only.
    #[arg(long)]
    pub offline: bool,
    /// Deploy without sending traffic: the new revision only gets its own URL
    /// (`https://NAME---SERVICE-...run.app`). NAME is typically the branch
    /// (`feature/login` becomes the tag `feature-login`).
    #[arg(long, value_name = "NAME", conflicts_with = "traffic")]
    pub preview: Option<String>,
    /// Canary: send PERCENT of the traffic to the new revision (tagged
    /// `canary`); finish with `runway traffic --promote`.
    #[arg(long, value_name = "PERCENT", value_parser = clap::value_parser!(u32).range(1..=99))]
    pub traffic: Option<u32>,
}

#[derive(Debug, Args)]
pub struct DeployArgs {
    #[command(flatten)]
    pub stage: StageArg,
    /// Deploy this image instead of the configured image or source build.
    #[arg(long)]
    pub image: Option<String>,
    /// Maximum time to wait for the service to become ready.
    #[arg(long, default_value = "10m", value_parser = humantime::parse_duration)]
    pub timeout: std::time::Duration,
    /// Maximum Cloud Build duration.
    #[arg(long, default_value = "20m", value_parser = humantime::parse_duration)]
    pub build_timeout: std::time::Duration,
    /// Rebuild even if an image for identical source already exists.
    #[arg(long)]
    pub force_build: bool,
    /// Take over an existing service that has no runway labels.
    #[arg(long)]
    pub adopt: bool,
    /// Deploy without sending traffic: the new revision only gets its own URL
    /// (`https://NAME---SERVICE-...run.app`). NAME is typically the branch
    /// (`feature/login` becomes the tag `feature-login`).
    #[arg(long, value_name = "NAME", conflicts_with = "traffic")]
    pub preview: Option<String>,
    /// Canary: send PERCENT of the traffic to the new revision (tagged
    /// `canary`); finish with `runway traffic --promote`.
    #[arg(long, value_name = "PERCENT", value_parser = clap::value_parser!(u32).range(1..=99))]
    pub traffic: Option<u32>,
    /// Retries per failed step (overrides `retry.attempts`; 0 disables retries).
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=19))]
    pub retries: Option<u32>,
    /// Tag the image with the latest version of the changelog (vX.Y.Z or X.Y.Z).
    #[arg(long, conflicts_with = "tag_rc")]
    pub tag: bool,
    /// Tag the image as the next release candidate of that version (X.Y.Z-RCn).
    #[arg(long)]
    pub tag_rc: bool,
    /// Initial delay between retries, doubling each time (overrides `retry.delay`).
    #[arg(long, value_parser = humantime::parse_duration)]
    pub retry_delay: Option<std::time::Duration>,
}

#[derive(Debug, Args)]
pub struct InfoArgs {
    #[command(flatten)]
    pub stage: StageArg,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    #[command(flatten)]
    pub stage: StageArg,
    /// How far back to look (e.g. 10m, 1h, 2d).
    #[arg(long, default_value = "10m")]
    pub since: String,
    /// Maximum number of entries.
    #[arg(long, default_value_t = 200)]
    pub limit: usize,
    /// Keep polling for new entries until interrupted.
    #[arg(short, long)]
    pub follow: bool,
    /// Include Cloud Run request logs.
    #[arg(long)]
    pub include_requests: bool,
    /// Minimum severity (DEBUG, INFO, WARNING, ERROR, ...).
    #[arg(long)]
    pub severity: Option<String>,
}

/// Default configuration file names, in lookup order.
pub const CONFIG_NAMES: [&str; 2] = ["runway.yaml", "runway.yml"];

/// The configuration file to use: an explicit file as given; in a directory
/// (the current one by default), `runway.yaml` or else `runway.yml`.
pub fn config_path(arg: Option<&std::path::Path>) -> PathBuf {
    let dir = match arg {
        Some(p) if !p.is_dir() => return p.to_path_buf(),
        Some(p) => p,
        None => std::path::Path::new(""),
    };
    let candidates = CONFIG_NAMES.map(|n| dir.join(n));
    candidates
        .iter()
        .find(|p| p.is_file())
        .unwrap_or(&candidates[0])
        .clone()
}

/// Global context shared by commands.
pub struct Context {
    /// Resolved configuration file (see [`config_path`]).
    pub config: PathBuf,
    /// File name given explicitly with `--config` (used by `init`).
    pub config_name: Option<String>,
    /// `--impersonate-service-account` / `RUNWAY_IMPERSONATE_SERVICE_ACCOUNT`.
    pub impersonate: Option<String>,
    pub output: OutputFormat,
    pub progress: Progress,
}

pub async fn run(cli: Cli) -> Result<()> {
    crate::style::init(cli.color, cli.output == OutputFormat::Json);
    let config_name = cli
        .config
        .as_deref()
        .filter(|p| !p.is_dir())
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());
    let ctx = Context {
        config: config_path(cli.config.as_deref()),
        config_name,
        impersonate: cli.impersonate_service_account,
        output: cli.output,
        progress: Progress::new(!cli.quiet),
    };
    match cli.command {
        Command::Init(a) => commands::init::run(&ctx, a),
        Command::Validate(a) => commands::validate::run(&ctx, a),
        Command::Doctor(a) => commands::doctor::run(&ctx, a).await,
        Command::Plan(a) => commands::plan::run(&ctx, a).await,
        Command::Deploy(a) => commands::deploy::run(&ctx, a).await,
        Command::Info(a) => commands::info::run(&ctx, a).await,
        Command::RunJob(a) => commands::run_job::run(&ctx, a).await,
        Command::Logs(a) => commands::logs::run(&ctx, a).await,
        Command::Describe(a) => commands::describe::run(&ctx, a),
        Command::Traffic(a) => commands::traffic::run(&ctx, a).await,
        Command::Preview(a) => commands::preview::run(&ctx, a).await,
        Command::Undeploy(a) => commands::undeploy::run(&ctx, a).await,
        Command::Completions(a) => commands::completions::run(a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_mark_is_plain_ascii_before_usage() {
        let help = Cli::command().render_help().to_string();
        let mark = help.split("Deploy applications").next().unwrap();
        assert!(mark.contains("####"));
        assert!(mark.is_ascii());
        assert!(!mark.contains('\x1b'));
        assert!(mark.lines().all(|line| line.len() <= 76));
        assert!(help.contains("Usage:"));
    }
}
