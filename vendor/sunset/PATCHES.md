# Local patches to sunset 0.6.0

This is a vendored copy of `sunset` 0.6.0 (0BSD, <https://github.com/mkj/sunset>),
wired in through a path dependency in the firmware `Cargo.toml`. Upstream
provenance is recorded in `UPSTREAM.toml`; reproducible patch files live in
`../../vendor-patches/sunset/`. Update only with `./tools/update-sunset.sh
<version>`, which downloads the crates.io release, verifies its checksum,
drops repository-only files (CI scripts, design notes, `rust-toolchain.toml`,
`[profile.*]` tables) and reapplies every patch in name order. The library
sources differ only by the changes below, each marked `RATPUTER PATCH`.

1. `Runner::close_channel(chan, exit_status)` (patch 0001). Upstream only
   answers a peer's EOF/CLOSE, so a server cannot end an `exec` command or a
   shell `exit` itself and OpenSSH waits forever. The new call sends the
   optional `exit-status` channel request (session channels only), then EOF
   and CLOSE, using the existing `Req` and packet code (`Req::ExitStatus` is
   the new request variant). The peer's CLOSE reply goes through the normal
   `handle_close` path; the application still calls `channel_done()`.

2. Larger channel receive window and packet size (patch 0002,
   `config.rs`). Upstream's `DEFAULT_WINDOW`/`DEFAULT_MAX_PACKET` of 1000
   bytes make the client stop after every kilobyte and wait for a window
   adjust, capping SFTP uploads at a few dozen KiB/s. Now 32 KiB / 3072
   bytes; the packet size must fit the firmware's 4 KiB input buffer
   including SSH packet overhead, and the window only shifts backpressure to
   TCP. Both constants affect the receive direction only.
