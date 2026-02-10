use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use serde::{Deserialize, Serialize};

use crate::model::{CommandCatalog, CommandEntry, CommandSource, ParamType, render_template};
use crate::{config, provider};

const MAX_CHAT_LINES: usize = 600;
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug, Clone, Copy)]
pub struct RankingSettings {
    pub usage_enabled: bool,
    pub usage_weight: i64,
}

#[derive(Debug, Clone)]
pub struct RuntimeContext {
    pub cwd: PathBuf,
    pub explicit_config_path: Option<PathBuf>,
}

struct ReloadPayload {
    commands: Vec<CommandEntry>,
    config_path: Option<PathBuf>,
    provider_aliases: HashMap<String, String>,
    ranking: RankingSettings,
}

enum InternalTaskResult {
    Reloaded(ReloadPayload),
    Inited {
        path: PathBuf,
        payload: ReloadPayload,
    },
    Error(String),
}

pub fn run_tui(
    commands: Vec<CommandEntry>,
    config_path: Option<&Path>,
    provider_aliases: HashMap<String, String>,
    ranking: RankingSettings,
    runtime: RuntimeContext,
) -> Result<()> {
    let mut terminal = init_terminal()?;
    let mut app = AppState::new(
        commands,
        config_path.map(Path::to_path_buf),
        provider_aliases,
        ranking,
        runtime,
    );

    match run_loop(&mut terminal, &mut app) {
        Ok(LoopExit::NeedsRestore) => {
            restore_terminal(&mut terminal)?;
            Ok(())
        }
        Ok(LoopExit::AlreadyRestored) => Ok(()),
        Err(err) => {
            let _ = restore_terminal(&mut terminal);
            Err(err)
        }
    }
}

fn init_terminal() -> Result<TuiTerminal> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("failed to create terminal")
}

fn restore_terminal(terminal: &mut TuiTerminal) -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)
        .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to show cursor")
}

fn run_loop(terminal: &mut TuiTerminal, app: &mut AppState) -> Result<LoopExit> {
    loop {
        terminal.draw(|frame| draw_ui(frame, app))?;

        if event::poll(Duration::from_millis(100))? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match app.on_key(key) {
                UiAction::None => {}
                UiAction::Quit => break,
                UiAction::Run(request) => {
                    // Force a redraw before execution so prompt popups disappear immediately.
                    terminal.draw(|frame| draw_ui(frame, app))?;
                    match execute_command(terminal, app, request)? {
                        CommandExec::Continue => {}
                        CommandExec::ExitAlreadyRestored => return Ok(LoopExit::AlreadyRestored),
                    }
                }
                UiAction::RunInternal(request) => {
                    terminal.draw(|frame| draw_ui(frame, app))?;
                    execute_internal_command(terminal, app, request)?;
                }
            }
        }
    }

    Ok(LoopExit::NeedsRestore)
}

fn execute_command(
    terminal: &mut TuiTerminal,
    app: &mut AppState,
    request: RunRequest,
) -> Result<CommandExec> {
    app.mode = Mode::Search;

    if !request.return_to_tui {
        restore_terminal(terminal)?;

        println!();
        println!("fzc: {}", request.display_name);
        if let Some(dir) = &request.working_dir {
            println!("working directory: {}", dir.display());
        }
        println!("$ {}", request.command_line);
        println!();

        let run_result =
            run_shell_command_inherit(&request.command_line, request.working_dir.as_deref());
        match &run_result {
            Ok(code) => println!("exit code: {code}"),
            Err(err) => println!("execution failed: {err:#}"),
        }
        app.record_usage(&request.usage_key);

        return Ok(CommandExec::ExitAlreadyRestored);
    }

    app.push_command(request.command_line.clone());
    if let Some(dir) = &request.working_dir {
        app.push_info(format!("working directory: {}", dir.display()));
    }
    app.start_loading(&request.display_name);
    terminal.draw(|frame| draw_ui(frame, app))?;

    let run_result = run_shell_command_streaming(
        terminal,
        app,
        &request.command_line,
        request.working_dir.as_deref(),
    );
    match run_result {
        Ok(result) => {
            if result.interrupted {
                app.push_info("Interrupted by user (Escape)");
            } else {
                app.push_info(format!("exit code: {}", result.exit_code));
            }
        }
        Err(err) => app.push_error(format!("execution failed: {err:#}")),
    }
    app.stop_loading();
    app.record_usage(&request.usage_key);

    Ok(CommandExec::Continue)
}

fn execute_internal_command(
    terminal: &mut TuiTerminal,
    app: &mut AppState,
    request: InternalRunRequest,
) -> Result<()> {
    app.mode = Mode::Search;
    app.query.clear();
    app.query_cursor = 0;
    app.refresh_filtered();

    let label = match &request.command {
        InternalCommand::Reload => "/reload",
        InternalCommand::Init { .. } => "/init",
        InternalCommand::Unknown(_) => "internal",
    };
    app.start_loading(label);
    terminal.draw(|frame| draw_ui(frame, app))?;

    let runtime = app.runtime.clone();
    let command = request.command;
    let (tx, rx) = mpsc::channel::<InternalTaskResult>();
    let _worker = thread::spawn(move || {
        let result = run_internal_task(&runtime, command);
        let _ = tx.send(result);
    });

    loop {
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => {
                match result {
                    InternalTaskResult::Reloaded(payload) => {
                        let count = payload.commands.len();
                        app.apply_reload_payload(payload);
                        app.push_info(format!("Reloaded {count} commands"));
                    }
                    InternalTaskResult::Inited { path, payload } => {
                        let count = payload.commands.len();
                        app.apply_reload_payload(payload);
                        app.push_info(format!("Wrote example config: {}", path.display()));
                        app.push_info(format!("Reloaded {count} commands"));
                    }
                    InternalTaskResult::Error(err) => app.push_error(err),
                }
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                app.tick_loading();
                terminal.draw(|frame| draw_ui(frame, app))?;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                app.push_error("internal command worker disconnected unexpectedly");
                break;
            }
        }
    }

    app.stop_loading();
    Ok(())
}

fn run_internal_task(runtime: &RuntimeContext, command: InternalCommand) -> InternalTaskResult {
    match command {
        InternalCommand::Reload => match load_catalog_payload(runtime) {
            Ok(payload) => InternalTaskResult::Reloaded(payload),
            Err(err) => InternalTaskResult::Error(format!("reload failed: {err:#}")),
        },
        InternalCommand::Init { force } => match config::global_config_path() {
            Ok(path) => match config::write_example_config(&path, force) {
                Ok(()) => match load_catalog_payload(runtime) {
                    Ok(payload) => InternalTaskResult::Inited { path, payload },
                    Err(err) => InternalTaskResult::Error(format!("reload failed: {err:#}")),
                },
                Err(err) => InternalTaskResult::Error(format!("init failed: {err:#}")),
            },
            Err(err) => InternalTaskResult::Error(format!("init failed: {err:#}")),
        },
        InternalCommand::Unknown(name) => InternalTaskResult::Error(format!(
            "Unknown internal command '/{name}'. Available: /reload, /init"
        )),
    }
}

fn load_catalog_payload(runtime: &RuntimeContext) -> Result<ReloadPayload> {
    let loaded = config::load(&runtime.cwd, runtime.explicit_config_path.as_deref())?;
    let provider_aliases = loaded.config.providers.alias_map()?;

    let mut catalog = CommandCatalog::empty();
    if loaded.config.providers.config.enabled {
        catalog.extend(CommandCatalog::from_config(&loaded, &runtime.cwd)?.into_vec());
    }
    catalog.extend(provider::load_provider_commands(
        &loaded.config.providers,
        &runtime.cwd,
    )?);

    let mut commands = catalog.into_vec();
    commands.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    Ok(ReloadPayload {
        commands,
        config_path: loaded.path,
        provider_aliases,
        ranking: RankingSettings {
            usage_enabled: loaded.config.ranking.usage_enabled,
            usage_weight: loaded.config.ranking.usage_weight,
        },
    })
}

fn run_shell_command_inherit(command: &str, working_dir: Option<&Path>) -> Result<i32> {
    #[cfg(target_os = "windows")]
    let mut process = {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    };

    #[cfg(not(target_os = "windows"))]
    let mut process = {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };

    if let Some(dir) = working_dir {
        process.current_dir(dir);
    }
    apply_color_env(&mut process);

    let status = process
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to start shell command: {command}"))?;

    Ok(status.code().unwrap_or_default())
}

