fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent-dark".to_string())
        .with_bundled_translations("lang");
    match slint_build::compile_with_config("ui/main.slint", config) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("slint compilation failed: {e}");
            std::process::exit(1);
        }
    }
}
