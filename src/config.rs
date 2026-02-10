use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub ranking: RankingConfig,
    #[serde(default)]
    pub commands: Vec<CommandConfig>,
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_usage_weight() -> i64 {
    8_000
}

fn default_justfile_path() -> String {
    "justfile".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default, deserialize_with = "deserialize_config_provider")]
    pub config: ConfigProviderConfig,
    #[serde(default, deserialize_with = "deserialize_artisan_provider")]
    pub artisan: ArtisanProviderConfig,
    #[serde(default, deserialize_with = "deserialize_composer_provider")]
    pub composer: ComposerProviderConfig,
    #[serde(default, deserialize_with = "deserialize_justfile_provider")]
    pub justfile: JustfileProviderConfig,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            config: ConfigProviderConfig::default(),
            artisan: ArtisanProviderConfig::default(),
            composer: ComposerProviderConfig::default(),
            justfile: JustfileProviderConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RankingConfig {
    #[serde(default = "default_true")]
    pub usage_enabled: bool,
    #[serde(default = "default_usage_weight")]
    pub usage_weight: i64,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            usage_enabled: true,
            usage_weight: default_usage_weight(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigProviderConfig {
    #[serde(default = "default_false")]
    pub enabled: bool,
    #[serde(default)]
    pub alias: Option<String>,
}

impl Default for ConfigProviderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            alias: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtisanProviderConfig {
    #[serde(default = "default_false")]
    pub enabled: bool,
    #[serde(default)]
    pub alias: Option<String>,
}

impl Default for ArtisanProviderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            alias: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ComposerProviderConfig {
    #[serde(default = "default_false")]
    pub enabled: bool,
    #[serde(default)]
    pub alias: Option<String>,
}

impl Default for ComposerProviderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            alias: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct JustfileProviderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_justfile_path")]
    pub path: String,
    #[serde(default, deserialize_with = "deserialize_provider_options")]
    pub options: Vec<String>,
    #[serde(default)]
    pub alias: Option<String>,
}

impl Default for JustfileProviderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_justfile_path(),
            options: Vec::new(),
            alias: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ProviderOptionsConfig {
    Single(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ProviderBoolOrTable<T> {
    Bool(bool),
    Table(T),
}

fn deserialize_artisan_provider<'de, D>(
    deserializer: D,
) -> std::result::Result<ArtisanProviderConfig, D::Error>
where
    D: Deserializer<'de>,
{
    let input = ProviderBoolOrTable::<ArtisanProviderConfig>::deserialize(deserializer)?;
    Ok(match input {
        ProviderBoolOrTable::Bool(enabled) => ArtisanProviderConfig {
            enabled,
            ..ArtisanProviderConfig::default()
        },
        ProviderBoolOrTable::Table(config) => config,
    })
}

fn deserialize_config_provider<'de, D>(
    deserializer: D,
) -> std::result::Result<ConfigProviderConfig, D::Error>
where
    D: Deserializer<'de>,
{
    let input = ProviderBoolOrTable::<ConfigProviderConfig>::deserialize(deserializer)?;
    Ok(match input {
        ProviderBoolOrTable::Bool(enabled) => ConfigProviderConfig {
            enabled,
            ..ConfigProviderConfig::default()
        },
        ProviderBoolOrTable::Table(config) => config,
    })
}

fn deserialize_composer_provider<'de, D>(
    deserializer: D,
) -> std::result::Result<ComposerProviderConfig, D::Error>
where
    D: Deserializer<'de>,
{
    let input = ProviderBoolOrTable::<ComposerProviderConfig>::deserialize(deserializer)?;
    Ok(match input {
        ProviderBoolOrTable::Bool(enabled) => ComposerProviderConfig {
            enabled,
            ..ComposerProviderConfig::default()
        },
        ProviderBoolOrTable::Table(config) => config,
    })
}

