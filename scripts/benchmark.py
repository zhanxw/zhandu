#!/usr/bin/env python3
"""Reproducible warm-cache end-to-end benchmark; creates/removes only its own fixture."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import shutil
import statistics
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=Path)
parser.add_argument("--directories", type=int, default=300)
parser.add_argument("--files-per-dir", type=int, default=100)
parser.add_argument("--repeats", type=int, default=7)
parser.add_argument("--baseline", type=Path, help="Compare a saved pre-change executable on the same fixture")
parser.add_argument("--deep", action="store_true", help="Nest the directories into a chain (keep path length within platform limits)")
parser.add_argument("--output", type=Path, default=Path("benchmark-results/local.json"))
args = parser.parse_args()
binary = str(args.binary.resolve())
assert min(args.directories, args.files_per_dir, args.repeats) > 0

with tempfile.TemporaryDirectory(prefix="zhandu-bench-") as tmp:
    root = Path(tmp)
    payload = bytes(4096)
    parent = root
    for d in range(args.directories):
        directory = parent / f"d{d:03}"
        directory.mkdir()
        for f in range(args.files_per_dir):
            (directory / f"file-{f:05}").write_bytes(payload)
        if args.deep:
            parent = directory
    commands = {f"zhandu-{threads}-workers": [binary, "--json", "--threads", str(threads), tmp] for threads in [1, 4, 8]}
    commands["system-du"] = [shutil.which("du"), "-sk", tmp]
    if args.baseline:
        for threads in (1, 4, 8):
            commands[f"before-{threads}-workers"] = [str(args.baseline.resolve()), "--json", "--threads", str(threads), tmp]
    timings = {label: [] for label in commands}
    outputs = {}
    # Explicit warmup; do not flush system caches.
    for label, command in commands.items():
        outputs[label] = subprocess.check_output(command)
    reference = json.loads(outputs["zhandu-1-workers"])
    for threads in (4, 8):
        assert json.loads(outputs[f"zhandu-{threads}-workers"])["total"] == reference["total"]
    for label, output in outputs.items():
        if label == "system-du":
            continue
        actual = json.loads(output)
        for key in ("complete", "entries", "total", "directory_self", "groups", "users", "children", "error_count"):
            assert actual[key] == reference[key], f"{label}: accounting mismatch in {key}"
    du_kib = int(outputs["system-du"].split()[0])
    assert du_kib == (reference["total"]["allocated"] + 1023) // 1024, "du and zhandu totals differ"
    rng = random.Random(42)
    for _ in range(args.repeats):
        order = list(commands)
        rng.shuffle(order)
        for label in order:
            begin = time.perf_counter()
            subprocess.run(commands[label], stdout=subprocess.DEVNULL, check=True)
            timings[label].append(time.perf_counter() - begin)
    memory = {}
    for label, command in commands.items():
        child = subprocess.Popen(command, stdout=subprocess.DEVNULL)
        _, status, usage = os.wait4(child.pid, 0)
        child.returncode = os.waitstatus_to_exitcode(status)
        assert child.returncode == 0
        memory[label] = usage.ru_maxrss * (1 if platform.system() == "Darwin" else 1024)
    report = {
        "platform": platform.platform(), "architecture": platform.machine(),
        "zhandu_version": subprocess.check_output([binary, "--version"], text=True).strip(),
        "binary_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest(),
        "baseline_sha256": hashlib.sha256(args.baseline.read_bytes()).hexdigest() if args.baseline else None,
        "cache": "warm; one warmup per command, randomized measurement order; no cache flushing",
        "scope": "end-to-end subprocess wall time, output discarded; zhandu includes ownership aggregation and JSON serialization",
        "fixture": {"directories": args.directories, "regular_files": args.directories * args.files_per_dir, "bytes_per_file": 4096, "deep": args.deep},
        "total": reference["total"], "allocated_total_matches_system_du": True,
        "runs": {label: {"median_seconds": statistics.median(samples), "min_seconds": min(samples), "max_seconds": max(samples),
                         "samples_seconds": samples, "peak_rss_bytes_separate_run": memory.get(label)} for label, samples in timings.items()},
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    for label, result in report["runs"].items():
        print(f"{label:18} {result['median_seconds'] * 1000:8.1f} ms  peak RSS {result['peak_rss_bytes_separate_run']}")
    print(f"Verified allocated totals against system du. Saved {args.output}")