fn run_shell_command_streaming(
    terminal: &mut TuiTerminal,
    app: &mut AppState,
    command: &str,
    working_dir: Option<&Path>,
) -> Result<StreamRunResult> {
    #[cfg(target_os = "windows")]
    let mut process = {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    };

    #[cfg(not(target_os = "windows"))]
    let mut process = {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };

    if let Some(dir) = working_dir {
        process.current_dir(dir);
    }
    apply_color_env(&mut process);

    process.stdin(Stdio::null());
    process.stdout(Stdio::piped());
    process.stderr(Stdio::piped());

    let mut child = process
        .spawn()
        .with_context(|| format!("failed to start shell command: {command}"))?;

    let stdout = child
        .stdout
        .take()
        .context("failed to capture stdout from command process")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture stderr from command process")?;

    let (tx, rx) = mpsc::channel::<StreamChunk>();
    let _stdout_reader = spawn_stream_reader(stdout, ChatLineKind::Stdout, tx.clone());
    let _stderr_reader = spawn_stream_reader(stderr, ChatLineKind::Stderr, tx.clone());
    drop(tx);

    loop {
        if should_interrupt_running_command()? {
            let _ = child.kill();
            let _ = child.wait();

            while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(10)) {
                app.push_line(chunk.kind, chunk.text);
            }
            app.tick_loading();
            terminal.draw(|frame| draw_ui(frame, app))?;

            return Ok(StreamRunResult {
                exit_code: 130,
                interrupted: true,
            });
        }

        while let Ok(chunk) = rx.try_recv() {
            app.push_line(chunk.kind, chunk.text);
            app.tick_loading();
            terminal.draw(|frame| draw_ui(frame, app))?;
        }

        if let Some(status) = child.try_wait()? {
            while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(10)) {
                app.push_line(chunk.kind, chunk.text);
            }
            app.tick_loading();
            terminal.draw(|frame| draw_ui(frame, app))?;

            return Ok(StreamRunResult {
                exit_code: status.code().unwrap_or_default(),
                interrupted: false,
            });
        }

        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(chunk) => {
                app.push_line(chunk.kind, chunk.text);
                app.tick_loading();
                terminal.draw(|frame| draw_ui(frame, app))?;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                app.tick_loading();
                terminal.draw(|frame| draw_ui(frame, app))?;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }
    }
}

fn should_interrupt_running_command() -> Result<bool> {
    if !event::poll(Duration::from_millis(0))? {
        return Ok(false);
    }

    let Event::Key(key) = event::read()? else {
        return Ok(false);
    };
    if key.kind != KeyEventKind::Press {
        return Ok(false);
    }

    Ok(matches!(key.code, KeyCode::Esc))
}

fn apply_color_env(process: &mut Command) {
    process
        .env("CLICOLOR_FORCE", "1")
        .env("FORCE_COLOR", "1")
        .env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string()),
        );
}

fn spawn_stream_reader<R: io::Read + Send + 'static>(
    reader: R,
    kind: ChatLineKind,
    tx: mpsc::Sender<StreamChunk>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffered = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match buffered.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let text = line.trim_end_matches(['\n', '\r']).to_string();
                    if tx.send(StreamChunk { kind, text }).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
}

fn draw_ui(frame: &mut Frame, app: &AppState) {
    let bottom_height = if app.show_help { 14 } else { 1 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(8),
            Constraint::Length(1),
            Constraint::Length(bottom_height),
        ])
        .split(frame.area());

    draw_chat_panel(frame, app, chunks[0]);
    draw_commands_panel(frame, app, chunks[1]);
    draw_search_bar(frame, app, chunks[2]);
    if app.show_help {
        draw_help_panel(frame, app, chunks[3]);
    } else {
        draw_hint_bar(frame, app, chunks[3]);
    }

    if matches!(app.mode, Mode::Search) && !app.is_loading {
        let x = chunks[2].x.saturating_add(8 + app.query_cursor as u16);
        let y = chunks[2].y;
        frame.set_cursor_position((x, y));
    }

    match &app.mode {
        Mode::Prompt(prompt) => draw_prompt_popup(frame, app, prompt),
        Mode::InternalPrompt(prompt) => draw_internal_prompt_popup(frame, app, prompt),
        Mode::Search => {}
    }
}

fn draw_chat_panel(frame: &mut Frame, app: &AppState, area: Rect) {
    let max_lines = area.height.saturating_sub(2) as usize;
    let visible = max_lines.max(1);
    let max_offset = app.chat.len().saturating_sub(visible);
    let offset = app.session_scroll.min(max_offset);
    let start = app
        .chat
        .len()
        .saturating_sub(visible.saturating_add(offset));

    let items: Vec<ListItem<'_>> = app.chat.iter().skip(start).map(render_chat_line).collect();

    let border_color = if app.active_pane == ActivePane::Session {
        Color::Rgb(88, 150, 201)
    } else {
        Color::Rgb(70, 84, 96)
    };
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(if app.active_pane == ActivePane::Session {
                "Session [active]"
            } else {
                "Session"
            })
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color)),
    );

    frame.render_widget(list, area);
}

fn render_chat_line(entry: &ChatLine) -> ListItem<'static> {
    match entry.kind {
        ChatLineKind::Info => {
            let style = Style::default().fg(Color::Gray);
            ListItem::new(Line::from(vec![
                Span::styled("• ".to_string(), style),
                Span::styled(entry.text.clone(), style),
            ]))
        }
        ChatLineKind::Command => {
            let style = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            ListItem::new(Line::from(vec![
                Span::styled("$ ".to_string(), style),
                Span::styled(entry.text.clone(), style),
            ]))
        }
        ChatLineKind::Stdout => {
            let prefix_style = Style::default().fg(Color::DarkGray);
            let default_style = Style::default().fg(Color::White);
            let mut spans = vec![Span::styled("  ".to_string(), prefix_style)];
            spans.extend(parse_ansi_spans(&entry.text, default_style, Color::White));
            ListItem::new(Line::from(spans))
        }
        ChatLineKind::Stderr => {
            let prefix_style = Style::default().fg(Color::DarkGray);
            let default_style = Style::default().fg(Color::LightRed);
            let mut spans = vec![Span::styled("! ".to_string(), prefix_style)];
            spans.extend(parse_ansi_spans(
                &entry.text,
                default_style,
                Color::LightRed,
            ));
            ListItem::new(Line::from(spans))
        }
    }
}

fn parse_ansi_spans(text: &str, default_style: Style, default_fg: Color) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut style = default_style;
    let mut buffer = String::new();
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && matches!(chars.peek(), Some('[')) {
            chars.next();
            if !buffer.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buffer), style));
            }

            let mut seq = String::new();
            while let Some(next) = chars.next() {
                if next == 'm' {
                    apply_sgr_sequence(&seq, &mut style, default_style, default_fg);
                    break;
                }
                if next.is_ascii_digit() || next == ';' {
                    seq.push(next);
                } else {
                    break;
                }
            }
            continue;
        }

        buffer.push(ch);
    }

    if !buffer.is_empty() {
        spans.push(Span::styled(buffer, style));
    }

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), default_style));
    }

    spans
}

