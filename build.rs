// Build-time: kompilacja UI Slint (ui/ratputer.slint) do w kodu Rusta.
// Fonty z ui/fonts są pakowane do obrazu (brak PSRAM na ADV — wszystko w flashu/RAM).
fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config(manifest_dir.join("ui/ratputer.slint"), config).unwrap();
}
