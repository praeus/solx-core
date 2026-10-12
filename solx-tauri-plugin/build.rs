include!("src/command_names.rs");

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}
