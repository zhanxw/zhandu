# Performance changes inspired by parallel-disk-usage

Assessed and implemented October 27, 2026. Upstream source inspected at commit `c30e46f16478800d3304264fb9a49bebef9db5c6` (package version 0.24.0). Measurements below compare zhandu with its saved pre-change executable, not against a locally built pdu.

## What makes pdu fast

1. **Parallelize child entries, including files.** Its [tree builder](https://github.com/KSXGitHub/parallel-disk-usage/blob/c30e46f16478800d3304264fb9a49bebef9db5c6/src/tree_builder.rs) processes children through Rayon's parallel iterator and recursively constructs their subtrees. This exposes work even when a directory contains many files and few subdirectories. Our previous scanner parallelized directories but processed metadata within each directory sequentially.
2. **Keep metadata lifetimes short.** Its [filesystem adapter](https://github.com/KSXGitHub/parallel-disk-usage/blob/c30e46f16478800d3304264fb9a49bebef9db5c6/src/fs_tree_builder.rs) extracts required values, drops metadata promptly, and obtains child names from directory enumeration. This suggested avoiding retained full metadata objects and separate file-type queries in zhandu.
3. **Make optional work optional.** pdu's [hard-link option](https://github.com/KSXGitHub/parallel-disk-usage/blob/c30e46f16478800d3304264fb9a49bebef9db5c6/src/args.rs) is opt-in, while its [error-only reporter](https://github.com/KSXGitHub/parallel-disk-usage/blob/c30e46f16478800d3304264fb9a49bebef9db5c6/src/reporter/error_only_reporter.rs) avoids progress bookkeeping for successful entries. Thus, an unconstrained timing comparison can mix different accounting and reporting costs.
4. **Retain less tree data when only a shallow chart is needed.** The same tree builder sums descendants beyond its storage depth without retaining their child arrays. This reduces retained data for that display mode.
5. **Account for storage characteristics.** Its [thread selection](https://github.com/KSXGitHub/parallel-disk-usage/blob/c30e46f16478800d3304264fb9a49bebef9db5c6/src/app.rs) can choose one worker on detected HDD storage. More threads are not universally faster.

These are source-based explanations of available performance mechanisms, not an attribution of a measured percentage of pdu's speed to each mechanism.

## Applied in zhandu

Replaced the jwalk scheduler with a single fixed-size Rayon pool shared by directory jobs and file-metadata jobs. Directory tasks submit child work and return; idle workers can steal metadata batches instead of waiting on a directory-work channel. Directories with at least 256 entries distribute metadata work in batches of 64. Smaller directories avoid that extra scheduling overhead. The worker count remains controlled by `--threads`; a separate coordinator waits outside the worker pool and does not scan files.

Directory enumeration collects names without separately requesting each entry's file type. A single metadata read supplies type, owner, group, allocated/apparent sizes, and hard-link identity. Each full metadata object is immediately reduced to the fields needed downstream. Progress counters update once per metadata batch rather than once per file. Large name lists also sort through the worker pool.

Results pass through a bounded 32-message channel to the collector. Parallel completion order does not determine accounting: sorted depth-first tree construction afterward preserves deterministic node order and hard-link attribution, followed by the existing bottom-up ownership aggregation. The serial path avoids both a worker pool and a channel. Cancellation checks remain in enumeration, metadata work, scheduling, and tree construction.

The TUI and group/user filtering behavior are unchanged. All subdirectories remain available for interactive browsing, so pdu's shallow-tree retention technique is not used. Hard-link deduplication stays enabled, and the TUI keeps scan progress. HDD autodetection is not added; `--threads 1` remains available for rotational or sensitive shared storage.

The replacement also removes jwalk and its now-unused transitive dependencies. Rayon was already present transitively and is now a direct dependency.

## Measurements

macOS ARM64, 4 KiB per regular file, 30,000 files per layout, seven warm-cache repetitions in randomized command order. Each executable received a warmup. No system caches were flushed. End-to-end timings include process startup, scanning, ownership aggregation, and JSON serialization; measured stdout was discarded. This table uses eight workers for both zhandu versions.

| Layout | Before | After | Before / after | System du |
| --- | ---: | ---: | ---: | ---: |
| One folder, 30,000 files | 122.9 ms | 58.8 ms | 2.09× | 47.3 ms |
| 300 sibling folders × 100 files | 41.9 ms | 42.5 ms | 0.99× | 47.3 ms |
| 80 nested folders × 375 files | 218.1 ms | 85.8 ms | 2.54× | 43.1 ms |

The spread layout is essentially unchanged; treat its small timing difference as noise. The main benefit is eliminating the per-directory metadata bottleneck in flat and deep layouts. System du remains faster in the flat and deep fixtures, so this is not a claim that zhandu is the fastest overall disk scanner.

Peak RSS measured in a separate execution:

| Layout | Before, 8 workers | After, 8 workers |
| --- | ---: | ---: |
| One folder, 30,000 files | 26.9 MiB | 19.8 MiB |
| 300 sibling folders × 100 files | 19.1 MiB | 19.2 MiB |
| 80 nested folders × 375 files | 24.4 MiB | 19.8 MiB |

Memory is still proportional to the full tree and directory/owner combinations. This design collects compact scan records before constructing the navigable tree; it is not a constant-memory index. The spread fixture's single-worker peak RSS increased from about 14.5 to 18.6 MiB. Raw results include all one-, four-, and eight-worker measurements and binary SHA-256 hashes.

These fixtures use one owner/group. Multi-owner correctness is tested separately. Cold-cache, HDD, network-storage, very large tree, and Linux performance remain unmeasured. The performance figures should not be generalized to those environments.

## Correctness and validation

- Before/after JSON reports match in completeness, entry count, allocated/apparent/inode totals, directory self-usage, group/user partitions, and immediate children for every benchmark configuration.
- Allocated totals match system du for all three fixtures.
- 13 Rust tests pass. The added 1,024-file test exercises parallel metadata reads, nested directories, symlinks, hard links, deterministic serial/parallel node ordering and accounting, and exact completion progress counts.
- Existing tests continue to cover sparse files, real and synthetic group ownership, combined UID/GID filters, permission errors, exclusions, cancellation, deep trees, and TUI navigation.
- Four real-terminal smoke checks pass, including cancellation and terminal restoration. Formatting and Clippy with warnings denied pass.
- These performance measurements predate the native multi-platform release workflow; they are macOS-only, irrespective of later CI validation. A real cross-device mount fixture remains untested.

## Reproduce

The saved pre-change executable is `benchmark-results/zhandu-before-pdu`. The baseline binary and raw benchmark data are local, ignored build artifacts; copy them separately when transferring this project. The baseline is the build immediately before this optimization, rather than the earlier measurements recorded before renaming zdu.

```sh
cargo build --release --locked
python3 scripts/benchmark.py target/release/zhandu --baseline benchmark-results/zhandu-before-pdu --directories 1 --files-per-dir 30000 --output benchmark-results/pdu-flat.json
python3 scripts/benchmark.py target/release/zhandu --baseline benchmark-results/zhandu-before-pdu --output benchmark-results/pdu-spread.json
python3 scripts/benchmark.py target/release/zhandu --baseline benchmark-results/zhandu-before-pdu --directories 80 --files-per-dir 375 --deep --output benchmark-results/pdu-deep.json
```

Raw results: [flat](benchmark-results/pdu-flat.json), [spread](benchmark-results/pdu-spread.json), [deep](benchmark-results/pdu-deep.json).
