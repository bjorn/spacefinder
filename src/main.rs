slint::include_modules!();

mod config;
mod controller;
mod dir_size;
mod fs_scan;
mod i18n;
mod icons;
mod sidebar;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("space=info,warn"),
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

    // Pick the initial directory. Prefer the last-seen location if it still
    // exists, otherwise fall back to the user's home directory, and finally
    // the filesystem root so the app can at least launch.
    let start = cfg
        .last_location
        .clone()
        .filter(|p| p.is_dir())
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("/"));

    let _app = controller::App::new(&window, start, cfg);
    window.run()?;
    Ok(())
}
