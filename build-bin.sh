#!/usr/bin/env bash
# Builds the firmware and produces a merged .bin (bootloader @0x0 + partition table @0x8000 + app @0x10000).
# Image header per the M5Stack StampS3A module spec (ESP32-S3FN8):
#   flash: 8 MB, QIO, 80 MHz  (source: platformio board m5stack-stamps3.json)
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release

espflash save-image \
  --chip esp32s3 \
  --merge \
  --skip-padding \
  --flash-size 8mb \
  --flash-mode qio \
  --flash-freq 80mhz \
  target/xtensa-esp32s3-none-elf/release/ratputer-rs \
  ratputer-adv.bin

ls -lh ratputer-adv.bin
echo ""
echo "Flash (one command; the image is merged from 0x0):"
echo "  espflash write-bin 0x0 ratputer-adv.bin --verify"
echo "  # or esptool:"
echo "  esptool --chip esp32s3 write_flash 0x0 ratputer-adv.bin"
