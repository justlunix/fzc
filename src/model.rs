use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::Glob;

use crate::config::{
    CommandConfig, LoadedConfig, ParamConfig, ParamLiteralConfig, ParamTypeConfig,
};

#[derive(Debug, Clone)]
pub enum CommandSource {
    Config,
    Provider(&'static str),
}

#[derive(Debug, Clone)]
pub enum ParamType {
    Value,
    Flag,
}

#[derive(Debug, Clone)]
pub struct ParamSpec {
    pub name: String,
    pub kind: ParamType,
    pub prompt: String,
    pub placeholder: Option<String>,
    pub default_value: Option<String>,
    pub value_value: Option<String>,
    pub default_flag: Option<bool>,
    pub value_flag: Option<bool>,
    pub required: bool,
    pub prompt_in_tui: bool,
}

impl ParamSpec {
    pub fn requires_input(&self) -> bool {
        match self.kind {
            ParamType::Value => {
                self.value_value.is_none()
                    && (self.prompt_in_tui || self.required || self.default_value.is_none())
            }
            // Flags are interactive by default unless hardcoded via `value`.
            ParamType::Flag => self.value_flag.is_none(),
        }
    }

    pub fn flag_token(&self) -> String {
        if self.name.starts_with('-') {
            self.name.clone()
        } else {
            format!("--{}", self.name)
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandEntry {
    pub name: String,
    pub description: Option<String>,
    pub template: String,
    pub params: Vec<ParamSpec>,
    pub source: CommandSource,
    pub working_dir: Option<PathBuf>,
}

pub struct CommandCatalog {
    commands: Vec<CommandEntry>,
}

impl CommandCatalog {
    pub fn empty() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub fn from_config(loaded: &LoadedConfig, cwd: &Path) -> Result<Self> {
        let mut commands = Vec::new();
        for command in &loaded.config.commands {
            if !matches_scope(&command.scopes, cwd)? {
                continue;
            }
            commands.push(command_from_config(command, cwd));
        }
        Ok(Self { commands })
    }

    pub fn extend(&mut self, commands: Vec<CommandEntry>) {
        self.commands.extend(commands);
    }

    pub fn into_vec(self) -> Vec<CommandEntry> {
        self.commands
    }
}

pub fn render_template(template: &str, params: &HashMap<String, String>) -> String {
    let mut output = template.to_owned();
    for (key, value) in params {
        let needle = format!("{{{{{key}}}}}");
        output = output.replace(&needle, value);
    }
    output
}

fn command_from_config(command: &CommandConfig, cwd: &Path) -> CommandEntry {
    let working_dir = command.working_dir.as_ref().map(|raw| {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        }
    });

    CommandEntry {
        name: command.name.clone(),
        description: command.description.clone(),
        template: command.run.clone(),
        params: command.params.iter().map(param_from_config).collect(),
        source: CommandSource::Config,
        working_dir,
    }
}

fn param_from_config(param: &ParamConfig) -> ParamSpec {
    let kind = match param.r#type {
        ParamTypeConfig::Value => ParamType::Value,
        ParamTypeConfig::Flag => ParamType::Flag,
    };

    let default_value = literal_as_string(param.default.as_ref());
    let value_value = literal_as_string(param.value.as_ref());
    let default_flag = literal_as_bool(param.default.as_ref());
    let value_flag = literal_as_bool(param.value.as_ref());
    let default_prompt = match kind {
        ParamType::Value => format!("{}:", param.name),
        ParamType::Flag => {
            let token = if param.name.starts_with('-') {
                param.name.clone()
            } else {
                format!("--{}", param.name)
            };
            format!("Enable {token}?")
        }
    };

    ParamSpec {
        name: param.name.clone(),
        prompt: param.prompt.clone().unwrap_or(default_prompt),
        kind,
        placeholder: param.placeholder.clone(),
        default_value,
        value_value,
        default_flag,
        value_flag,
        required: param.required,
        prompt_in_tui: param.prompt.is_some(),
    }
}

fn literal_as_string(literal: Option<&ParamLiteralConfig>) -> Option<String> {
    match literal {
        Some(ParamLiteralConfig::String(value)) => Some(value.clone()),
        Some(ParamLiteralConfig::Bool(value)) => Some(value.to_string()),
        None => None,
    }
}

fn literal_as_bool(literal: Option<&ParamLiteralConfig>) -> Option<bool> {
    match literal {
        Some(ParamLiteralConfig::Bool(value)) => Some(*value),
        Some(ParamLiteralConfig::String(value)) => parse_bool_string(value),
        None => None,
    }
}

fn parse_bool_string(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "1" | "yes" | "y" | "on" => Some(true),
        "false" | "f" | "0" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

fn matches_scope(patterns: &[String], cwd: &Path) -> Result<bool> {
    if patterns.is_empty() {
        return Ok(true);
    }

    let laravel_root = detect_laravel_root(cwd);
    let composer_root = detect_composer_root(cwd);
    let candidates = scope_match_candidates(cwd, laravel_root.as_deref(), composer_root.as_deref());

    for pattern in patterns {
        if matches_special_scope(pattern, laravel_root.as_deref(), composer_root.as_deref()) {
            return Ok(true);
        }

        let glob =
            Glob::new(pattern).with_context(|| format!("invalid scope pattern: {pattern}"))?;
        let matcher = glob.compile_matcher();
        if candidates
            .iter()
            .any(|candidate| matcher.is_match(candidate))
        {
            return Ok(true);
        }
    }

    Ok(false)
}

fn matches_special_scope(
    pattern: &str,
    laravel_root: Option<&Path>,
    composer_root: Option<&Path>,
) -> bool {
    match pattern.trim().to_ascii_lowercase().as_str() {
        "laravel" | "project:laravel" | "framework:laravel" => laravel_root.is_some(),
        "composer" | "project:composer" | "tool:composer" => composer_root.is_some(),
        _ => false,
    }
}

fn scope_match_candidates(
    cwd: &Path,
    laravel_root: Option<&Path>,
    composer_root: Option<&Path>,
) -> Vec<PathBuf> {
    let mut candidates = vec![cwd.to_path_buf()];
    if let Some(root) = laravel_root {
        candidates.push(root.to_path_buf());
        candidates.push(root.join("app"));
        candidates.push(root.join("app").join("__fzc_scope_marker__"));
        candidates.push(root.join("artisan"));
    }
    if let Some(root) = composer_root {
        candidates.push(root.to_path_buf());
        candidates.push(root.join("composer.json"));
    }
    candidates
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn template_replacement_works() {
        let mut params = HashMap::new();
        params.insert("env".to_string(), "production".to_string());
        params.insert("region".to_string(), "us-east-1".to_string());

        let rendered = render_template("deploy --env={{env}} --region={{region}}", &params);
        assert_eq!(rendered, "deploy --env=production --region=us-east-1");
    }

    #[test]
    fn scope_matching_works() {
        let cwd = Path::new("/Users/me/projects/laravel-app");
        let patterns = vec!["**/laravel-app".to_string()];
        assert!(matches_scope(&patterns, cwd).unwrap());
    }

    #[test]
    fn laravel_literal_scope_matches_when_artisan_exists() {
        let root = make_temp_dir();
        fs::write(root.join("artisan"), "#!/usr/bin/env php").unwrap();

        let patterns = vec!["laravel".to_string()];
        assert!(matches_scope(&patterns, &root).unwrap());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn composer_literal_scope_matches_when_composer_exists() {
        let root = make_temp_dir();
        fs::write(root.join("composer.json"), r#"{"name":"example/app"}"#).unwrap();

        let patterns = vec!["composer".to_string()];
        assert!(matches_scope(&patterns, &root).unwrap());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn init_app_glob_scope_matches_laravel_root() {
        let root = make_temp_dir();
        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(root.join("artisan"), "#!/usr/bin/env php").unwrap();

        let patterns = vec!["**/app/**".to_string()];
        assert!(matches_scope(&patterns, &root).unwrap());

        let _ = fs::remove_dir_all(root);
    }

    fn make_temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("fzc-scope-test-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
