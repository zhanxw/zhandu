#!/usr/bin/env python3
"""Offline installer checks: first install, upgrade, pinning, corrupt/missing assets."""
import hashlib
import io
import os
from pathlib import Path
import platform
import subprocess
import tarfile
import tempfile

installer = Path(__file__).resolve().parent.parent / 'install.sh'
with tempfile.TemporaryDirectory(prefix='zhandu-installer-') as tmp:
    root = Path(tmp)
    fixtures = root / 'fixtures'
    fixtures.mkdir()
    tools = root / 'tools'
    tools.mkdir()
    arch = 'aarch64' if platform.machine() in ('arm64', 'aarch64') else 'x86_64'
    target = arch + ('-apple-darwin' if platform.system() == 'Darwin' else '-unknown-linux-gnu')
    for version in ('v1.0.0', 'v1.0.1'):
        folder = f'zhandu-{version}-{target}'
        directory = fixtures / version
        directory.mkdir()
        archive = directory / f'{folder}.tar.gz'
        with tarfile.open(archive, 'w:gz') as tar:
            content = f'#!/bin/sh\necho zhandu {version[1:]}\n'.encode()
            info = tarfile.TarInfo(folder + '/zhandu')
            info.mode = 0o755
            info.size = len(content)
            tar.addfile(info, io.BytesIO(content))
        (directory / 'SHA256SUMS').write_text(f'{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n')
    curl = tools / 'curl'
    curl.write_text('''#!/usr/bin/env python3
import os, pathlib, shutil, sys
args=sys.argv[1:]
url=next(a for a in args if a.startswith('https://'))
assert url.startswith('https://github.com/zhanxw/zhandu/releases/')
if url.endswith('/latest'):
 print('https://github.com/zhanxw/zhandu/releases/tag/v1.0.1', end='')
else:
 version,name=url.split('/')[-2:]
 source=pathlib.Path(os.environ['FIXTURES'])/version/name
 if not source.exists(): sys.exit(22)
 shutil.copyfile(source,args[args.index('-o')+1])
''')
    curl.chmod(0o755)
    dest = root / 'install with spaces'
    env = dict(os.environ, PATH=str(tools)+os.pathsep+os.environ['PATH'], FIXTURES=str(fixtures), ZHANDU_INSTALL_DIR=str(dest))
    env.pop('ZHANDU_VERSION', None)
    def run(version=None, success=True):
        settings = dict(env)
        if version: settings['ZHANDU_VERSION'] = version
        result = subprocess.run(['sh', str(installer)], env=settings, capture_output=True, text=True)
        assert (result.returncode == 0) == success, result.stdout + result.stderr
    run('v1.0.0')
    binary = dest / 'zhandu'
    assert subprocess.check_output([str(binary), '--version']).strip() == b'zhandu 1.0.0'
    run()
    assert subprocess.check_output([str(binary), '--version']).strip() == b'zhandu 1.0.1'
    before = binary.read_bytes()
    run()  # idempotent reinstall
    run('v9.9.9', success=False)
    run('../invalid', success=False)
    manifest = fixtures / 'v1.0.1' / 'SHA256SUMS'
    good = manifest.read_text()
    manifest.write_text('0'*64 + '  ' + good.split()[1] + '\n')
    run(success=False)
    manifest.write_text('')
    run(success=False)
    assert binary.read_bytes() == before
    assert not list(dest.glob('.zhandu-install.*'))
    print('PASS: install, upgrade, reinstall, pinning, missing downloads, invalid version, checksum rejection, preservation, cleanup')
