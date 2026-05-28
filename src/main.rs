slint::include_modules!();

mod columns;
mod config;
mod controller;
mod dir_size;
mod disk;
mod fs_scan;
mod i18n;
mod icons;
mod sidebar;
mod treemap;

use clap::Parser;
use std::path::PathBuf;

/// CLI surface. `--help` and `--version` are auto-wired by clap.
#[derive(Parser)]
#[command(name = "spacefinder", version, about, long_about = None)]
struct Cli {
    /// Directory to open. Defaults to the last-opened directory, or the home
    /// directory on first run.
    #[arg(value_parser = parse_dir)]
    path: Option<PathBuf>,
}

fn parse_dir(s: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(s);
    if !p.is_dir() {
        return Err(format!("'{s}' is not a directory"));
    }
    Ok(p)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("spacefinder=info,warn"),
    )
    .init();

    // Load Rust-side translations from the bundled .po files.
    let lang = i18n::init();
    log::info!("translations: active language = {}", lang);

    let window = MainWindow::new()?;

    // Keep the Slint side in sync. `select_bundled_translation` uses the
    // language folder names under `lang/`. Auto-detection runs when the
    // first component is created, but calling it explicitly makes the
    // behavior obvious and lets us log the result.
    if let Err(e) = slint::select_bundled_translation(lang) {
        log::warn!("select_bundled_translation({:?}): {}", lang, e);
    }

    // Load persisted settings before creating the controller so the first
    // paint already reflects the user's choices.
    let cfg = config::load();
    log::info!("config: loaded {:?}", cfg);

    // Apply the persisted window size in logical pixels. Slint will map this
    // to physical pixels using the window's scale factor.
    let [win_w, win_h] = cfg.window_size;
    window
        .window()
        .set_size(slint::LogicalSize::new(win_w as f32, win_h as f32));

    // Pick the initial directory. CLI path wins if provided; otherwise prefer
    // the last-seen location, fall back to $HOME, and finally to `/` so the
    // app can at least launch.
    let start = cli
        .path
        .or_else(|| cfg.last_location.clone().filter(|p| p.is_dir()))
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));

    let _app = controller::App::new(&window, start, cfg);
    window.run()?;
    Ok(())
}
