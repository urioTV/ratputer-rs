"""Send one command to the RATPUTER USB debug console.

This intentionally uses only Python's standard library so the Nix dev shell
needs no imperative packages. Invoke it through the `ratctl` devshell command.
"""

from __future__ import annotations

import argparse
import glob
import os
import random
import selectors
import sys
import termios
import time


def find_port(requested: str | None) -> str:
    if requested:
        return requested
    ports = sorted(glob.glob("/dev/ttyACM*"))
    if len(ports) == 1:
        return ports[0]
    if not ports:
        raise RuntimeError("no /dev/ttyACM* device found; connect the Cardputer")
    raise RuntimeError(
        "multiple CDC devices found; select one with --port: " + ", ".join(ports)
    )


def configure_raw(fd: int) -> None:
    attrs = termios.tcgetattr(fd)
    attrs[0] = 0
    attrs[1] = 0
    attrs[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
    attrs[3] = 0
    attrs[4] = termios.B115200
    attrs[5] = termios.B115200
    attrs[6][termios.VMIN] = 0
    attrs[6][termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW, attrs)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="ratctl",
        description="Control and inspect RATPUTER over USB Serial/JTAG",
    )
    parser.add_argument("--port", help="CDC device (default: the only /dev/ttyACM*)")
    parser.add_argument("--timeout", type=float, default=5.0, help="response timeout in seconds")
    parser.add_argument("--logs", action="store_true", help="also print normal firmware logs")
    parser.add_argument(
        "command",
        nargs=argparse.REMAINDER,
        help="PING, STATUS, HELP, KEY <key>, TEXT <text>, CLEAR, or REBOOT",
    )
    args = parser.parse_args()
    if not args.command:
        args.command = ["STATUS"]
    return args


def main() -> int:
    args = parse_args()
    try:
        port = find_port(args.port)
        fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    except (OSError, RuntimeError) as error:
        print(f"ratctl: {error}", file=sys.stderr)
        return 2

    try:
        configure_raw(fd)
        # Drop stale boot logs so output belongs to this command. The request ID
        # still protects against delayed responses from earlier invocations.
        termios.tcflush(fd, termios.TCIFLUSH)
        request_id = random.randrange(1, 2**31)
        command = " ".join(args.command)
        os.write(fd, f"RAT {request_id} {command}\n".encode("ascii"))

        selector = selectors.DefaultSelector()
        selector.register(fd, selectors.EVENT_READ)
        deadline = time.monotonic() + args.timeout
        buffered = bytearray()
        saw_response = False
        while time.monotonic() < deadline:
            events = selector.select(max(0.0, deadline - time.monotonic()))
            if not events:
                continue
            try:
                chunk = os.read(fd, 4096)
            except BlockingIOError:
                continue
            if not chunk:
                continue
            buffered.extend(chunk)
            while b"\n" in buffered:
                raw_line, _, remainder = buffered.partition(b"\n")
                buffered = bytearray(remainder)
                line = raw_line.rstrip(b"\r").decode("utf-8", errors="replace")
                prefix = f"@RAT {request_id} "
                if line.startswith(prefix):
                    saw_response = True
                    payload = line[len(prefix) :]
                    print(payload)
                    if payload == "END":
                        return 0
                elif args.logs and line:
                    print(f"LOG {line}")

        if not saw_response:
            print(
                f"ratctl: no protocol response from {port}; firmware may be old, rebooting, or using USB MSC",
                file=sys.stderr,
            )
        else:
            print("ratctl: response did not terminate", file=sys.stderr)
        return 1
    except (OSError, UnicodeEncodeError, termios.error) as error:
        print(f"ratctl: {error}", file=sys.stderr)
        return 2
    finally:
        os.close(fd)


if __name__ == "__main__":
    raise SystemExit(main())
