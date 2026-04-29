fn main() {
    let music_dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("Music");
    if let Err(err) = srtui::radio::run(music_dir) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
