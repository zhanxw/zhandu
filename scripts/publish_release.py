#!/usr/bin/env python3
"""Publish a release only after all expected archives have passed the build jobs."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

tag = os.environ['RELEASE_TAG']
if not re.fullmatch(r'v\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?', tag):
    raise SystemExit('Invalid release tag')
targets = {
    'x86_64-unknown-linux-gnu': '.tar.gz',
    'aarch64-unknown-linux-gnu': '.tar.gz',
    'x86_64-apple-darwin': '.tar.gz',
    'aarch64-apple-darwin': '.tar.gz',
    'x86_64-pc-windows-msvc': '.zip',
}
expected = {f'zhandu-{tag}-{target}{extension}' for target, extension in targets.items()}
dist = Path('dist')
actual = {path.name for path in dist.iterdir() if path.is_file()}
if actual != expected:
    raise SystemExit(f'Expected exactly five release archives; got {sorted(actual)}')
manifest = dist / 'SHA256SUMS'
manifest.write_text(''.join(f'{hashlib.sha256((dist / name).read_bytes()).hexdigest()}  {name}\n' for name in sorted(expected)))
existing = subprocess.run(['gh', 'release', 'view', tag, '--json', 'isDraft'], capture_output=True, text=True)
if existing.returncode == 0:
    if not json.loads(existing.stdout)['isDraft']:
        raise SystemExit('Refusing to replace an already published release')
else:
    subprocess.run(['gh', 'release', 'create', tag, '--verify-tag', '--draft', '--title', f'zhandu {tag}', '--notes-file', 'RELEASE_NOTES.md'], check=True)
assets = [str(dist / name) for name in sorted(expected | {'SHA256SUMS'})]
subprocess.run(['gh', 'release', 'upload', tag, *assets, '--clobber'], check=True)
release = json.loads(subprocess.check_output(['gh', 'release', 'view', tag, '--json', 'assets']))
if {asset['name'] for asset in release['assets']} != expected | {'SHA256SUMS'}:
    raise SystemExit('Release asset verification failed; leaving release as a draft')
subprocess.run(['gh', 'release', 'edit', tag, '--draft=false', '--latest'], check=True)
