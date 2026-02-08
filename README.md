# fzc

`fzc` is a terminal command launcher with fuzzy search, live output streaming, provider discovery, and a fast
keyboard-first TUI.

## Highlights

- Fuzzy-ranked command search with optional usage-based weighting
- Session history with streamed, ANSI-colored output
- Config-driven commands plus provider-based commands
- Provider filtering via `!`
- Internal commands via `/`

## Quick Start

```bash
cargo build --release
./target/release/fzc
```

```bash
# use auto-discovered config
fzc

# use explicit config
fzc --config /path/to/fzc.toml
```

When running for the first time, run `/init` inside of `fzc` to initialize a default config.

## Config Discovery

1. `--config <path>`
2. `./fzc.toml`
3. `./.fzc.toml`
4. `~/.config/fzc/config.toml`

If no config is found, `fzc` still runs, but providers default to disabled.

## Example Config

```toml
[ranking]
usage_enabled = true
usage_weight = 8000

# Load commands from this file
[providers.config]
enabled = true
alias = "c"

# Auto-load Laravel artisan commands when inside a Laravel project
[providers.artisan]
enabled = false
alias = "a"

# Auto-load just recipes from a justfile
# parameters are currently unsupported
[providers.justfile]
enabled = false
path = "justfile"
options = "--working-directory ."
alias = "j"
```

## Command Reference (TOML)

```toml
# One command entry
[[commands]]
name = "Run tests"                                     # required
run = "php artisan test --filter={{filter}} {{force}}" # required
description = "Run Laravel tests"                      # optional
scopes = ["laravel"]                                   # optional (currently only "laravel" is supported)
working_dir = "."                                      # optional

# Params are attached to the previous [[commands]] entry
[[commands.params]]
name = "filter"           # required; maps to {{filter}}
type = "value"            # optional: "value" (default) or "flag"
prompt = "Test filter"    # optional
placeholder = "UserTest"  # optional
required = true           # optional (value type only)
default = "UserTest"      # optional
# value = "UserTest"      # optional fixed value (no prompt)

[[commands.params]]
name = "force"
type = "flag"
prompt = "Use --force?"   # optional
default = false           # Enter fallback (y/n also works directly)
# value = true            # optional fixed flag value
```

## Providers Reference (TOML)

### Config Provider

```toml
[providers.config]
enabled = true   # load [[commands]] from loaded config
alias = "c"      # optional
```

### Artisan Provider

```toml
[providers.artisan]
enabled = false  # auto-load artisan commands in Laravel projects
alias = "a"      # optional
```

### Justfile Provider

```toml
[providers.justfile]
enabled = false
alias = "j"                          # optional
path = "justfile"                    # default: justfile
options = "--working-directory ."    # optional, string or array
# options = ["--working-directory .", "--unstable"]
```

## Search and Filters

- Type to search commands.
- `/` run internal command
- `!alias query` filter by provider alias
- `!provider query` filter by provider name (if no alias exists)

Internal commands:

- `/reload`: reload config and providers
- `/init`: write starter config and reload
- `/init --force`: overwrite existing starter config

## Keybindings

- `Tab`: toggle active pane (`Commands` <-> `Session`)
- `Up` / `Down`: scroll active pane
- `PgUp` / `PgDn`: scroll active pane faster
- `Left` / `Right` / `Home` / `End`: edit search cursor
- `Enter`: run selected command (`Commands` pane only)
- `Option+Enter`: run selected command and exit
- `?`: toggle help
- `Esc`: clear search, close help, interrupt running command, or quit when search is empty
- `Ctrl+C`: quit

Typing while `Session` is active automatically returns focus to `Commands` and continues search input.
