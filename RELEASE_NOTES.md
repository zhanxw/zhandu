**zhandu v0.5.0** improves owner filtering, panel search, and scan status clarity.

- Keep All users / All groups pinned at the top of each owner picker. Home then Enter clears that filter while preserving the other.
- Show named user and group filters beneath the current path; label owner previews and each panel's scope.
- Search the focused panel with `/`, finish typing with Enter, and clear search with Esc before clearing ownership filters.
- Keep directory columns aligned when panel focus changes.
- Distinguish complete, cached-refreshing, scanning, and finished-with-errors states. Provisional aggregate sizes use `~`; uncertain zeros show `…`.
- Keep Tab dedicated to panel focus and show context-specific keyboard hints.
- Preserve existing caching and deletion safeguards. Search changes visible rows, not accounting totals or file marks.
- Install/update with `cargo install zhandu --locked` (Rust 1.89 or newer).

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