fn apply_sgr_sequence(seq: &str, style: &mut Style, default_style: Style, default_fg: Color) {
    if seq.is_empty() {
        *style = default_style;
        return;
    }

    let params: Vec<u16> = seq
        .split(';')
        .filter_map(|part| part.parse::<u16>().ok())
        .collect();
    if params.is_empty() {
        *style = default_style;
        return;
    }

    let mut i = 0usize;
    while i < params.len() {
        let code = params[i];
        match code {
            0 => *style = default_style,
            1 => *style = style.add_modifier(Modifier::BOLD),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            30..=37 => *style = style.fg(map_ansi_color(code)),
            39 => *style = style.fg(default_fg),
            40..=47 => *style = style.bg(map_ansi_color(code - 10)),
            49 => *style = style.bg(Color::Reset),
            90..=97 => *style = style.fg(map_ansi_color(code)),
            100..=107 => *style = style.bg(map_ansi_color(code - 10)),
            38 | 48 => {
                let is_fg = code == 38;
                if i + 2 < params.len() && params[i + 1] == 5 {
                    let color = Color::Indexed((params[i + 2].min(u8::MAX as u16)) as u8);
                    *style = if is_fg {
                        style.fg(color)
                    } else {
                        style.bg(color)
                    };
                    i += 2;
                } else if i + 4 < params.len() && params[i + 1] == 2 {
                    let r = (params[i + 2].min(u8::MAX as u16)) as u8;
                    let g = (params[i + 3].min(u8::MAX as u16)) as u8;
                    let b = (params[i + 4].min(u8::MAX as u16)) as u8;
                    let color = Color::Rgb(r, g, b);
                    *style = if is_fg {
                        style.fg(color)
                    } else {
                        style.bg(color)
                    };
                    i += 4;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

fn map_ansi_color(code: u16) -> Color {
    match code {
        30 => Color::Black,
        31 => Color::Red,
        32 => Color::Green,
        33 => Color::Yellow,
        34 => Color::Blue,
        35 => Color::Magenta,
        36 => Color::Cyan,
        37 => Color::Gray,
        90 => Color::DarkGray,
        91 => Color::LightRed,
        92 => Color::LightGreen,
        93 => Color::LightYellow,
        94 => Color::LightBlue,
        95 => Color::LightMagenta,
        96 => Color::LightCyan,
        97 => Color::White,
        _ => Color::Reset,
    }
}

fn draw_commands_panel(frame: &mut Frame, app: &AppState, area: Rect) {
    let total = if app.is_internal_query() {
        app.internal_commands.len()
    } else {
        app.commands.len()
    };
    let title = if app.active_pane == ActivePane::Commands {
        format!("Commands ({}/{total}) [active]", app.filtered.len())
    } else {
        format!("Commands ({}/{total})", app.filtered.len())
    };
    let border_color = if app.active_pane == ActivePane::Commands {
        Color::Rgb(88, 150, 201)
    } else {
        Color::Rgb(70, 84, 96)
    };

    if app.filtered.is_empty() {
        let empty = Paragraph::new("No matching commands")
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
        frame.render_widget(empty, area);
        return;
    }

    let items: Vec<ListItem<'_>> = app
        .filtered
        .iter()
        .map(|item| match item {
            SearchItem::Command(index) => {
                let command = &app.commands[*index];
                let provider_name = command_provider_name(command);
                let provider_badge = app
                    .provider_alias_by_name
                    .get(provider_name)
                    .cloned()
                    .unwrap_or_else(|| provider_name.to_string());
                let display_name = display_command_name(command, provider_name);

                let mut spans = vec![
                    Span::styled(
                        format!("[{provider_badge}] "),
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(display_name, Style::default().fg(Color::White)),
                ];

                if let Some(description) = &command.description {
                    spans.push(Span::styled(
                        format!(" | {description}"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }

                ListItem::new(Line::from(spans))
            }
            SearchItem::Internal(index) => {
                let internal = &app.internal_commands[*index];
                let spans = vec![
                    Span::styled(
                        "[internal] ".to_string(),
                        Style::default()
                            .fg(Color::LightGreen)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(internal.name.to_string(), Style::default().fg(Color::White)),
                    Span::styled(
                        format!(" | {}", internal.description),
                        Style::default().fg(Color::DarkGray),
                    ),
                ];
                ListItem::new(Line::from(spans))
            }
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(app.selected));

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border_color)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(42, 88, 116))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");

    frame.render_stateful_widget(list, area, &mut list_state);
}

fn draw_search_bar(frame: &mut Frame, app: &AppState, area: Rect) {
    let search_text = if app.is_loading {
        let label = app.loading_label.as_deref().unwrap_or("command");
        format!(
            "Search: {} Running {} (Esc to interrupt)",
            app.spinner_frame(),
            label
        )
    } else if app.active_pane == ActivePane::Session {
        format!("Search: {}  [session active]", app.query)
    } else {
        format!("Search: {}", app.query)
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            search_text,
            Style::default().fg(Color::White),
        ))),
        area,
    );
}

fn draw_hint_bar(frame: &mut Frame, app: &AppState, area: Rect) {
    let text = if app.show_help {
        "  Press ? or Esc to close help"
    } else if app.is_loading {
        "  Esc to interrupt"
    } else {
        "  ? for help"
    };
    let hint = Paragraph::new(text)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Left);
    frame.render_widget(hint, area);
}

fn draw_help_panel(frame: &mut Frame, _app: &AppState, area: Rect) {
    let rows = vec![
        Line::from("  Enter          Run selected command"),
        Line::from("  Option+Enter   Run selected command and exit"),
        Line::from("  Tab            Toggle command/session focus"),
        Line::from("  Up/Down        Scroll active pane"),
        Line::from("  PgUp/PgDn      Scroll active pane faster"),
        Line::from("  Left/Right     Move cursor in search input"),
        Line::from("  Home/End       Jump cursor in search input"),
        Line::from("  Backspace/Del  Edit search input"),
        Line::from("  :provider text Filter by provider"),
        Line::from("  /              Internal commands"),
        Line::from("  ?              Toggle this help"),
        Line::from("  Esc            Clear search / quit / interrupt running command"),
    ];
    let content = Paragraph::new(rows).alignment(Alignment::Left).block(
        Block::default()
            .borders(Borders::NONE)
            .style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(content, area);
}

fn draw_prompt_popup(frame: &mut Frame, app: &AppState, prompt: &PromptState) {
    let area = centered_rect(70, 30, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title("Parameter")
            .style(Style::default().bg(Color::Black)),
        area,
    );

    let command = &app.commands[prompt.command_index];
    let param_idx = prompt.pending_params[prompt.current_param];
    let param = &command.params[param_idx];

    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .margin(1)
        .split(area);

    let heading = format!(
        "{} ({}/{})",
        param.prompt,
        prompt.current_param + 1,
        prompt.pending_params.len()
    );
    frame.render_widget(Paragraph::new(heading), body[0]);

    let helper_text = match param.kind {
        ParamType::Value => param
            .placeholder
            .as_deref()
            .or(param.default_value.as_deref())
            .map(|value| format!("placeholder: {value}"))
            .unwrap_or_default(),
        ParamType::Flag => {
            let default = if param.default_flag.unwrap_or(false) {
                "yes"
            } else {
                "no"
            };
            format!("answer: y/n (Enter = {default})")
        }
    };
    frame.render_widget(Paragraph::new(helper_text), body[1]);

    frame.render_widget(
        Paragraph::new(format!("command: {}", command.name)),
        body[2],
    );

    let input_line = format!("> {}", prompt.input);
    frame.render_widget(Paragraph::new(input_line), body[3]);

    let x = body[3].x.saturating_add(2 + prompt.input.len() as u16);
    let y = body[3].y;
    frame.set_cursor_position((x, y));
}

fn draw_internal_prompt_popup(frame: &mut Frame, app: &AppState, prompt: &InternalPromptState) {
    let area = centered_rect(70, 30, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title("Internal Parameter")
            .style(Style::default().bg(Color::Black)),
        area,
    );

    let command = &app.internal_commands[prompt.command_index];
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .margin(1)
        .split(area);

    let default = if command.default_force { "yes" } else { "no" };
    frame.render_widget(Paragraph::new("Use --force?"), body[0]);
    frame.render_widget(
        Paragraph::new(format!("answer: y/n (Enter = {default})")),
        body[1],
    );
    frame.render_widget(
        Paragraph::new(format!("command: {}", command.name)),
        body[2],
    );
    frame.render_widget(Paragraph::new(format!("> {}", prompt.input)), body[3]);

    let x = body[3].x.saturating_add(2 + prompt.input.len() as u16);
    let y = body[3].y;
    frame.set_cursor_position((x, y));
}

fn centered_rect(percent_x: u16, percent_y: u16, rect: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(rect);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

enum UiAction {
    None,
    Quit,
    Run(RunRequest),
    RunInternal(InternalRunRequest),
}

enum LoopExit {
    NeedsRestore,
    AlreadyRestored,
}

enum CommandExec {
    Continue,
    ExitAlreadyRestored,
}

struct StreamRunResult {
    exit_code: i32,
    interrupted: bool,
}

struct RunRequest {
    display_name: String,
    command_line: String,
    working_dir: Option<PathBuf>,
    usage_key: String,
    return_to_tui: bool,
}

struct InternalRunRequest {
    command: InternalCommand,
}

enum Mode {
    Search,
    Prompt(PromptState),
    InternalPrompt(InternalPromptState),
}

struct PromptState {
    command_index: usize,
    pending_params: Vec<usize>,
    current_param: usize,
    input: String,
    values: HashMap<String, String>,
    return_to_tui: bool,
}

struct InternalPromptState {
    command_index: usize,
    input: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActivePane {
    Commands,
    Session,
}

#[derive(Clone, Copy)]
enum ChatLineKind {
    Info,
    Command,
    Stdout,
    Stderr,
}

struct ChatLine {
    kind: ChatLineKind,
    text: String,
}

struct StreamChunk {
    kind: ChatLineKind,
    text: String,
}

#[derive(Clone)]
enum InternalCommand {
    Reload,
    Init { force: bool },
    Unknown(String),
}

#[derive(Clone, Copy)]
enum InternalCommandKind {
    Reload,
    Init,
}

struct InternalCommandDef {
    name: &'static str,
    description: &'static str,
    kind: InternalCommandKind,
    default_force: bool,
}

#[derive(Clone, Copy)]
enum SearchItem {
    Command(usize),
    Internal(usize),
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UsageStore {
    #[serde(default)]
    counts: HashMap<String, u64>,
}

struct AppState {
    commands: Vec<CommandEntry>,
    filtered: Vec<SearchItem>,
    internal_commands: Vec<InternalCommandDef>,
    selected: usize,
    query: String,
    query_cursor: usize,
    matcher: SkimMatcherV2,
    mode: Mode,
    chat: Vec<ChatLine>,
    config_path: Option<PathBuf>,
    provider_aliases: HashMap<String, String>,
    provider_alias_by_name: HashMap<String, String>,
    provider_names_without_alias: HashSet<String>,
    ranking: RankingSettings,
    usage_counts: HashMap<String, u64>,
    usage_path: Option<PathBuf>,
    is_loading: bool,
    loading_label: Option<String>,
    spinner_index: usize,
    show_help: bool,
    runtime: RuntimeContext,
    active_pane: ActivePane,
    session_scroll: usize,
}

impl AppState {
    fn new(
        mut commands: Vec<CommandEntry>,
        config_path: Option<PathBuf>,
        provider_aliases: HashMap<String, String>,
        ranking: RankingSettings,
        runtime: RuntimeContext,
    ) -> Self {
        commands.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        let count = commands.len();
        let provider_alias_by_name = provider_aliases
            .iter()
            .map(|(alias, provider)| (provider.clone(), alias.clone()))
            .collect();
        let provider_names_without_alias =
            provider_names_without_alias(&commands, &provider_alias_by_name);
        let (usage_counts, usage_path) = load_usage_store();
        let mut app = Self {
            commands,
            filtered: Vec::new(),
            internal_commands: vec![
                InternalCommandDef {
                    name: "/init",
                    description: "Create default config file",
                    kind: InternalCommandKind::Init,
                    default_force: false,
                },
                InternalCommandDef {
                    name: "/reload",
                    description: "Reload config and providers",
                    kind: InternalCommandKind::Reload,
                    default_force: false,
                },
            ],
            selected: 0,
            query: String::new(),
            query_cursor: 0,
            matcher: SkimMatcherV2::default(),
            mode: Mode::Search,
            chat: Vec::new(),
            config_path,
            provider_aliases,
            provider_alias_by_name,
            provider_names_without_alias,
            ranking,
            usage_counts,
            usage_path,
            is_loading: false,
            loading_label: None,
            spinner_index: 0,
            show_help: false,
            runtime,
            active_pane: ActivePane::Commands,
            session_scroll: 0,
        };

        app.refresh_filtered();
        app.push_info(format!("Loaded {count} commands"));
        if let Some(path) = &app.config_path {
            app.push_info(format!("Config: {}", path.display()));
        } else {
            app.push_info("Config: none (providers only or defaults)");
        }

        app
    }

    fn on_key(&mut self, key: KeyEvent) -> UiAction {
        if self.show_help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') => {
                    self.show_help = false;
                    return UiAction::None;
                }
                _ => {
                    // Auto-close help and continue handling the key normally.
                    self.show_help = false;
                }
            }
        }

        match self.mode {
            Mode::Search => self.on_search_key(key),
            Mode::Prompt(_) => self.on_prompt_key(key),
            Mode::InternalPrompt(_) => self.on_internal_prompt_key(key),
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) -> UiAction {
        if matches!(key.code, KeyCode::Char('?')) {
            self.show_help = true;
            return UiAction::None;
        }

        if matches!(key.code, KeyCode::Tab) {
            self.active_pane = match self.active_pane {
                ActivePane::Commands => ActivePane::Session,
                ActivePane::Session => ActivePane::Commands,
            };
            return UiAction::None;
        }

        match key.code {
            KeyCode::Esc => {
                if self.query.is_empty() {
                    UiAction::Quit
                } else {
                    self.query.clear();
                    self.query_cursor = 0;
                    self.refresh_filtered();
                    self.active_pane = ActivePane::Commands;
                    UiAction::None
                }
            }
            KeyCode::Enter => {
                if self.active_pane == ActivePane::Session {
                    return UiAction::None;
                }
                if self.is_internal_query() {
                    return self.prepare_selected_internal_command();
                }
                let return_to_tui = !key.modifiers.contains(KeyModifiers::ALT);
                self.prepare_selected_command(return_to_tui)
            }
            KeyCode::Left => {
                if self.query_cursor > 0 {
                    self.query_cursor -= 1;
                }
                self.active_pane = ActivePane::Commands;
                UiAction::None
            }
            KeyCode::Right => {
                let len = self.query.chars().count();
                if self.query_cursor < len {
                    self.query_cursor += 1;
                }
                self.active_pane = ActivePane::Commands;
                UiAction::None
            }
            KeyCode::Home => {
                self.query_cursor = 0;
                self.active_pane = ActivePane::Commands;
                UiAction::None
            }
            KeyCode::End => {
                self.query_cursor = self.query.chars().count();
                self.active_pane = ActivePane::Commands;
                UiAction::None
            }
            KeyCode::Up => {
                if self.active_pane == ActivePane::Session {
                    self.scroll_session(1);
                } else {
                    self.move_selection(-1);
                }
                UiAction::None
            }
            KeyCode::Down => {
                if self.active_pane == ActivePane::Session {
                    self.scroll_session(-1);
                } else {
                    self.move_selection(1);
                }
                UiAction::None
            }
            KeyCode::PageUp => {
                if self.active_pane == ActivePane::Session {
                    self.scroll_session(10);
                } else {
                    self.move_selection_by(-10);
                }
                UiAction::None
            }
            KeyCode::PageDown => {
                if self.active_pane == ActivePane::Session {
                    self.scroll_session(-10);
                } else {
                    self.move_selection_by(10);
                }
                UiAction::None
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.active_pane == ActivePane::Session {
                    self.scroll_session(-1);
                } else {
                    self.move_selection(1);
                }
                UiAction::None
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.active_pane == ActivePane::Session {
                    self.scroll_session(1);
                } else {
                    self.move_selection(-1);
                }
                UiAction::None
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => UiAction::Quit,
            KeyCode::Backspace => {
                if self.query_cursor > 0 && remove_char_at(&mut self.query, self.query_cursor - 1) {
                    self.query_cursor -= 1;
                    self.refresh_filtered();
                }
                self.active_pane = ActivePane::Commands;
                UiAction::None
            }
            KeyCode::Delete => {
                if remove_char_at(&mut self.query, self.query_cursor) {
                    self.refresh_filtered();
                }
                self.active_pane = ActivePane::Commands;
                UiAction::None
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.active_pane = ActivePane::Commands;
                insert_char_at(&mut self.query, self.query_cursor, ch);
                self.query_cursor += 1;
                self.refresh_filtered();
                UiAction::None
            }
            _ => UiAction::None,
        }
    }

    fn on_prompt_key(&mut self, key: KeyEvent) -> UiAction {
        let mut prompt_state = match std::mem::replace(&mut self.mode, Mode::Search) {
            Mode::Prompt(prompt) => prompt,
            Mode::Search => return UiAction::None,
            Mode::InternalPrompt(_) => return UiAction::None,
        };

        match key.code {
            KeyCode::Esc => {
                self.push_info("Parameter entry canceled");
                self.mode = Mode::Search;
                UiAction::None
            }
            KeyCode::Backspace => {
                prompt_state.input.pop();
                self.mode = Mode::Prompt(prompt_state);
                UiAction::None
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                let param_index = prompt_state.pending_params[prompt_state.current_param];
                let param = self.commands[prompt_state.command_index].params[param_index].clone();
                if matches!(param.kind, ParamType::Flag) {
                    let typed = ch.to_string();
                    if let Some(flag_value) =
                        parse_flag_input(&typed, param.default_flag.unwrap_or(false))
                    {
                        let token = if flag_value {
                            param.flag_token()
                        } else {
                            String::new()
                        };
                        prompt_state.values.insert(param.name.clone(), token);
                        prompt_state.current_param += 1;
                        prompt_state.input.clear();

                        if prompt_state.current_param >= prompt_state.pending_params.len() {
                            let index = prompt_state.command_index;
                            let values = prompt_state.values;
                            let return_to_tui = prompt_state.return_to_tui;
                            self.mode = Mode::Search;
                            return self.build_run_request(index, values, return_to_tui);
                        }

                        self.mode = Mode::Prompt(prompt_state);
                        return UiAction::None;
                    }
                }

                prompt_state.input.push(ch);
                self.mode = Mode::Prompt(prompt_state);
                UiAction::None
            }
            KeyCode::Enter => {
                let param_index = prompt_state.pending_params[prompt_state.current_param];
                let param = self.commands[prompt_state.command_index].params[param_index].clone();
                let input = prompt_state.input.trim().to_string();

                match param.kind {
                    ParamType::Value => {
                        let value = if input.is_empty() {
                            if let Some(default) = &param.default_value {
                                default.clone()
                            } else if param.required {
                                self.push_info(format!("'{}' is required", param.name));
                                self.mode = Mode::Prompt(prompt_state);
                                return UiAction::None;
                            } else {
                                String::new()
                            }
                        } else {
                            input
                        };

                        if !value.is_empty() {
                            prompt_state.values.insert(param.name.clone(), value);
                        }
                    }
                    ParamType::Flag => {
                        let default = param.default_flag.unwrap_or(false);
                        let Some(flag_value) = parse_flag_input(&input, default) else {
                            self.push_info("Please enter y or n");
                            self.mode = Mode::Prompt(prompt_state);
                            return UiAction::None;
                        };
                        let token = if flag_value {
                            param.flag_token()
                        } else {
                            String::new()
                        };
                        prompt_state.values.insert(param.name.clone(), token);
                    }
                }

                prompt_state.current_param += 1;
                prompt_state.input.clear();

                if prompt_state.current_param >= prompt_state.pending_params.len() {
                    let index = prompt_state.command_index;
                    let values = prompt_state.values;
                    let return_to_tui = prompt_state.return_to_tui;
                    self.mode = Mode::Search;
                    self.build_run_request(index, values, return_to_tui)
                } else {
                    self.mode = Mode::Prompt(prompt_state);
                    UiAction::None
                }
            }
            _ => {
                self.mode = Mode::Prompt(prompt_state);
                UiAction::None
            }
        }
    }

    fn prepare_selected_command(&mut self, return_to_tui: bool) -> UiAction {
        let Some(command_index) = self.current_command_index() else {
            self.push_info("No command selected");
            return UiAction::None;
        };

        let command = &self.commands[command_index];
        let mut values = HashMap::new();
        let mut pending_params = Vec::new();

        for (idx, param) in command.params.iter().enumerate() {
            match param.kind {
                ParamType::Value => {
                    if let Some(value) = &param.value_value {
                        values.insert(param.name.clone(), value.clone());
                        continue;
                    }

                    if param.requires_input() {
                        pending_params.push(idx);
                        continue;
                    }

                    if let Some(default) = &param.default_value {
                        values.insert(param.name.clone(), default.clone());
                    }
                }
                ParamType::Flag => {
                    if let Some(value) = param.value_flag {
                        let token = if value {
                            param.flag_token()
                        } else {
                            String::new()
                        };
                        values.insert(param.name.clone(), token);
                        continue;
                    }

                    if param.requires_input() {
                        pending_params.push(idx);
                        continue;
                    }

                    let token = if param.default_flag.unwrap_or(false) {
                        param.flag_token()
                    } else {
                        String::new()
                    };
                    values.insert(param.name.clone(), token);
                }
            }
        }

        if pending_params.is_empty() {
            return self.build_run_request(command_index, values, return_to_tui);
        }

        self.mode = Mode::Prompt(PromptState {
            command_index,
            pending_params,
            current_param: 0,
            input: String::new(),
            values,
            return_to_tui,
        });
        UiAction::None
    }

    fn build_run_request(
        &mut self,
        index: usize,
        values: HashMap<String, String>,
        return_to_tui: bool,
    ) -> UiAction {
        let command = &self.commands[index];
        let rendered = render_template(&command.template, &values);

        if rendered.contains("{{") && rendered.contains("}}") {
            self.push_info(format!(
                "Command '{}' still has unresolved placeholders",
                command.name
            ));
            return UiAction::None;
        }

        let display_name = command.name.clone();
        let working_dir = command.working_dir.clone();
        let usage_key = command_usage_key(command);

        self.query.clear();
        self.query_cursor = 0;
        self.refresh_filtered();

        UiAction::Run(RunRequest {
            display_name,
            command_line: rendered,
            working_dir,
            usage_key,
            return_to_tui,
        })
    }

    fn on_internal_prompt_key(&mut self, key: KeyEvent) -> UiAction {
        let mut prompt_state = match std::mem::replace(&mut self.mode, Mode::Search) {
            Mode::InternalPrompt(prompt) => prompt,
            _ => return UiAction::None,
        };

        match key.code {
            KeyCode::Esc => {
                self.push_info("Internal command canceled");
                self.mode = Mode::Search;
                UiAction::None
            }
            KeyCode::Backspace => {
                prompt_state.input.pop();
                self.mode = Mode::InternalPrompt(prompt_state);
                UiAction::None
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                let typed = ch.to_string();
                let command = &self.internal_commands[prompt_state.command_index];
                let default = command.default_force;
                if let Some(force) = parse_flag_input(&typed, default) {
                    self.mode = Mode::Search;
                    UiAction::RunInternal(InternalRunRequest {
                        command: InternalCommand::Init { force },
                    })
                } else {
                    prompt_state.input.push(ch);
                    self.mode = Mode::InternalPrompt(prompt_state);
                    UiAction::None
                }
            }
            KeyCode::Enter => {
                let command = &self.internal_commands[prompt_state.command_index];
                let default = command.default_force;
                let Some(force) = parse_flag_input(prompt_state.input.trim(), default) else {
                    self.push_info("Please enter y or n");
                    self.mode = Mode::InternalPrompt(prompt_state);
                    return UiAction::None;
                };

                self.mode = Mode::Search;
                UiAction::RunInternal(InternalRunRequest {
                    command: InternalCommand::Init { force },
                })
            }
            _ => {
                self.mode = Mode::InternalPrompt(prompt_state);
                UiAction::None
            }
        }
    }

    fn prepare_selected_internal_command(&mut self) -> UiAction {
        let trimmed = self.query.trim();
        if let Some(parsed) = parse_internal_command(trimmed) {
            match parsed {
                InternalCommand::Reload => {
                    return UiAction::RunInternal(InternalRunRequest {
                        command: InternalCommand::Reload,
                    });
                }
                InternalCommand::Init { force } => {
                    if query_has_force_flag(trimmed) {
                        return UiAction::RunInternal(InternalRunRequest {
                            command: InternalCommand::Init { force },
                        });
                    }
                }
                InternalCommand::Unknown(_) => {}
            }
        }

        let Some(index) = self.current_internal_index() else {
            self.push_info("Unknown internal command. Available: /reload, /init");
            return UiAction::None;
        };

        let command = &self.internal_commands[index];
        match command.kind {
            InternalCommandKind::Reload => UiAction::RunInternal(InternalRunRequest {
                command: InternalCommand::Reload,
            }),
            InternalCommandKind::Init => {
                self.mode = Mode::InternalPrompt(InternalPromptState {
                    command_index: index,
                    input: String::new(),
                });
                UiAction::None
            }
        }
    }

    fn apply_reload_payload(&mut self, payload: ReloadPayload) {
        self.commands = payload.commands;
        self.config_path = payload.config_path;
        self.provider_aliases = payload.provider_aliases;
        self.provider_alias_by_name = self
            .provider_aliases
            .iter()
            .map(|(alias, provider)| (provider.clone(), alias.clone()))
            .collect();
        self.provider_names_without_alias =
            provider_names_without_alias(&self.commands, &self.provider_alias_by_name);
        self.ranking = payload.ranking;
        self.refresh_filtered();
        if self.selected >= self.filtered.len() {
            self.selected = 0;
        }
    }

    fn push_info<S: Into<String>>(&mut self, text: S) {
        self.push_line(ChatLineKind::Info, text.into());
    }

    fn push_command<S: Into<String>>(&mut self, text: S) {
        self.push_line(ChatLineKind::Command, text.into());
    }

    fn push_error<S: Into<String>>(&mut self, text: S) {
        self.push_line(ChatLineKind::Stderr, text.into());
    }

    fn push_line(&mut self, kind: ChatLineKind, text: String) {
        self.chat.push(ChatLine { kind, text });
        if self.active_pane == ActivePane::Commands {
            self.session_scroll = 0;
        }
        if self.chat.len() > MAX_CHAT_LINES {
            let overflow = self.chat.len() - MAX_CHAT_LINES;
            self.chat.drain(0..overflow);
            self.session_scroll = self.session_scroll.saturating_sub(overflow);
        }
    }

    fn start_loading(&mut self, label: &str) {
        self.is_loading = true;
        self.loading_label = Some(label.to_string());
        self.spinner_index = 0;
    }

    fn stop_loading(&mut self) {
        self.is_loading = false;
        self.loading_label = None;
    }

    fn tick_loading(&mut self) {
        if self.is_loading {
            self.spinner_index = (self.spinner_index + 1) % SPINNER_FRAMES.len();
        }
    }

    fn spinner_frame(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_index % SPINNER_FRAMES.len()]
    }

    fn record_usage(&mut self, key: &str) {
        let entry = self.usage_counts.entry(key.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
        let _ = persist_usage_store(&self.usage_counts, self.usage_path.as_deref());
    }

    fn usage_boost_for_command(&self, command: &CommandEntry) -> i64 {
        if !self.ranking.usage_enabled {
            return 0;
        }

        let usage = self
            .usage_counts
            .get(&command_usage_key(command))
            .copied()
            .unwrap_or_default();
        let usage = usage.min(i64::MAX as u64) as i64;
        usage.saturating_mul(self.ranking.usage_weight.max(0))
    }

    fn is_internal_query(&self) -> bool {
        self.query.trim_start().starts_with('/')
    }

    fn current_internal_index(&self) -> Option<usize> {
        match self.filtered.get(self.selected).copied() {
            Some(SearchItem::Internal(index)) => Some(index),
            _ => None,
        }
    }

    fn refresh_filtered(&mut self) {
        if self.is_internal_query() {
            let trimmed = self.query.trim_start();
            let internal_query = trimmed.trim_start_matches('/').trim();
            let normalized = internal_query.to_lowercase();
            let mut scored = Vec::new();

            for (index, command) in self.internal_commands.iter().enumerate() {
                let haystack = format!("{} {}", command.name, command.description).to_lowercase();
                let fuzzy = if normalized.is_empty() {
                    1
                } else {
                    self.matcher
                        .fuzzy_match(&haystack, &normalized)
                        .unwrap_or_default()
                };

                if normalized.is_empty() || fuzzy > 0 || haystack.contains(&normalized) {
                    let contains_bonus = if !normalized.is_empty() && haystack.contains(&normalized)
                    {
                        10_000
                    } else {
                        0
                    };
                    scored.push((
                        index,
                        fuzzy.saturating_mul(10) + contains_bonus,
                        command.name,
                    ));
                }
            }

            scored.sort_by(|a, b| match b.1.cmp(&a.1) {
                Ordering::Equal => a.2.cmp(b.2),
                other => other,
            });
            self.filtered = scored
                .into_iter()
                .map(|(index, _, _)| SearchItem::Internal(index))
                .collect();
            self.selected = 0;
            return;
        }

        let (provider_filter, query, unknown_alias) = parse_query_provider_filter(
            self.query.as_str(),
            &self.provider_aliases,
            &self.provider_names_without_alias,
        );
        if unknown_alias {
            self.filtered.clear();
            self.selected = 0;
            return;
        }

        if query.is_empty() {
            let mut ordered: Vec<(usize, i64, String)> = self
                .commands
                .iter()
                .enumerate()
                .filter(|(_, command)| {
                    provider_filter.is_none_or(|provider| {
                        command_provider_name(command).eq_ignore_ascii_case(provider)
                    })
                })
                .map(|(index, command)| {
                    (
                        index,
                        self.usage_boost_for_command(command),
                        command.name.to_lowercase(),
                    )
                })
                .collect();
            ordered.sort_by(|a, b| match b.1.cmp(&a.1) {
                Ordering::Equal => a.2.cmp(&b.2),
                other => other,
            });
            self.filtered = ordered
                .into_iter()
                .map(|entry| SearchItem::Command(entry.0))
                .collect();
            self.selected = 0;
            return;
        }

        let query_terms = tokenize_for_match(query);
        let mut scored = Vec::new();

        for (index, command) in self.commands.iter().enumerate() {
            if provider_filter.is_some_and(|provider| {
                !command_provider_name(command).eq_ignore_ascii_case(provider)
            }) {
                continue;
            }

            let mut haystack = command.name.clone();
            if let Some(desc) = &command.description {
                haystack.push(' ');
                haystack.push_str(desc);
            }

            if let Some(score) = score_command_match(&self.matcher, query, &query_terms, command) {
                let usage_bonus = self.usage_boost_for_command(command);
                scored.push((
                    index,
                    score.total.saturating_add(usage_bonus),
                    score.fuzzy,
                    command.name.to_lowercase(),
                ));
            }
        }

        scored.sort_by(|a, b| match b.1.cmp(&a.1) {
            Ordering::Equal => match b.2.cmp(&a.2) {
                Ordering::Equal => a.3.cmp(&b.3),
                other => other,
            },
            other => other,
        });

        self.filtered = scored
            .into_iter()
            .map(|entry| SearchItem::Command(entry.0))
            .collect();
        self.selected = 0;
    }

    fn move_selection(&mut self, direction: isize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }

        let len = self.filtered.len() as isize;
        let next = (self.selected as isize + direction).rem_euclid(len);
        self.selected = next as usize;
    }

    fn move_selection_by(&mut self, step: isize) {
        self.move_selection(step);
    }

    fn scroll_session(&mut self, delta: isize) {
        if delta > 0 {
            self.session_scroll = self
                .session_scroll
                .saturating_add(delta as usize)
                .min(self.chat.len().saturating_sub(1));
        } else if delta < 0 {
            self.session_scroll = self.session_scroll.saturating_sub((-delta) as usize);
        }
    }

    fn current_command_index(&self) -> Option<usize> {
        match self.filtered.get(self.selected).copied() {
            Some(SearchItem::Command(index)) => Some(index),
            _ => None,
        }
    }
}

fn provider_names_without_alias(
    commands: &[CommandEntry],
    provider_alias_by_name: &HashMap<String, String>,
) -> HashSet<String> {
    commands
        .iter()
        .map(command_provider_name)
        .filter(|provider| !provider_alias_by_name.contains_key(*provider))
        .map(|provider| provider.to_ascii_lowercase())
        .collect()
}

fn command_provider_name(command: &CommandEntry) -> &'static str {
    match command.source {
        CommandSource::Config => "config",
        CommandSource::Provider(name) => name,
    }
}

fn display_command_name(command: &CommandEntry, provider_name: &str) -> String {
    let prefix = format!("{provider_name} ");
    if command.name.to_ascii_lowercase().starts_with(&prefix) {
        return command.name[prefix.len()..].to_string();
    }
    command.name.clone()
}

fn parse_query_provider_filter<'a>(
    query: &'a str,
    provider_aliases: &'a HashMap<String, String>,
    provider_names_without_alias: &'a HashSet<String>,
) -> (Option<&'a str>, &'a str, bool) {
    let trimmed = query.trim_start();
    if !trimmed.starts_with(':') {
        return (None, query, false);
    }

    let after = &trimmed[1..];
    let alias_end = after.find(char::is_whitespace).unwrap_or(after.len());
    let alias = after[..alias_end].trim().to_ascii_lowercase();
    let remaining = after[alias_end..].trim_start();

    if alias.is_empty() {
        return (None, query, false);
    }

    match provider_aliases.get(&alias) {
        Some(provider) => (Some(provider.as_str()), remaining, false),
        None => match provider_names_without_alias.get(alias.as_str()) {
            Some(provider_name) => (Some(provider_name.as_str()), remaining, false),
            None => (None, remaining, true),
        },
    }
}

fn parse_internal_command(query: &str) -> Option<InternalCommand> {
    let trimmed = query.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed[1..].split_whitespace();
    let name = parts.next().unwrap_or_default().to_ascii_lowercase();
    if name.is_empty() {
        return Some(InternalCommand::Unknown(String::new()));
    }

    match name.as_str() {
        "reload" => Some(InternalCommand::Reload),
        "init" => {
            let force = parts.any(|part| part == "--force" || part == "-f");
            Some(InternalCommand::Init { force })
        }
        _ => Some(InternalCommand::Unknown(name)),
    }
}

fn parse_flag_input(input: &str, default: bool) -> Option<bool> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Some(default);
    }

    match trimmed.to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" | "1" | "on" => Some(true),
        "n" | "no" | "false" | "0" | "off" => Some(false),
        _ => None,
    }
}

fn query_has_force_flag(query: &str) -> bool {
    query
        .split_whitespace()
        .any(|part| part == "--force" || part == "-f")
}

fn command_usage_key(command: &CommandEntry) -> String {
    format!("{}::{}", command_provider_name(command), command.name)
}

fn load_usage_store() -> (HashMap<String, u64>, Option<PathBuf>) {
    let Some(path) = usage_store_path() else {
        return (HashMap::new(), None);
    };
    if !path.exists() {
        return (HashMap::new(), Some(path));
    }

    let counts = fs::read_to_string(&path)
        .ok()
        .and_then(|content| toml::from_str::<UsageStore>(&content).ok())
        .map(|store| store.counts)
        .unwrap_or_default();
    (counts, Some(path))
}

fn persist_usage_store(counts: &HashMap<String, u64>, path: Option<&Path>) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create usage directory {}", parent.display()))?;
    }

    let payload = UsageStore {
        counts: counts.clone(),
    };
    let serialized = toml::to_string(&payload).context("failed to serialize usage store")?;
    fs::write(path, serialized)
        .with_context(|| format!("failed to write usage store {}", path.display()))?;
    Ok(())
}

