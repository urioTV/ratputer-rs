# Local patches to hadris-fat 2.4.0

This is a vendored copy of `hadris-fat` 2.4.0 (MIT,
<https://github.com/hxyulin/hadris>), wired in through a path dependency in the
firmware `Cargo.toml`. Tests and examples were dropped; the library sources are
unchanged apart from the change below, marked `RATPUTER PATCH`.

1. `FatDirIter::next_entry` buffers a sliding 4 KiB window of the directory
   instead of the entire cluster. Upstream allocates `cluster_size` bytes when
   the `alloc` feature is on; on a large FAT32 card with 32 KiB clusters that
   is a 32 KiB allocation per directory — a guaranteed OOM panic on this
   device's 150 KiB heap while Wi-Fi, Slint and FTP buffers are live.
   Both the sync and async copies of the iterator carry the patch; the seek
   position now adds `buffer_base`, and the refill condition triggers whenever
   the read offset leaves the current window (previously it only did so for
   fixed FAT12/16 roots).
