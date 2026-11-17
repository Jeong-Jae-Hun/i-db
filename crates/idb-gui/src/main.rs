#[cfg(target_os = "macos")]
fn main() {
    idb_gui::ui::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("i-db GUI 는 macOS 에서만 돈다");
    std::process::exit(1);
}
