slint::include_modules!();

mod controller;
mod dir_size;
mod fs_scan;
mod icons;
mod sidebar;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("space=info,warn"),
    )
    .init();
    let window = MainWindow::new()?;
    let start = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
    let _app = controller::App::new(&window, start);
    window.run()?;
    Ok(())
}
