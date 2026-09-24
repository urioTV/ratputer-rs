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
