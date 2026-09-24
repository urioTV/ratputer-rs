// Build-time: compile the Slint UI (ui/ratputer.slint) into Rust code.
// Fonts from ui/fonts are packed into the image (no PSRAM on the ADV — flash/RAM only).
fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config(manifest_dir.join("ui/ratputer.slint"), config).unwrap();
}
