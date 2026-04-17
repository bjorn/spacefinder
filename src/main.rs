slint::include_modules!();

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

    let start = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
    let _app = controller::App::new(&window, start);
    window.run()?;
    Ok(())
}
