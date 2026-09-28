#!/usr/bin/env bash
# Flash a verified merged image from WSL (via Windows COM) or native Linux.
set -euo pipefail

image="${1:-ratputer-adv.bin}"
if [[ ! -f "$image" ]]; then
  echo "Image not found: $image (run build first)" >&2
  exit 1
fi

if [[ -n "${WSL_DISTRO_NAME:-}" && "${RATPUTER_FLASH_TRANSPORT:-windows}" != linux ]]; then
  for tool in powershell.exe wslpath espflash.exe; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      echo "Missing $tool in WSL. Install the Windows ESP tool or set RATPUTER_FLASH_TRANSPORT=linux for USB passthrough." >&2
      exit 1
    fi
  done

  port="${ESPFLASH_PORT:-}"
  if [[ -z "$port" ]]; then
    # PowerShell expands $_ on Windows, not Bash.
    # shellcheck disable=SC2016
    detected=$(powershell.exe -NoProfile -NonInteractive -Command \
      'Get-CimInstance Win32_SerialPort | Where-Object { $_.PNPDeviceID -match "VID_303A&PID_1001" } | ForEach-Object DeviceID' | tr -d '\r')
    mapfile -t ports < <(printf '%s\n' "$detected" | grep -E '^COM[0-9]+$' || true)
    if (( ${#ports[@]} != 1 )); then
      echo "Expected exactly one ESP32-S3 USB Serial/JTAG COM port; found ${#ports[@]}. Set ESPFLASH_PORT=COMn explicitly." >&2
      exit 1
    fi
    port="${ports[0]}"
  fi
  if [[ ! "$port" =~ ^COM[0-9]+$ ]]; then
    echo "Invalid Windows ESPFLASH_PORT: $port (expected COMn)" >&2
    exit 1
  fi

  echo "Flashing $image through Windows $port (watchdog reset after write)"
  espflash.exe write-bin --non-interactive --port "$port" --chip esp32s3 \
    --after watchdog-reset 0x0 "$(wslpath -w "$(realpath "$image")")"

  verify_script=$(wslpath -w "$(dirname "$(realpath "${BASH_SOURCE[0]}")")/verify-boot.ps1")
  if ! powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass \
    -File "$verify_script" -Port "$port"; then
    echo "No firmware PING after flashing; retrying with an explicit watchdog reset..." >&2
    espflash.exe reset --non-interactive --port "$port" --chip esp32s3 --after watchdog-reset
    if ! powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass \
      -File "$verify_script" -Port "$port"; then
      echo "Firmware did not answer PING. Check its screen or press the physical reset button." >&2
      exit 1
    fi
  fi
else
  echo "Flashing $image through Linux serial (watchdog reset after write)"
  espflash write-bin --after watchdog-reset 0x0 "$image"
  echo "Flash verified; check the display to confirm the firmware booted."
fi