fn usage_store_path() -> Option<PathBuf> {
    let config_root = dirs::config_dir()?;
    Some(config_root.join("fzc").join("usage.toml"))
}

fn insert_char_at(value: &mut String, char_index: usize, ch: char) {
    let byte_index = byte_index_for_char(value, char_index);
    value.insert(byte_index, ch);
}

fn remove_char_at(value: &mut String, char_index: usize) -> bool {
    let start = byte_index_for_char(value, char_index);
    if start >= value.len() {
        return false;
    }
    let end = byte_index_for_char(value, char_index + 1);
    value.replace_range(start..end, "");
    true
}

fn byte_index_for_char(value: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }
    value
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

#[derive(Debug)]
struct MatchScore {
    total: i64,
    fuzzy: i64,
}

fn score_command_match(
    matcher: &SkimMatcherV2,
    query: &str,
    query_terms: &[String],
    command: &CommandEntry,
) -> Option<MatchScore> {
    let mut haystack = command.name.to_lowercase();
    if let Some(desc) = &command.description {
        haystack.push(' ');
        haystack.push_str(&desc.to_lowercase());
    }

    let normalized_query = query.to_lowercase();
    let fuzzy = matcher
        .fuzzy_match(&haystack, &normalized_query)
        .unwrap_or_default();
    if query_terms.is_empty() {
        return Some(MatchScore {
            total: fuzzy * 10,
            fuzzy,
        });
    }

    let name_terms = tokenize_for_match(&command.name);
    let haystack_terms = tokenize_for_match(&haystack);

    let mut exact_name_hits = 0i64;
    let mut partial_name_hits = 0i64;
    let mut coverage_hits = 0i64;
    let mut description_hits = 0i64;

    for term in query_terms {
        let mut matched_name = false;
        for token in &name_terms {
            match token_match_quality(token, term) {
                2 => {
                    exact_name_hits += 1;
                    matched_name = true;
                    break;
                }
                1 => {
                    partial_name_hits += 1;
                    matched_name = true;
                    break;
                }
                _ => {}
            }
        }
        if matched_name {
            coverage_hits += 1;
            continue;
        }

        if haystack_terms
            .iter()
            .any(|token| token_match_quality(token, term) > 0)
        {
            coverage_hits += 1;
            description_hits += 1;
        }
    }

    if fuzzy == 0 && coverage_hits == 0 {
        return None;
    }

    let all_terms_in_name = query_terms.iter().all(|term| {
        name_terms
            .iter()
            .any(|token| token_match_quality(token, term) > 0)
    });
    let ordered_in_name = terms_in_order(&name_terms, query_terms);
    let contiguous_in_name = terms_contiguous(&name_terms, query_terms);
    let query_phrase = query_terms.join(" ");
    let normalized_name = name_terms.join(" ");
    let phrase_match = !query_phrase.is_empty() && normalized_name.contains(&query_phrase);

    let total = fuzzy * 10
        + exact_name_hits * 12_000
        + partial_name_hits * 6_000
        + coverage_hits * 10_000
        + description_hits * 2_500
        + if all_terms_in_name { 35_000 } else { 0 }
        + if ordered_in_name { 10_000 } else { 0 }
        + if contiguous_in_name { 10_000 } else { 0 }
        + if phrase_match { 15_000 } else { 0 };

    Some(MatchScore { total, fuzzy })
}