fn deserialize_justfile_provider<'de, D>(
    deserializer: D,
) -> std::result::Result<JustfileProviderConfig, D::Error>
where
    D: Deserializer<'de>,
{
    let input = ProviderBoolOrTable::<JustfileProviderConfig>::deserialize(deserializer)?;
    Ok(match input {
        ProviderBoolOrTable::Bool(enabled) => JustfileProviderConfig {
            enabled,
            ..JustfileProviderConfig::default()
        },
        ProviderBoolOrTable::Table(config) => config,
    })
}

impl ProvidersConfig {
    pub fn alias_map(&self) -> Result<HashMap<String, String>> {
        let mut aliases = HashMap::new();
        insert_alias(&mut aliases, "config", self.config.alias.as_deref())?;
        insert_alias(&mut aliases, "artisan", self.artisan.alias.as_deref())?;
        insert_alias(&mut aliases, "composer", self.composer.alias.as_deref())?;
        insert_alias(&mut aliases, "justfile", self.justfile.alias.as_deref())?;
        Ok(aliases)
    }
}

fn insert_alias(
    aliases: &mut HashMap<String, String>,
    provider_name: &str,
    alias: Option<&str>,
) -> Result<()> {
    let Some(alias) = alias else {
        return Ok(());
    };
    let normalized = alias.trim().trim_start_matches(':').to_ascii_lowercase();
    if normalized.is_empty() {
        bail!("provider alias for '{provider_name}' cannot be empty");
    }

    if let Some(existing) = aliases.get(&normalized) {
        bail!(
            "provider alias ':{}' is duplicated between '{}' and '{}'",
            normalized,
            existing,
            provider_name
        );
    }

    aliases.insert(normalized, provider_name.to_string());
    Ok(())
}

fn deserialize_provider_options<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let options = ProviderOptionsConfig::deserialize(deserializer)?;
    Ok(match options {
        ProviderOptionsConfig::Single(value) => vec![value],
        ProviderOptionsConfig::Many(values) => values,
    })
}

#[cfg(test)]
mod tests {
    use super::{Config, ParamLiteralConfig, ParamTypeConfig};

    #[test]
    fn supports_table_provider_config() {
        let raw = r#"
[providers.artisan]
enabled = true
alias = "a"

[providers.composer]
enabled = true
alias = "co"

[providers.config]
enabled = true
alias = "cf"

[providers.justfile]
enabled = true
path = ".justfile"
options = "--working-directory ."
alias = "j"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(cfg.providers.config.enabled);
        assert!(cfg.providers.artisan.enabled);
        assert!(cfg.providers.composer.enabled);
        assert!(cfg.providers.justfile.enabled);
        assert_eq!(cfg.providers.justfile.path, ".justfile");
        assert_eq!(
            cfg.providers.justfile.options,
            vec!["--working-directory .".to_string()]
        );
        assert_eq!(cfg.providers.artisan.alias.as_deref(), Some("a"));
        assert_eq!(cfg.providers.composer.alias.as_deref(), Some("p"));
        assert_eq!(cfg.providers.config.alias.as_deref(), Some("c"));
        assert_eq!(cfg.providers.justfile.alias.as_deref(), Some("j"));
        assert!(cfg.ranking.usage_enabled);
    }

    #[test]
    fn supports_legacy_boolean_provider_config() {
        let raw = r#"
[providers]
config = false
artisan = true
composer = true
justfile = false
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(!cfg.providers.config.enabled);
        assert!(cfg.providers.artisan.enabled);
        assert!(cfg.providers.composer.enabled);
        assert!(!cfg.providers.justfile.enabled);
        assert_eq!(cfg.providers.justfile.path, "justfile");
        assert!(cfg.providers.justfile.options.is_empty());
    }

    #[test]
    fn missing_providers_default_to_disabled() {
        let raw = r#"
[[commands]]
name = "foo"
run = "echo foo"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(!cfg.providers.config.enabled);
        assert!(!cfg.providers.artisan.enabled);
        assert!(!cfg.providers.composer.enabled);
        assert!(!cfg.providers.justfile.enabled);
    }

    #[test]
    fn ranking_is_configurable() {
        let raw = r#"
[ranking]
usage_enabled = false
usage_weight = 123
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(!cfg.ranking.usage_enabled);
        assert_eq!(cfg.ranking.usage_weight, 123);
    }

