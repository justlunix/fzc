use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::config::{
    ArtisanProviderConfig, ComposerProviderConfig, JustfileProviderConfig, ProvidersConfig,
};
use crate::model::{CommandEntry, CommandSource};

pub fn load_provider_commands(config: &ProvidersConfig, cwd: &Path) -> Result<Vec<CommandEntry>> {
    let mut commands = Vec::new();

    if config.artisan.enabled {
        commands.extend(load_artisan_provider(cwd, &config.artisan)?);
    }
    if config.composer.enabled {
        commands.extend(load_composer_provider(cwd, &config.composer)?);
    }
    if config.justfile.enabled {
        commands.extend(load_justfile_provider(cwd, &config.justfile)?);
    }

    Ok(commands)
}

fn load_artisan_provider(cwd: &Path, _config: &ArtisanProviderConfig) -> Result<Vec<CommandEntry>> {
    let Some(root) = detect_laravel_root(cwd) else {
        return Ok(Vec::new());
    };

    let raw_list = artisan_list_raw(&root).unwrap_or_default();
    let command_names = parse_artisan_commands(&raw_list);
    let command_descriptions = artisan_descriptions(&root).unwrap_or_default();

    let commands = command_names
        .into_iter()
        .map(|name| CommandEntry {
            name: format!("artisan {name}"),
            description: command_descriptions
                .get(&name)
                .filter(|desc| !desc.trim().is_empty())
                .cloned()
                .or_else(|| Some("Laravel artisan command".to_string())),
            template: format!("php artisan {name} --ansi"),
            params: Vec::new(),
            source: CommandSource::Provider("artisan"),
            working_dir: Some(root.clone()),
        })
        .collect();

    Ok(commands)
}

fn load_justfile_provider(
    cwd: &Path,
    config: &JustfileProviderConfig,
) -> Result<Vec<CommandEntry>> {
    let Some(justfile_path) = resolve_provider_path(cwd, &config.path) else {
        return Ok(Vec::new());
    };

    let option_tokens = tokenize_provider_options(&config.options);
    let raw_list = just_list_summary_raw(&justfile_path, cwd, &option_tokens).unwrap_or_default();
    let recipes = parse_just_recipes(&raw_list);

    let commands = recipes
        .into_iter()
        .map(|recipe| CommandEntry {
            name: format!("just {recipe}"),
            description: Some("just recipe".to_string()),
            template: build_just_command_template(&justfile_path, &option_tokens, &recipe),
            params: Vec::new(),
            source: CommandSource::Provider("justfile"),
            working_dir: Some(cwd.to_path_buf()),
        })
        .collect();

    Ok(commands)
}

fn load_composer_provider(
    cwd: &Path,
    _config: &ComposerProviderConfig,
) -> Result<Vec<CommandEntry>> {
    let Some(root) = detect_composer_root(cwd) else {
        return Ok(Vec::new());
    };

    let mut commands = Vec::new();

    for (name, description) in basic_composer_commands() {
        commands.push(CommandEntry {
            name: format!("composer {name}"),
            description: Some(description.to_string()),
            template: format!("composer {name}"),
            params: Vec::new(),
            source: CommandSource::Provider("composer"),
            working_dir: Some(root.clone()),
        });
    }

    for script in composer_scripts(&root) {
        commands.push(CommandEntry {
            name: format!("composer script:{script}"),
            description: Some("composer script".to_string()),
            template: format!("composer run-script {script}"),
            params: Vec::new(),
            source: CommandSource::Provider("composer"),
            working_dir: Some(root.clone()),
        });
    }

    Ok(commands)
}

fn basic_composer_commands() -> &'static [(&'static str, &'static str)] {
    &[
        ("install", "Install project dependencies"),
        ("update", "Update dependencies"),
        ("dump-autoload", "Regenerate autoloader files"),
        ("validate", "Validate composer.json and composer.lock"),
        ("show", "List installed packages"),
        ("outdated", "Show outdated dependencies"),
        ("audit", "Run security audit on dependencies"),
    ]
}

fn detect_laravel_root(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if dir.join("artisan").is_file() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

fn detect_composer_root(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if dir.join("composer.json").is_file() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

fn composer_scripts(root: &Path) -> Vec<String> {
    let path = root.join("composer.json");
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => return Vec::new(),
    };
    parse_composer_scripts_json(&content)
}

fn parse_composer_scripts_json(raw: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };

    let mut scripts = BTreeSet::new();
    let Some(map) = value.get("scripts").and_then(|value| value.as_object()) else {
        return Vec::new();
    };

    for key in map.keys() {
        let name = key.trim();
        if name.is_empty() || name.starts_with('_') {
            continue;
        }
        scripts.insert(name.to_string());
    }

    scripts.into_iter().collect()
}

