use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use pulp_tui::config::{Config, default_config_path, default_data_dir};
use pulp_tui::pulp::{Capabilities, PulpClient};
use pulp_tui::store::{NotMigrated, Store};
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
    /// Manage the external Content View database of the selected profile (`[database]` or its own `database`).
    Db {
        #[command(subcommand)]
        cmd: DbCmd,
    },
}

#[derive(Subcommand)]
enum DbCmd {
    /// Create the schema, or upgrade it to this pulp-tui version (normal starts never change it).
    Migrate,
    /// Copy the Content View data of the selected profile into its database, from the local file or another profile's database.
    Copy {
        /// `local` (always the local file, also when a profile has that name) or the name of a profile.
        #[arg(long, default_value = "local")]
        from: String,
    },
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
    let store = Arc::new(Store::open(&data_dir.join("pulp-tui.db")).await?);
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
        Some(Cmd::Db { cmd: DbCmd::Migrate }) => {
            let name = profile.as_deref().unwrap_or("");
            let Some((d, user)) = cfg.database_for(name) else {
                if name.is_empty() {
                    bail!("no [database] in {}", cfg_path.display());
                }
                bail!("profile {name} uses the local file; db migrate is only for an external database");
            };
            let (from, to) = Store::migrate_external(&d.connect_url(&user)?, &d.display_url()).await?;
            if from == to {
                println!("{}: schema {to} is current", d.display_url());
            } else {
                println!("{}: schema {from} -> {to}", d.display_url());
            }
            Ok(())
        }
        Some(Cmd::Db { cmd: DbCmd::Copy { from } }) => {
            let name = profile.context("no profile configured")?;
            cfg.check_copy_write(&name)?;
            let unavailable = |e: String| anyhow!("shared database unavailable: {e}");
            let dst = cv_store(&cfg, &name, &store).await.map_err(unavailable)?;
            let dst_scope = dst.scope(&name, &cfg.profiles[&name]);
            // `local` is the local file, also when a profile has that name.
            let (src, src_scope) = if from == "local" {
                (store.clone(), name.clone())
            } else {
                let p = cfg.profiles.get(&from).with_context(|| format!("profile {from} not found in {}", cfg_path.display()))?;
                let src = cv_store(&cfg, &from, &store).await.map_err(unavailable)?;
                let scope = src.scope(&from, p);
                (src, scope)
            };
            let r = dst.copy_from(&src, &src_scope, &dst_scope).await?;
            let target = cfg.database_for(&name).map_or_else(|| "the local file".into(), |(d, _)| d.display_url());
            println!(
                "copied {} content view(s), {} version(s), {} environment(s), {} binding(s), {} audit row(s) from {from} into {target}",
                r.views, r.versions, r.environments, r.bindings, r.audit
            );
            Ok(())
        }
        Some(Cmd::ExportCv { output }) => {
            let name = profile.context("no profile configured")?;
            let cv = cv_store(&cfg, &name, &store).await.map_err(|e| anyhow!("shared database unavailable: {e}"))?;
            let json = serde_json::to_string_pretty(&cv.export(&cv.scope(&name, &cfg.profiles[&name])).await?)?;
            match output {
                Some(p) => std::fs::write(&p, json).with_context(|| format!("write {}", p.display()))?,
                None => println!("{json}"),
            }
            Ok(())
        }
        Some(Cmd::ImportCv { file }) => {
            let name = profile.context("no profile configured")?;
            cfg.check_shared_write(&name)?;
            let ex = serde_json::from_slice(&std::fs::read(&file).with_context(|| format!("read {}", file.display()))?)?;
            let cv = cv_store(&cfg, &name, &store).await.map_err(|e| anyhow!("shared database unavailable: {e}"))?;
            let (cvs, envs) = cv.import(&cv.scope(&name, &cfg.profiles[&name]), &ex).await?;
            println!("imported {cvs} content view(s) and {envs} environment(s) into profile {name}");
            Ok(())
        }
        Some(Cmd::ConfigExample) => unreachable!(),
        None => {
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                bail!("pulp-tui needs an interactive terminal (in containers use `docker run -it` / `podman run -it`)");
            }
            // Connect opens the CV store of the profile.
            let mut app = pulp_tui::tui::app::App::new(cfg, cfg_path, store, Err(pulp_tui::tui::app::NO_PROFILE.into()));
            app.local_db = data_dir.join("pulp-tui.db");
            pulp_tui::tui::run(app, profile).await
        }
    }
}

/// The Content View store of a profile: its external database when it has one (no fallback to the local file), else the local store.
/// A database that is not migrated names the profile in its hint: `db migrate` needs `--profile`.
async fn cv_store(cfg: &Config, profile: &str, local: &Arc<Store>) -> Result<Arc<Store>, String> {
    let Some((d, user)) = cfg.database_for(profile) else { return Ok(local.clone()) };
    let url = d.connect_url(&user).map_err(|e| format!("{e:#}"))?;
    Store::open_external(&url, &d.display_url()).await.map(Arc::new).map_err(|e| {
        let hint = if e.downcast_ref::<NotMigrated>().is_some() { format!(" --profile {profile}") } else { String::new() };
        format!("{e:#}{hint}")
    })
}

/// Logs go to a file (never the terminal the TUI draws on). Secrets are never logged:
/// auth headers are marked sensitive and only method, path and status are recorded.
fn init_logging(dir: &std::path::Path) -> Result<()> {
    let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("pulp-tui.log"))?;
    let filter = tracing_subscriber::EnvFilter::try_from_env("PULP_TUI_LOG").unwrap_or_else(|_| "pulp_tui=info,warn".into());
    tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::sync::Mutex::new(file)).init();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulp_tui::config::{Database, Profile};

    #[tokio::test]
    async fn not_migrated_hint_names_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cv.db");
        std::fs::write(&file, b"").unwrap();
        let mut cfg = Config::default();
        let database = Some(Database { url: format!("sqlite://{}", file.display()), secret: None, ca_cert: None });
        cfg.profiles.insert("lab".into(), Profile { url: "http://h".into(), database, ..Default::default() });
        let local = Arc::new(Store::open_in_memory().await.unwrap());
        let e = cv_store(&cfg, "lab", &local).await.unwrap_err();
        assert!(e.ends_with("; run pulp-tui db migrate --profile lab"), "{e}");
    }
}
