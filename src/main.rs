mod app;
mod config;
mod model;
mod provider;

use std::env;
use std::path::PathBuf;
use std::process;

use anyhow::Result;
use clap::Parser;
use model::CommandCatalog;

#[derive(Debug, Parser)]
#[command(name = "fzc", version, about = "Fuzzy terminal command launcher")]
struct Cli {
    /// Override config path. If omitted, fzc checks ./fzc.toml, ./.fzc.toml, and then ~/.config/fzc/config.toml
    #[arg(short, long)]
    config: Option<PathBuf>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let explicit_config = cli.config.clone();
    let cwd = env::current_dir()?;
    let loaded = config::load(&cwd, explicit_config.as_deref())?;
    let provider_aliases = loaded.config.providers.alias_map()?;

    let mut catalog = CommandCatalog::empty();
    if loaded.config.providers.config.enabled {
        catalog.extend(CommandCatalog::from_config(&loaded, &cwd)?.into_vec());
    }
    catalog.extend(provider::load_provider_commands(
        &loaded.config.providers,
        &cwd,
    )?);

    app::run_tui(
        catalog.into_vec(),
        loaded.path.as_deref(),
        provider_aliases,
        app::RankingSettings {
            usage_enabled: loaded.config.ranking.usage_enabled,
            usage_weight: loaded.config.ranking.usage_weight,
        },
        app::RuntimeContext {
            cwd,
            explicit_config_path: explicit_config,
        },
    )
}