    #[test]
    fn rejects_duplicate_provider_aliases() {
        let raw = r#"
[providers.artisan]
enabled = true
alias = "x"

[providers.composer]
enabled = true
alias = "x"

[providers.justfile]
enabled = true
alias = "j"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.providers.alias_map().unwrap_err().to_string();
        assert!(err.contains("duplicated"));
    }

    #[test]
    fn supports_flag_param_type_with_boolean_defaults() {
        let raw = r#"
[[commands]]
name = "Deploy"
run = "./deploy {{force}}"

[[commands.params]]
name = "force"
type = "flag"
default = false
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.commands[0].params.len(), 1);
        assert_eq!(cfg.commands[0].params[0].r#type, ParamTypeConfig::Flag);
        assert!(matches!(
            cfg.commands[0].params[0].default,
            Some(ParamLiteralConfig::Bool(false))
        ));
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandConfig {
    pub name: String,
    #[serde(alias = "cmd")]
    pub run: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub params: Vec<ParamConfig>,
    #[serde(default)]
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ParamTypeConfig {
    #[default]
    Value,
    Flag,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ParamLiteralConfig {
    String(String),
    Bool(bool),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ParamConfig {
    pub name: String,
    #[serde(default)]
    pub r#type: ParamTypeConfig,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub default: Option<ParamLiteralConfig>,
    #[serde(default)]
    pub value: Option<ParamLiteralConfig>,
    #[serde(default)]
    pub required: bool,
}

pub fn load(cwd: &Path, explicit_path: Option<&Path>) -> Result<LoadedConfig> {
    if let Some(path) = explicit_path {
        return Ok(LoadedConfig {
            config: load_from_path(path)?,
            path: Some(path.to_path_buf()),
        });
    }

    let local_candidates = [cwd.join("fzc.toml"), cwd.join(".fzc.toml")];
    for path in &local_candidates {
        if path.exists() {
            return Ok(LoadedConfig {
                config: load_from_path(path)?,
                path: Some(path.to_path_buf()),
            });
        }
    }

    let global_path = global_config_path()?;
    if global_path.exists() {
        return Ok(LoadedConfig {
            config: load_from_path(&global_path)?,
            path: Some(global_path),
        });
    }

    Ok(LoadedConfig {
        config: Config::default(),
        path: None,
    })
}

pub fn global_config_path() -> Result<PathBuf> {
    let config_root = dirs::config_dir().context("unable to resolve OS config directory")?;
    Ok(config_root.join("fzc").join("config.toml"))
}

pub fn write_example_config(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists. Use --force to overwrite.",
            path.display()
        );
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    fs::write(path, EXAMPLE_CONFIG)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn load_from_path(path: &Path) -> Result<Config> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("invalid TOML in {}", path.display()))
}

const EXAMPLE_CONFIG: &str = r#"# fzc config
#
# Use {{param}} placeholders inside command `run` templates.
# Parameter types:
# - value (default): free text
# - flag: y/n prompt, renders --name when enabled

[ranking]
usage_enabled = true
usage_weight = 8000

# Load commands from this file (`[[commands]]` blocks)
[providers.config]
enabled = true
alias = "cf"

# Auto-load Laravel artisan commands when inside a Laravel project.
[providers.artisan]
enabled = false
alias = "a"

# Auto-load composer commands and scripts when composer.json is present.
[providers.composer]
enabled = false
alias = "co"

# Auto-load just recipes from a justfile.
[providers.justfile]
enabled = false
path = "justfile"
options = "--working-directory ."
alias = "j"

# Add your own commands below using `[[commands]]`.
# Example:
#
# [[commands]]
# name = "Run tests"
# run = "php artisan test --filter={{filter}} {{no-coverage}}"
# description = "Example command"
# scopes = ["laravel"] # optional
#
# [[commands.params]]
# name = "filter"
# prompt = "Test filter"
# required = true
#
# [[commands.params]]
# name = "no-coverage"
# type = "flag"
# default = false
"#;
