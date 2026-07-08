fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("{} {}", bitbygit_core::APP_NAME, bitbygit_core::VERSION);
        return Ok(());
    }

    bitbygit_tui::run()?;
    Ok(())
}
