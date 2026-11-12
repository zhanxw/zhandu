**zhandu v0.6.1** adds human-readable sizes to JSON output, addressing [#1](https://github.com/zhanxw/zhandu/issues/1).

```sh
zhandu PATH --json --human-readable
```

- Include `allocated_human` and `apparent_human` strings in totals, directory self-usage, user/group totals, and child entries.
- Use binary units such as `1.0 KiB`, `234.0 MiB`, and `2.0 GiB`, matching the TUI.
- Retain exact numeric byte and inode counts. Default JSON output, filters, and sorting remain unchanged.
- Require `--json` when using `--human-readable`.

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
