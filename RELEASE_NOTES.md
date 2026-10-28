**zhandu v0.6.0** makes owner-filtered browsing more consistent and adds display controls, safer refresh recovery, and a short terminal demo.

- Start with two panels. `v` switches between one and two panels; `d` toggles files without changing layout. Left/Backspace goes to the parent directory; Tab switches panel focus.
- Press `p` to show/hide percentages in both panels. Percentages follow the current size mode and filtered totals.
- Highlight sizes ≥1 GiB in yellow and ≥100 GiB in bold red. Shift+C toggles size colors.
- Press `q` in help, error, or details panels to close the panel; `q` in the browser exits.
- Press `r` for background refresh while retaining navigation and filters; revalidate file marks before enabling deletion again.
- Press `z` to hide known zero usage and `i` for entry details. Keep marks, cache age, and branch completeness visible.
- Preserve partial caches alongside complete snapshots and recover safely when selected paths or scan roots change.
- Add a 13-second terminal demo to the README.

Install/update with `cargo install zhandu --locked` (Rust 1.89 or newer).

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
