First public release of **zhandu**, a fast terminal disk usage explorer.

- Parallel directory and file-metadata scanning using a bounded Rayon worker pool.
- Arrow-key browsing with separate owner and directory panels.
- Usage by Unix user/group or Windows owner/primary-group SID.
- Allocated/apparent size switching, hard-link deduplication, JSON output, exclusions, and cancellation.

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