fn tokenize_for_match(raw: &str) -> Vec<String> {
    raw.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_lowercase())
        .collect()
}

fn token_match_quality(token: &str, term: &str) -> i64 {
    if token == term {
        return 2;
    }
    if token.starts_with(term) || token.contains(term) {
        return 1;
    }
    0
}

fn terms_in_order(name_terms: &[String], query_terms: &[String]) -> bool {
    if query_terms.is_empty() {
        return false;
    }

    let mut cursor = 0usize;
    for query in query_terms {
        let mut found = false;
        while cursor < name_terms.len() {
            if token_match_quality(&name_terms[cursor], query) > 0 {
                found = true;
                cursor += 1;
                break;
            }
            cursor += 1;
        }

        if !found {
            return false;
        }
    }

    true
}

fn terms_contiguous(name_terms: &[String], query_terms: &[String]) -> bool {
    if query_terms.is_empty() || query_terms.len() > name_terms.len() {
        return false;
    }

    for start in 0..=(name_terms.len() - query_terms.len()) {
        let mut all_match = true;
        for offset in 0..query_terms.len() {
            if token_match_quality(&name_terms[start + offset], &query_terms[offset]) == 0 {
                all_match = false;
                break;
            }
        }
        if all_match {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::model::CommandSource;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn mock_command(name: &str) -> CommandEntry {
        CommandEntry {
            name: name.to_string(),
            description: Some("artisan command".to_string()),
            template: format!("php artisan {name}"),
            params: Vec::new(),
            source: CommandSource::Provider("artisan"),
            working_dir: None,
        }
    }

    fn default_ranking() -> RankingSettings {
        RankingSettings {
            usage_enabled: true,
            usage_weight: 8_000,
        }
    }

    fn test_runtime() -> RuntimeContext {
        RuntimeContext {
            cwd: std::env::temp_dir(),
            explicit_config_path: None,
        }
    }

    fn top_name_for(query: &str, commands: Vec<CommandEntry>) -> String {
        let mut app = AppState::new(
            commands,
            None,
            HashMap::new(),
            default_ranking(),
            test_runtime(),
        );
        app.query = query.to_string();
        app.refresh_filtered();

        let SearchItem::Command(index) = app.filtered[0] else {
            panic!("expected command result");
        };
        app.commands[index].name.clone()
    }

    #[test]
    fn phrase_query_prefers_cache_clear() {
        let top = top_name_for(
            "cache clear",
            vec![
                mock_command("artisan cache:clear"),
                mock_command("artisan clear-compiled"),
                mock_command("artisan cache:table"),
            ],
        );
        assert_eq!(top, "artisan cache:clear");
    }

    #[test]
    fn reversed_query_still_prefers_cache_clear() {
        let top = top_name_for(
            "clear cache",
            vec![
                mock_command("artisan cache:clear"),
                mock_command("artisan clear-compiled"),
                mock_command("artisan cache:table"),
            ],
        );
        assert_eq!(top, "artisan cache:clear");
    }

    #[test]
    fn prompt_submit_switches_back_to_search_mode() {
        let mut command = mock_command("deploy");
        command.template = "deploy --env={{env}}".to_string();
        command.params = vec![crate::model::ParamSpec {
            name: "env".to_string(),
            kind: ParamType::Value,
            prompt: "Environment".to_string(),
            placeholder: None,
            default_value: None,
            value_value: None,
            default_flag: None,
            value_flag: None,
            required: true,
            prompt_in_tui: true,
        }];

        let mut app = AppState::new(
            vec![command],
            None,
            HashMap::new(),
            default_ranking(),
            test_runtime(),
        );
        let action = app.prepare_selected_command(true);
        assert!(matches!(action, UiAction::None));
        assert!(matches!(app.mode, Mode::Prompt(_)));

        if let Mode::Prompt(prompt) = &mut app.mode {
            prompt.input = "production".to_string();
        }

        let action = app.on_prompt_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, UiAction::Run(_)));
        assert!(matches!(app.mode, Mode::Search));
    }

    #[test]
    fn alias_filter_limits_results_to_provider() {
        let commands = vec![
            CommandEntry {
                name: "artisan cache:clear".to_string(),
                description: None,
                template: "php artisan cache:clear".to_string(),
                params: Vec::new(),
                source: CommandSource::Provider("artisan"),
                working_dir: None,
            },
            CommandEntry {
                name: "just build".to_string(),
                description: None,
                template: "just build".to_string(),
                params: Vec::new(),
                source: CommandSource::Provider("justfile"),
                working_dir: None,
            },
        ];

        let mut aliases = HashMap::new();
        aliases.insert("a".to_string(), "artisan".to_string());
        aliases.insert("j".to_string(), "justfile".to_string());

        let mut app = AppState::new(commands, None, aliases, default_ranking(), test_runtime());
        app.query = ":a cache".to_string();
        app.refresh_filtered();

        assert_eq!(app.filtered.len(), 1);
        let SearchItem::Command(index) = app.filtered[0] else {
            panic!("expected command result");
        };
        assert_eq!(app.commands[index].name, "artisan cache:clear");
    }

    #[test]
    fn provider_name_filter_works_without_alias() {
        let commands = vec![
            CommandEntry {
                name: "artisan cache:clear".to_string(),
                description: None,
                template: "php artisan cache:clear".to_string(),
                params: Vec::new(),
                source: CommandSource::Provider("artisan"),
                working_dir: None,
            },
            CommandEntry {
                name: "just build".to_string(),
                description: None,
                template: "just build".to_string(),
                params: Vec::new(),
                source: CommandSource::Provider("justfile"),
                working_dir: None,
            },
        ];

        let mut aliases = HashMap::new();
        aliases.insert("a".to_string(), "artisan".to_string());

        let mut app = AppState::new(commands, None, aliases, default_ranking(), test_runtime());
        app.query = ":justfile build".to_string();
        app.refresh_filtered();

        assert_eq!(app.filtered.len(), 1);
        let SearchItem::Command(index) = app.filtered[0] else {
            panic!("expected command result");
        };
        assert_eq!(app.commands[index].name, "just build");
    }

    #[test]
    fn provider_name_filter_is_disabled_when_alias_exists() {
        let commands = vec![CommandEntry {
            name: "artisan cache:clear".to_string(),
            description: None,
            template: "php artisan cache:clear".to_string(),
            params: Vec::new(),
            source: CommandSource::Provider("artisan"),
            working_dir: None,
        }];

        let mut aliases = HashMap::new();
        aliases.insert("a".to_string(), "artisan".to_string());

        let mut app = AppState::new(commands, None, aliases, default_ranking(), test_runtime());
        app.query = ":artisan cache".to_string();
        app.refresh_filtered();

        assert!(app.filtered.is_empty());
    }

    #[test]
    fn search_cursor_allows_mid_string_editing() {
        let mut app = AppState::new(
            vec![mock_command("artisan cache:clear")],
            None,
            HashMap::new(),
            default_ranking(),
            test_runtime(),
        );
        app.on_search_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        app.on_search_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        app.on_search_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        app.on_search_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.on_search_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert_eq!(app.query, "abc");
    }

    #[test]
    fn usage_ranking_prefers_more_frequent_command() {
        let cmd_a = mock_command("artisan cache:clear");
        let cmd_b = mock_command("artisan cache:table");
        let key_b = command_usage_key(&cmd_b);

        let mut app = AppState::new(
            vec![cmd_a, cmd_b],
            None,
            HashMap::new(),
            default_ranking(),
            test_runtime(),
        );
        app.usage_counts.insert(key_b, 5);
        app.query = "cache".to_string();
        app.refresh_filtered();

        let SearchItem::Command(index) = app.filtered[0] else {
            panic!("expected command result");
        };
        assert_eq!(app.commands[index].name, "artisan cache:table");
    }

    #[test]
    fn help_mode_auto_closes_and_allows_typing() {
        let mut app = AppState::new(
            vec![mock_command("artisan cache:clear")],
            None,
            HashMap::new(),
            default_ranking(),
            test_runtime(),
        );
        app.show_help = true;
        app.query = "cache".to_string();
        app.query_cursor = app.query.chars().count();

        let action = app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(action, UiAction::None));
        assert_eq!(app.query, "cachex");
        assert!(!app.show_help);
    }

    #[test]
    fn tab_switches_focus_to_session_and_typing_returns_to_commands() {
        let mut app = AppState::new(
            vec![mock_command("artisan cache:clear")],
            None,
            HashMap::new(),
            default_ranking(),
            test_runtime(),
        );

        assert!(matches!(app.active_pane, ActivePane::Commands));
        app.on_search_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(app.active_pane, ActivePane::Session));

        app.on_search_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(app.active_pane, ActivePane::Commands));
        assert_eq!(app.query, "x");
    }

    #[test]
    fn session_focus_allows_scroll_keys() {
        let mut app = AppState::new(
            vec![mock_command("artisan cache:clear")],
            None,
            HashMap::new(),
            default_ranking(),
            test_runtime(),
        );
        for i in 0..20 {
            app.push_info(format!("line {i}"));
        }

        app.on_search_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(app.active_pane, ActivePane::Session));
        assert_eq!(app.session_scroll, 0);

        app.on_search_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(app.session_scroll > 0);
    }

    #[test]
    fn enter_does_nothing_when_session_is_active() {
        let mut app = AppState::new(
            vec![mock_command("artisan cache:clear")],
            None,
            HashMap::new(),
            default_ranking(),
            test_runtime(),
        );

        app.on_search_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(app.active_pane, ActivePane::Session));

        let action = app.on_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, UiAction::None));
        assert!(matches!(app.mode, Mode::Search));
    }

    #[test]
    fn parses_internal_reload_command() {
        let parsed = parse_internal_command("/reload").unwrap();
        assert!(matches!(parsed, InternalCommand::Reload));
    }

    #[test]
    fn parses_internal_init_force_command() {
        let parsed = parse_internal_command("/init --force").unwrap();
        assert!(matches!(parsed, InternalCommand::Init { force: true }));
    }

    #[test]
    fn internal_init_without_force_opens_prompt() {
        let mut app = AppState::new(
            vec![mock_command("artisan cache:clear")],
            None,
            HashMap::new(),
            default_ranking(),
            test_runtime(),
        );
        app.query = "/init".to_string();
        app.query_cursor = app.query.chars().count();
        app.refresh_filtered();

        let action = app.on_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, UiAction::None));
        assert!(matches!(app.mode, Mode::InternalPrompt(_)));
    }

    #[test]
    fn flag_param_prompt_uses_default_on_enter() {
        let mut command = mock_command("deploy");
        command.template = "deploy {{force}}".to_string();
        command.params = vec![crate::model::ParamSpec {
            name: "force".to_string(),
            kind: ParamType::Flag,
            prompt: "Use --force?".to_string(),
            placeholder: None,
            default_value: None,
            value_value: None,
            default_flag: Some(false),
            value_flag: None,
            required: false,
            prompt_in_tui: true,
        }];

        let mut app = AppState::new(
            vec![command],
            None,
            HashMap::new(),
            default_ranking(),
            test_runtime(),
        );
        let action = app.prepare_selected_command(true);
        assert!(matches!(action, UiAction::None));
        assert!(matches!(app.mode, Mode::Prompt(_)));

        let action = app.on_prompt_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let UiAction::Run(request) = action else {
            panic!("expected command run request");
        };
        assert_eq!(request.command_line.trim(), "deploy");
    }

    #[test]
    fn flag_param_prompt_accepts_y_without_enter() {
        let mut command = mock_command("deploy");
        command.template = "deploy {{force}}".to_string();
        command.params = vec![crate::model::ParamSpec {
            name: "force".to_string(),
            kind: ParamType::Flag,
            prompt: "Use --force?".to_string(),
            placeholder: None,
            default_value: None,
            value_value: None,
            default_flag: Some(false),
            value_flag: None,
            required: false,
            prompt_in_tui: true,
        }];

        let mut app = AppState::new(
            vec![command],
            None,
            HashMap::new(),
            default_ranking(),
            test_runtime(),
        );
        let action = app.prepare_selected_command(true);
        assert!(matches!(action, UiAction::None));
        assert!(matches!(app.mode, Mode::Prompt(_)));

        let action = app.on_prompt_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        let UiAction::Run(request) = action else {
            panic!("expected command run request");
        };
        assert_eq!(request.command_line.trim(), "deploy --force");
    }

    #[test]
    fn slash_query_only_shows_internal_commands() {
        let mut app = AppState::new(
            vec![mock_command("artisan cache:clear")],
            None,
            HashMap::new(),
            default_ranking(),
            test_runtime(),
        );
        app.query = "/".to_string();
        app.refresh_filtered();

        assert!(!app.filtered.is_empty());
        assert!(
            app.filtered
                .iter()
                .all(|item| matches!(item, SearchItem::Internal(_)))
        );
    }
}
