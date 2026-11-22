mod api;
mod app;
mod article;
mod cache;
mod config;
mod event;
mod graphics;
mod input;
mod models;
mod speed;
mod sync;
mod ui;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use ratatui_image::picker::Picker;
use tokio::sync::mpsc;

use crate::app::App;
use crate::cache::Cache;
use crate::config::Config;

/// A terminal client for reading and managing Readeck bookmarks.
#[derive(Debug, Parser)]
#[command(
    name = "eckly",
    version,
    about = "Read and manage your Readeck bookmarks from the terminal",
    after_help = "Configuration is read from the config file, then overridden by --server/--token \
                  and the READECK_URL, READECK_TOKEN and READECK_API_TOKEN environment variables.\n\
                  Themes: opaline builtins; use --theme, ECKLY_THEME or --list-themes.\n\
                  Use `eckly sync --help` to cache articles for offline reading.\n\
                  Use --offline to browse the local cache without contacting Readeck.\n\
                  Run without credentials to use the interactive setup screen."
)]
struct Cli {
    /// Readeck server URL (for example https://readeck.example.com)
    #[arg(long, value_name = "URL", global = true)]
    server: Option<String>,

    /// Readeck API token
    #[arg(long, value_name = "TOKEN", global = true)]
    token: Option<String>,

    /// Path to the configuration file
    #[arg(long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,

    /// Color theme (opaline builtin ID or display name)
    #[arg(long, value_name = "THEME", global = true)]
    theme: Option<String>,

    /// Start in offline mode using only the local cache
    #[arg(long)]
    offline: bool,

    /// List built-in themes and exit
    #[arg(long)]
    list_themes: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Sync the local cache for offline reading (changed bookmarks by default)
    Sync(sync::SyncArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.list_themes {
        let mut stdout = std::io::stdout().lock();
        for (id, name) in ui::theme::all() {
            if writeln!(stdout, "{id:<24} {name}").is_err() {
                break;
            }
        }
        return Ok(());
    }
    let config_path = cli.config.unwrap_or_else(config::default_path);

    let mut config = Config::load(&config_path)?.unwrap_or_default();

    if let Some(server) = cli.server.or_else(|| env_value("READECK_URL")) {
        config.server = server;
    }
    if let Some(token) = cli
        .token
        .or_else(|| env_value("READECK_TOKEN"))
        .or_else(|| env_value("READECK_API_TOKEN"))
    {
        config.token = token;
    }

    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("eckly");

    if let Some(Commands::Sync(args)) = cli.command {
        return sync::run(&config, &args, cache_dir).await;
    }

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "eckly must be run in an interactive terminal (try `eckly --help` for options)"
        );
    }

    let theme_name = cli
        .theme
        .or_else(|| env_value("ECKLY_THEME"))
        .unwrap_or_else(|| config.theme.clone());
    let theme_name = if theme_name.trim().is_empty() {
        "default".to_string()
    } else {
        theme_name
    };
    match ui::theme::set_name(&theme_name) {
        Ok(info) => config.theme = info.id,
        Err(err) => {
            eprintln!(
                "Warning: {err}. Using the default theme. Run `eckly --list-themes` to see available themes."
            );
            if let Ok(info) = ui::theme::set_name("default") {
                config.theme = info.id;
            }
        }
    }

    // Detection reads the terminal's replies from stdin, so it has to finish
    // before the input thread starts.
    let picker = config.terminal_graphics.then(|| {
        Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
    });

    let (tx, rx) = mpsc::unbounded_channel();
    event::spawn_input_thread(tx.clone());

    let cache = match Cache::load(cache_dir.clone()) {
        Ok(cache) => Some(cache),
        Err(err) => {
            eprintln!("Warning: {err:#}");
            Some(Cache::new(cache_dir))
        }
    };

    let mut app = App::with_cache(config, config_path, tx, cache, cli.offline);
    app.picker = picker;
    app.bootstrap();

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, rx).await;
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    mut rx: mpsc::UnboundedReceiver<event::Event>,
) -> Result<()> {
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        app.spawn_image_encodes();

        if app.quit {
            break;
        }

        // While speed reading, wake up exactly when the next word is due
        // instead of waiting for the coarse input poll tick.
        if let Some(deadline) = app.speed_frame_deadline() {
            tokio::select! {
                event = rx.recv() => match event {
                    Some(event) => app.handle_event(event),
                    None => break,
                },
                _ = tokio::time::sleep_until(deadline.into()) => {
                    app.advance_speed_reading();
                }
            }
        } else {
            match rx.recv().await {
                Some(event) => app.handle_event(event),
                None => break,
            }
        }
    }
    Ok(())
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
