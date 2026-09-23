use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, ExitCode};

const TARGET: &str = "xtensa-esp32s3-none-elf";
const FIRMWARE: &str = "ratputer-rs";
const OUTPUT: &str = "ratputer-adv.bin";

fn main() -> ExitCode {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> Result<(), String> {
    let command = env::args().nth(1).unwrap_or_else(|| "build".to_owned());
    if command != "build" {
        return Err(format!("unknown command `{command}`; expected `build`"));
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("xtask has no repository root")?;

    run_command(
        Command::new("cargo")
            .current_dir(root)
            .args(["build", "--release"]),
        "firmware build",
    )?;

    let elf = root
        .join("target")
        .join(TARGET)
        .join("release")
        .join(FIRMWARE);
    let output = root.join(OUTPUT);

    run_command(
        Command::new("espflash")
            .current_dir(root)
            .args([
                "save-image",
                "--chip",
                "esp32s3",
                "--merge",
                "--skip-padding",
                "--flash-size",
                "8mb",
                "--flash-mode",
                "dio",
                "--flash-freq",
                "80mhz",
            ])
            .arg(&elf)
            .arg(&output),
        "merged image generation",
    )?;

    verify_headers(&output)?;
    let size = output
        .metadata()
        .map_err(|error| format!("cannot inspect {}: {error}", output.display()))?
        .len();

    println!("\nCreated {} ({size} bytes)", output.display());
    println!("Verified headers: boot/app=e9 DIO, partition=aa 50");
    println!("Flash with:");
    println!("  espflash write-bin 0x0 {OUTPUT}");
    Ok(())
}

fn run_command(command: &mut Command, description: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|error| format!("failed to start {description}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{description} failed with {status}"))
    }
}

fn verify_headers(path: &Path) -> Result<(), String> {
    let expected = [
        (0_u64, &[0xe9][..]),
        (0x0002, &[0x02]), // DIO; QIO (0x00) fails in the ESP32-S3 ROM loader
        (0x8000, &[0xaa, 0x50]),
        (0x10000, &[0xe9]),
        (0x10002, &[0x02]),
    ];
    let mut file =
        File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;

    for (offset, expected_bytes) in expected {
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| format!("cannot seek to 0x{offset:x}: {error}"))?;
        let mut actual = vec![0; expected_bytes.len()];
        file.read_exact(&mut actual)
            .map_err(|error| format!("cannot read header at 0x{offset:x}: {error}"))?;
        if actual != expected_bytes {
            return Err(format!(
                "invalid header at 0x{offset:x}: expected {expected_bytes:02x?}, got {actual:02x?}"
            ));
        }
    }
    Ok(())
}
