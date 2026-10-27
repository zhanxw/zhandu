#!/usr/bin/env python3
"""Package a native build with documentation and dependency license texts."""
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tomllib
import zipfile

root = Path(__file__).resolve().parent.parent
target = sys.argv[1]
version = tomllib.loads((root / 'Cargo.toml').read_text())['package']['version']
tag = f'v{version}'
if os.environ.get('RELEASE_TAG') and os.environ['RELEASE_TAG'] != tag:
    raise SystemExit('Release tag must equal v + Cargo.toml package version')
exe_name = 'zhandu.exe' if 'windows' in target else 'zhandu'
exe = root / 'target' / target / 'release' / exe_name
if subprocess.check_output([str(exe), '--version'], text=True).strip() != f'zhandu {version}':
    raise SystemExit('Built executable has the wrong version')
metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--format-version', '1']))
licenses = []
for package in sorted(metadata['packages'], key=lambda package: package['name']):
    if package['name'] == 'zhandu':
        continue
    folder = Path(package['manifest_path']).parent
    licenses.append(f"\n{'=' * 72}\n{package['name']} {package['version']}\nSPDX: {package.get('license')}\n{package.get('repository') or ''}\n")
    files = set()
    for pattern in ('LICENSE*', 'LICENCE*', 'COPYING*', 'UNLICENSE*'):
        files.update(path for path in folder.glob(pattern) if path.is_file())
    if package.get('license_file'):
        files.add(folder / package['license_file'])
    for path in sorted(files):
        licenses.append(f'\n--- {path.name} ---\n{path.read_text(errors="replace")}\n')
files = {exe_name: exe.read_bytes(), 'THIRD_PARTY_LICENSES.txt': ''.join(licenses).encode()}
for name in ('README.md', 'LICENSE', 'PERFORMANCE.md'):
    files[name] = (root / name).read_bytes()
base = f'zhandu-{tag}-{target}'
dist = root / 'dist'
dist.mkdir(exist_ok=True)
if 'windows' in target:
    archive = dist / f'{base}.zip'
    with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as output:
        for name, content in files.items():
            output.writestr(f'{base}/{name}', content)
else:
    archive = dist / f'{base}.tar.gz'
    with tarfile.open(archive, 'w:gz') as output:
        for name, content in files.items():
            info = tarfile.TarInfo(f'{base}/{name}')
            info.size = len(content)
            info.mode = 0o755 if name == exe_name else 0o644
            output.addfile(info, io.BytesIO(content))
print(archive)
