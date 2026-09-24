# Local patches to embedded-sdmmc 0.10.0

This is a vendored copy of `embedded-sdmmc` 0.10.0 (MIT OR Apache-2.0,
<https://github.com/rust-embedded-community/embedded-sdmmc-rs>), wired in through
`[patch.crates-io]` in the firmware `Cargo.toml`. Tests, examples and dev
dependencies were dropped; the library sources are unchanged apart from the
changes below, all marked `RATPUTER PATCH`.

1. `VolumeManager::delete_entry_in_dir` frees the deleted entry's cluster chain
   (`FatVolume::free_cluster_chain`). Upstream only marks the directory entry as
   deleted, which leaks all of its clusters.
2. `FatVolume::delete_entry_in_block` also marks the long-file-name entries that
   precede the short entry in the same block as deleted.
3. `FatVolume::truncate_cluster_chain` increments the FSInfo free-cluster count
   for the final cluster too; upstream skipped it when breaking on end-of-chain.
