# Usability review and revision log — 2026-10-28

Scope: scan, browse, ownership filtering, search, refresh, cache persistence, marking and deletion. High priority means a core action is unavailable without restarting, results or selection can silently mislead the user, a recoverable failure loses the session, or deletion intent becomes unclear. This review is not a claim that every filesystem or workload has been validated.

## Round 1 — persistent status information

Findings: errors replaced the marked-file summary; cached age disappeared during refresh/search.

Changes: keep marked count and reclaim estimate visible independently of errors; show cache age in the header; distinguish a finished scan with errors from an error-free completion.

Validation: rendering tests at narrow and wide widths, existing navigation/deletion tests, Clippy.

Next review: refresh still required a restart, and zero-use directories crowded filtered views.

## Round 2 — refresh and visibility

Changes: `r` starts a background refresh, preserving filters and navigation. Scan orchestration lives in `session.rs`. Existing marks are checked again without silently adopting a changed file's metadata. Fatal refresh failure retains a stale, read-only preview and permits retry. `z` hides known zero-use entries while keeping uncertain directories visible. Totals and marks do not depend on this visibility setting.

Validation: unchanged versus modified mark test; root failure and retry; real-terminal refresh; cached refresh; deletion verification remains independent of scan results.

Next review: a single error made unaffected branches uncertain; trees with persistent permission errors could not create a new cache.

## Round 3 — branch completeness and partial snapshots

Changes: track directory enumeration and errors through the tree. Errors affect the branch and its ancestors, not siblings. Live aggregate totals remain provisional until final hard-link reconciliation. Save the newest finished partial snapshot separately from the last complete snapshot. Store error details and per-directory status. Interrupted scans replace neither snapshot; old concurrent scans cannot replace newer results. Retention is by root/configuration, including both snapshot kinds. Version-1 cache migration is transactional and preserves saved results.

Validation: deterministic live-status propagation; real permission-denial fixture where supported; partial/complete roundtrip; canceled writes; older-writer and rollback tests; version-1 migration; native CI on the round-3 commit.

Next review: a disappearing owner could change the preview implicitly, and a root symlink retarget could send live events into an incompatible preview tree.

## Round 4 — selection and recovery edge cases

Changes: retain a missing selected owner explicitly as zero-result rather than switching owners. Return to the nearest surviving ancestor when the current directory disappears. Defer merging when the canonical scan root changes; clear marks outside the new root. Show search/visibility counts and explicit empty-result messages. Add `i` for full-path, ownership, sizes, scan status and marking eligibility. Details are labeled as a snapshot at opening. Exiting after a failed refresh reports an incomplete result. A root-identity mismatch prevents a scan from overwriting another root’s cache. Replace the help/error Boolean with an explicit overlay enum.

Validation: disappearing-owner, removed-directory and changed-root regression tests; details and search rendering tests; a PTY test removes the scan root, verifies the session remains open, restores it and successfully retries. Local full suite: 44 Rust tests; formatting and Clippy; real-terminal browse, search, refresh, failed-refresh recovery, fatal error, interrupt, deletion, cancellation, cache warm/load-refresh (11 sessions).

## Final assessment

No remaining high-priority usability gap identified in the reviewed workflows after these changes. Existing guards still prevent marking/deleting during refresh or after a fatal refresh failure; deletion reviews all marks, including those outside the current search/filter, and verifies files again before removal. A partial scan does not authorize recursive directory deletion.

Remaining medium-priority suggestions:

- Measure startup latency, peak RSS and input latency on million-entry, multi-owner and network-storage trees. The existing performance measurements predate these UI/cache features; do not extrapolate them. Optimize row rebuilding/sorting or add lazy cache loading only with those measurements.
- Add optional subtree-only refresh to reduce metadata traffic on large shared storage, with explicit ancestor accounting and hard-link semantics.
- Add cache management (list snapshots, choose last complete, prune) so recovery/inspection does not require working directly with SQLite.
- Further separate rendering from navigation/filter state. The new scan-session module and explicit completeness/overlay types establish boundaries without a large unrelated rewrite.

Unvalidated environments remain documented in README/PERFORMANCE: real network shares, cold-cache performance and some filesystem-specific behavior. These are validation limits, not evidence of a currently observed high-priority defect.