fn artisan_list_raw(root: &Path) -> Option<String> {
    let output = Command::new("php")
        .arg("artisan")
        .arg("list")
        .arg("--raw")
        .arg("--no-ansi")
        .current_dir(root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout).ok()
}

fn artisan_descriptions(root: &Path) -> Option<HashMap<String, String>> {
    let output = Command::new("php")
        .arg("artisan")
        .arg("list")
        .arg("--format=json")
        .arg("--no-ansi")
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    Some(parse_artisan_descriptions_json(&raw))
}

fn just_list_summary_raw(
    justfile_path: &Path,
    root: &Path,
    option_tokens: &[String],
) -> Option<String> {
    let mut command = Command::new("just");
    for option in option_tokens {
        command.arg(option);
    }
    let output = command
        .arg("--summary")
        .arg("--justfile")
        .arg(justfile_path)
        .current_dir(root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout).ok()
}

fn parse_artisan_commands(raw: &str) -> Vec<String> {
    let mut commands = BTreeSet::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('_') {
            continue;
        }

        let Some(command) = line.split_whitespace().next() else {
            continue;
        };
        if command.starts_with('_') {
            continue;
        }

        commands.insert(command.to_string());
    }

    commands.into_iter().collect()
}

fn parse_artisan_descriptions_json(raw: &str) -> HashMap<String, String> {
    let mut descriptions = HashMap::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return descriptions;
    };

    if let Some(commands) = value.get("commands") {
        if let Some(array) = commands.as_array() {
            for cmd in array {
                let Some(name) = cmd.get("name").and_then(|v| v.as_str()) else {
                    continue;
                };
                let description = cmd
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                descriptions.insert(name.to_string(), description);
            }
            return descriptions;
        }

        if let Some(map) = commands.as_object() {
            for (name, value) in map {
                let description = value
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                descriptions.insert(name.to_string(), description);
            }
        }
    }

    descriptions
}

fn parse_just_recipes(raw: &str) -> Vec<String> {
    let mut recipes = BTreeSet::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.to_ascii_lowercase().starts_with("available recipes") {
            continue;
        }

        for token in line.split_whitespace() {
            if token == "--" {
                break;
            }

            let name = token.trim_matches(',').trim_matches(':');
            if name.is_empty() || name.starts_with('_') {
                continue;
            }
            if name.eq_ignore_ascii_case("available") || name.eq_ignore_ascii_case("recipes") {
                continue;
            }
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':')
            {
                continue;
            }

            recipes.insert(name.to_string());
        }
    }

    recipes.into_iter().collect()
}

fn resolve_provider_path(cwd: &Path, raw_path: &str) -> Option<PathBuf> {
    let candidate = expand_home_shorthand(raw_path)?;
    if candidate.is_absolute() {
        return candidate.is_file().then_some(candidate);
    }

    for dir in cwd.ancestors() {
        let joined = dir.join(&candidate);
        if joined.is_file() {
            return Some(joined);
        }
    }

    None
}

fn expand_home_shorthand(raw_path: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let starts_with_home =
        raw_path == "~" || raw_path.starts_with("~/") || raw_path.starts_with("~\\");
    #[cfg(not(windows))]
    let starts_with_home = raw_path == "~" || raw_path.starts_with("~/");

    if !starts_with_home {
        return Some(PathBuf::from(raw_path));
    }

    let home = dirs::home_dir()?;
    if raw_path == "~" {
        return Some(home);
    }

    let rest = raw_path.trim_start_matches("~/").trim_start_matches("~\\");
    Some(home.join(rest))
}

fn shell_escape_single_quoted(input: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        return format!("\"{}\"", input.replace('\"', "\\\""));
    }

    #[cfg(not(target_os = "windows"))]
    format!("'{}'", input.replace('\'', "'\\''"))
}

fn tokenize_provider_options(raw_options: &[String]) -> Vec<String> {
    raw_options
        .iter()
        .flat_map(|option| option.split_whitespace())
        .map(ToString::to_string)
        .collect()
}

fn build_just_command_template(
    justfile_path: &Path,
    option_tokens: &[String],
    recipe: &str,
) -> String {
    let mut pieces = Vec::new();
    pieces.push("just".to_string());
    for option in option_tokens {
        pieces.push(shell_escape_arg(option));
    }
    pieces.push("--justfile".to_string());
    pieces.push(shell_escape_arg(&justfile_path.to_string_lossy()));
    pieces.push(shell_escape_arg(recipe));
    pieces.join(" ")
}

fn shell_escape_arg(input: &str) -> String {
    if is_shell_safe_arg(input) {
        return input.to_string();
    }
    shell_escape_single_quoted(input)
}

