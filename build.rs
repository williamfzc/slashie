fn main() {
    // Tauri codegen only applies to the macOS tray build; the headless
    // artifact must not depend on the GUI build graph (DESIGN.md).
    #[cfg(feature = "tray")]
    tauri_build::build();
}
