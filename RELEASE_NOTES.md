**zhandu v0.4.0** adds a persistent SQLite scan cache.

- Open previous scan results on startup, clearly labeled cached, then refresh in the background.
- Update cached rows and add new entries while scanning; prune missing entries when refresh completes.
- Save completed, error-free snapshots atomically, retaining the previous snapshot on interruption or failure.
- Match caches by root identity and traversal settings; preserve Windows ownership using SIDs.
- Enable caching by default for the TUI; add `--no-cache` and `--cache-db PATH`. JSON always scans fresh.
- Retain the 16 most recent root/configuration snapshots; keep Unix cache files private.
- Keep marking/deletion disabled during refresh. Cache failures fall back to live scanning.
- SQLite is bundled. Building from source now requires Rust 1.89 or newer.
- Install/update with `cargo install zhandu --locked`.

## Downloads

Choose the archive matching your OS and architecture, extract it, and run `zhandu` (`zhandu.exe` on Windows). Documentation and dependency licenses are included. Verify downloads with `SHA256SUMS`.

| Platform | Archive target |
| --- | --- |
| Windows x64 | x86_64-pc-windows-msvc (.zip) |
| Linux x64 (glibc 2.35+) | x86_64-unknown-linux-gnu (.tar.gz) |
| Linux ARM64 (glibc 2.39+) | aarch64-unknown-linux-gnu (.tar.gz) |
| macOS Intel | x86_64-apple-darwin (.tar.gz) |
| macOS Apple Silicon | aarch64-apple-darwin (.tar.gz) |

Windows uses file owner and primary-group SIDs, not ACL access groups. JSON includes persistent SID strings; numeric Windows owner IDs are process-local intern keys. Use account names or SIDs for command-line filters. Unreadable file metadata/security descriptors produce incomplete results. Reparse points, including junctions, are not traversed. NTFS is the primary Windows filesystem target; ReFS/FAT and network shares have not been validated.

The archives are not notarized or Authenticode-signed. All five native build/test jobs must succeed before the workflow publishes this release.
