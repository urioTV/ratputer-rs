# Local patches to hadris-fat 2.4.0

This is a vendored copy of `hadris-fat` 2.4.0 (MIT,
<https://github.com/hxyulin/hadris>), wired in through a path dependency in the
firmware `Cargo.toml`. Tests and examples were dropped. Upstream provenance is
recorded in `UPSTREAM.toml`; reproducible patch files live in
`../../vendor-patches/hadris-fat/`. The library sources differ only by the
changes below, each marked `RATPUTER PATCH`.

1. `FatDirIter::next_entry` buffers a sliding 4 KiB window of the directory
   instead of the entire cluster. Upstream allocates `cluster_size` bytes when
   the `alloc` feature is on; on a large FAT32 card with 32 KiB clusters that
   is a 32 KiB allocation per directory — a guaranteed OOM panic on this
   device's 150 KiB heap while Wi-Fi, Slint and FTP buffers are live.
   Both the sync and async copies of the iterator carry the patch; the seek
   position now adds `buffer_base`, and the refill condition triggers whenever
   the read offset leaves the current window (previously it only did so for
   fixed FAT12/16 roots).

2. `FileWriter` exposes an opaque, validated `AppendCursor` plus
   `new_append_from_cursor`. FTP receives a file in separate main-loop polls and
   cannot retain a `FileWriter` borrowing the volume between polls. Upstream's
   `new_append` walks the whole FAT chain every time; reopening it for every
   4 KiB socket chunk therefore makes uploads quadratic. The cursor caches the
   committed tail cluster and offset, while revalidating the directory slot,
   identity, first cluster, size, tail, and offset before each reuse. Both sync
   and async APIs carry the patch through the shared transformed source.

3. `FileReader` exposes an analogous `ReadCursor` plus `new_from_cursor` for
   the same reason on the download path: each 4 KiB poll step reopens the file,
   and upstream would restart every open at the file's first cluster, walking
   the FAT once per cluster (quadratic in file size). The cursor caches the
   current cluster and offset, revalidating identity, first cluster, committed
   size, position, and cluster-offset bounds on each reuse.

## Updating

From the repository root, run the updater with the desired crates.io version:

```sh
./tools/update-hadris-fat.sh 2.5.0
```

The script downloads the official crate, verifies its crates.io SHA-256,
removes upstream tests/examples, applies the three patches in order, records
its release commit in `UPSTREAM.toml`, then runs the release check and complete
firmware build. It replaces the existing vendor only after download and patch
validation and restores the previous tree if compilation fails.

A patch conflict is intentional protection: inspect upstream before rebasing a
patch, because the new release may have changed or superseded the local fix.
After a successful update, review the vendor diff and `Cargo.lock`, run the host
FAT image test plus `fsck.fat`, and finally repeat the FTP/USB hardware suite.
