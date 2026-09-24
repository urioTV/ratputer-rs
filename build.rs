// Build-time: compile the Slint UI (ui/ratputer.slint) into Rust code.
// Fonts from ui/fonts are packed into the image (no PSRAM on the ADV — flash/RAM only).
fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config(manifest_dir.join("ui/ratputer.slint"), config).unwrap();

    // TinyUSB is deliberately a small C island: device core + MSC/SCSI + the
    // ESP32-S3 Synopsys DWC2 controller. It is polled by Rust, so no ESP-IDF or
    // FreeRTOS objects are linked.
    let mut c = cc::Build::new();
    c.compiler("xtensa-esp32s3-elf-gcc")
        .include(manifest_dir.join("src/usb"))
        .include(manifest_dir.join("vendor/tinyusb/src"))
        .define("CFG_TUSB_CONFIG_FILE", "\"tusb_config.h\"")
        .flag("-std=c11")
        .flag("-Os")
        .flag("-mlongcalls")
        .flag("-ffunction-sections")
        .flag("-fdata-sections")
        .flag("-fno-common")
        .file(manifest_dir.join("src/usb/tinyusb_bridge.c"))
        .file(manifest_dir.join("vendor/tinyusb/src/tusb.c"))
        .file(manifest_dir.join("vendor/tinyusb/src/common/tusb_fifo.c"))
        .file(manifest_dir.join("vendor/tinyusb/src/device/usbd.c"))
        .file(manifest_dir.join("vendor/tinyusb/src/class/msc/msc_device.c"))
        .file(manifest_dir.join("vendor/tinyusb/src/portable/synopsys/dwc2/dwc2_common.c"))
        .file(manifest_dir.join("vendor/tinyusb/src/portable/synopsys/dwc2/dcd_dwc2.c"));
    c.compile("ratputer_tinyusb");

    println!("cargo:rerun-if-changed=src/usb");
    println!("cargo:rerun-if-changed=vendor/tinyusb");
}