fn is_shell_safe_arg(input: &str) -> bool {
    !input.is_empty()
        && input.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, '_' | '-' | '.' | '/' | ':' | '=' | '+' | '@' | '%')
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::config::ComposerProviderConfig;

    use super::{
        build_just_command_template, expand_home_shorthand, parse_artisan_commands,
        parse_artisan_descriptions_json, parse_composer_scripts_json, parse_just_recipes,
        resolve_provider_path, shell_escape_arg, tokenize_provider_options,
    };

    #[test]
    fn parses_and_filters_artisan_raw_output() {
        let input =
            "about\nlist\n_foo\ncache:clear [store] [--tags[=TAGS]]\n\nmigrate:fresh {--seed}\n";
        let commands = parse_artisan_commands(input);

        assert_eq!(
            commands,
            vec!["about", "cache:clear", "list", "migrate:fresh"]
        );
    }

    #[test]
    fn parses_artisan_command_descriptions_from_json() {
        let input = r#"{
  "commands": [
    { "name": "about", "description": "Display basic information about your application" },
    { "name": "cache:clear", "description": "Flush the application cache" }
  ]
}"#;
        let descriptions = parse_artisan_descriptions_json(input);
        assert_eq!(
            descriptions.get("cache:clear").map(String::as_str),
            Some("Flush the application cache")
        );
    }

    #[test]
    fn parses_just_summary_output() {
        let input =
            "build check\n_ignored\nAvailable recipes:\nlint -- some description\nmodx::task\n";
        let recipes = parse_just_recipes(input);
        assert_eq!(recipes, vec!["build", "check", "lint", "modx::task"]);
    }

    #[test]
    fn resolves_relative_provider_path_from_ancestors() {
        let root = make_temp_dir();
        let nested = root.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let justfile = root.join("justfile");
        fs::write(&justfile, "default:\\n\\techo hi\\n").unwrap();

        let resolved = resolve_provider_path(&nested, "justfile").unwrap();
        assert_eq!(resolved, justfile);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn expands_home_shorthand_path() {
        let home = dirs::home_dir().unwrap();
        let expanded = expand_home_shorthand("~/nix/justfile").unwrap();
        assert_eq!(expanded, home.join("nix/justfile"));
    }

    #[test]
    fn tokenizes_provider_options_string_values() {
        let tokens = tokenize_provider_options(&[
            "--working-directory .".to_string(),
            "--unstable".to_string(),
        ]);
        assert_eq!(
            tokens,
            vec![
                "--working-directory".to_string(),
                ".".to_string(),
                "--unstable".to_string()
            ]
        );
    }

    #[test]
    fn builds_just_command_template_with_options() {
        let template = build_just_command_template(
            Path::new("/tmp/justfile"),
            &["--working-directory".to_string(), ".".to_string()],
            "build",
        );
        assert!(template.starts_with("just --working-directory ."));
        assert!(template.contains("--justfile"));
        assert!(template.ends_with(" build"));
    }

    #[test]
    fn shell_escape_arg_keeps_plain_flags_unquoted() {
        assert_eq!(
            shell_escape_arg("--working-directory"),
            "--working-directory"
        );
        assert_eq!(shell_escape_arg("."), ".");
        assert_eq!(shell_escape_arg("modx::task"), "modx::task");
        assert_eq!(shell_escape_arg("path with space"), "'path with space'");
    }

    #[test]
    fn parses_composer_scripts_from_json() {
        let raw = r#"{
  "scripts": {
    "test": "phpunit",
    "qa": ["phpstan", "phpunit"],
    "_private": "echo hidden"
  }
}"#;
        let scripts = parse_composer_scripts_json(raw);
        assert_eq!(scripts, vec!["qa".to_string(), "test".to_string()]);
    }

    #[test]
    fn loads_composer_basic_and_script_commands() {
        let root = make_temp_dir();
        let nested = root.join("deep/nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            root.join("composer.json"),
            r#"{"scripts":{"test":"phpunit","qa":"phpstan"}}"#,
        )
        .unwrap();

        let config = ComposerProviderConfig {
            enabled: true,
            alias: Some("p".to_string()),
        };
        let commands = super::load_composer_provider(&nested, &config).unwrap();

        assert!(
            commands
                .iter()
                .any(|command| command.name == "composer install")
        );
        assert!(
            commands
                .iter()
                .any(|command| command.name == "composer script:test")
        );
        assert!(
            commands
                .iter()
                .all(|command| command.working_dir.as_ref() == Some(&root))
        );

        let _ = fs::remove_dir_all(root);
    }

    fn make_temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("fzc-provider-test-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
