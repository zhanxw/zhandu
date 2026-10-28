**zhandu v0.3.0** shows live, browsable scan results.

- Open the TUI as soon as the scan root is ready; entries, sizes, owners and errors arrive in batches.
- Refresh partial results up to five times per second while retaining directory and selection across updates.
- Browse subdirectories and filter by user/group during scanning.
- Clearly label provisional totals; reconcile with the final deterministic hard-link accounting.
- Keep marking/deletion disabled until scanning completes. JSON remains a final report.
- Use a bounded update queue; retain an additional partial tree while scanning.
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
