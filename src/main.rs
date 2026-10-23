use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use pulp_tui::config::{Config, default_config_path, default_data_dir};
use pulp_tui::pulp::{Capabilities, PulpClient};
use pulp_tui::store::Store;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

/// Terminal management for Pulp 3 servers (RPM and File repositories, Content Views,
/// lifecycle environments, replication, users and tasks).
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Config file (default: $XDG_CONFIG_HOME/pulp-tui/config.toml).
    #[arg(long, global = true, env = "PULP_TUI_CONFIG")]
    config: Option<PathBuf>,
    /// Server profile to connect to on start.
    #[arg(long, short, global = true, env = "PULP_TUI_PROFILE")]
    profile: Option<String>,
    /// Directory for the local database and log file.
    #[arg(long, global = true, env = "PULP_TUI_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check the connection and capabilities of a profile (non-interactive).
    Check,
    /// Export Content View and environment definitions (no secrets) as JSON.
    ExportCv {
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Import Content View and environment definitions from JSON.
    ImportCv { file: PathBuf },
    /// Print an example configuration file.
    ConfigExample,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Cmd::ConfigExample) = cli.cmd {
        print!("{}", include_str!("../examples/config.toml"));
        return Ok(());
    }
    let cfg_path = cli.config.clone().or_else(default_config_path).context("cannot determine config path; use --config")?;
    let cfg = Config::load(&cfg_path)?;
    let data_dir = cli.data_dir.clone().or_else(default_data_dir).context("cannot determine data dir; use --data-dir")?;
    std::fs::create_dir_all(&data_dir).with_context(|| format!("create {}", data_dir.display()))?;
    init_logging(&data_dir)?;
    let store = Arc::new(Store::open(&data_dir.join("pulp-tui.db"))?);
    let profile = cfg.pick_profile(cli.profile.as_deref());
    if let (Some(req), None) = (&cli.profile, &profile) {
        bail!("profile {req} not found in {}", cfg_path.display());
    }

    match cli.cmd {
        Some(Cmd::Check) => {
            let name = profile.context("no profile configured")?;
            let c = PulpClient::new(&name, &cfg.profiles[&name], &cfg.settings)?;
            let caps = Capabilities::detect(&c).await?;
            println!("profile {name}: connected as {}", caps.user.as_ref().map(|u| u.username.as_str()).unwrap_or("anonymous"));
            for l in caps.summary_lines() {
                println!("  {l}");
            }
            Ok(())
        }
        Some(Cmd::ExportCv { output }) => {
            let name = profile.context("no profile configured")?;
            let json = serde_json::to_string_pretty(&store.export(&name)?)?;
            match output {
                Some(p) => std::fs::write(&p, json).with_context(|| format!("write {}", p.display()))?,
                None => println!("{json}"),
            }
            Ok(())
        }
        Some(Cmd::ImportCv { file }) => {
            let name = profile.context("no profile configured")?;
            let ex = serde_json::from_slice(&std::fs::read(&file).with_context(|| format!("read {}", file.display()))?)?;
            let (cvs, envs) = store.import(&name, &ex)?;
            println!("imported {cvs} content view(s) and {envs} environment(s) into profile {name}");
            Ok(())
        }
        Some(Cmd::ConfigExample) => unreachable!(),
        None => {
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                bail!("pulp-tui needs an interactive terminal (in containers use `docker run -it` / `podman run -it`)");
            }
            let app = pulp_tui::tui::app::App::new(cfg, cfg_path, store);
            pulp_tui::tui::run(app, profile).await
        }
    }
}

/// Logs go to a file (never the terminal the TUI draws on). Secrets are never logged:
/// auth headers are marked sensitive and only method, path and status are recorded.
fn init_logging(dir: &std::path::Path) -> Result<()> {
    let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("pulp-tui.log"))?;
    let filter = tracing_subscriber::EnvFilter::try_from_env("PULP_TUI_LOG").unwrap_or_else(|_| "pulp_tui=info,warn".into());
    tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::sync::Mutex::new(file)).init();
    Ok(())
}
