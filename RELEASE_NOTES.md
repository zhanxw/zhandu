**zhandu v0.2.0** adds a reviewed, file-only deletion workflow.

- **Space** marks/unmarks the highlighted regular file and advances; **d** still opens the file listing.
- Visible `[x]` marks persist across navigation; the status shows marked count and estimated reclaimable space.
- **x** reviews all marked paths; type **DELETE** and Enter to permanently delete, or Esc to cancel. **c** clears marks.
- Directories, symlinks and special files cannot be marked; owner filtering never implies recursive deletion.
- Changed files/parents are skipped; failures are reported. A background rescan refreshes totals after deletion.
- Space estimates account for hard links; actual free space can differ due to open files, snapshots and shared blocks.
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
