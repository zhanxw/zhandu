#!/usr/bin/env python3
"""Exercise real terminal setup, key input, cancellation, and restoration on Unix."""
import errno
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import sqlite3
import struct
import subprocess
import sys
import tempfile
import termios
import time

binary = str(Path(sys.argv[1]).resolve())


def session(path, action, expected, cache_db=None):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 110, 0, 0))
    env = dict(os.environ, TERM="xterm-256color")
    def child_session():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    # Keep the session leader alive until after termios verification: macOS
    # revokes a controlling PTY when its session leader exits.
    wrapper = """
import os, signal, subprocess, sys, termios
original = termios.tcgetattr(0)
child = subprocess.Popen(sys.argv[1:])
for sig in (signal.SIGINT, signal.SIGWINCH):
    signal.signal(sig, lambda sig, frame: child.send_signal(sig))
code = child.wait()
restored = termios.tcgetattr(0) == original
os.write(1, b'ZHANDU_TERMINAL_RESTORED=' + str(restored).encode() + b'\\n')
sys.exit(code)
"""
    proc = subprocess.Popen([sys.executable, "-c", wrapper, binary, "--numeric-ids", *(["--cache-db", str(cache_db)] if cache_db else ["--no-cache"]), str(path)], stdin=slave, stdout=slave, stderr=slave, env=env, preexec_fn=child_session)
    captured = bytearray()

    def pump(timeout=0.05):
        if select.select([master], [], [], timeout)[0]:
            try:
                captured.extend(os.read(master, 65536))
            except OSError as error:
                if error.errno != errno.EIO:
                    raise

    def wait_for(text):
        deadline = time.monotonic() + 15
        while text not in captured:
            pump()
            if time.monotonic() > deadline or proc.poll() is not None:
                raise AssertionError(f"did not render {text!r}: {bytes(captured)[-2000:]!r}")

    try:
        if action == "browse":
            wait_for(b"complete.")
            # Focus and browse the directory pane for both groups and users,
            # return to owners, clear filters, then exercise the full directory view.
            for key in [b"g", b"\x1b[C", b"\x1b[B", b"\x1b[A", b"\r", b"\x1b[D", b"\t", b"u", b"\x1b[C", b"\r", b"\x1b[D", b"\t", b"\x1b", b"d", b"\x1b[B", b"\x1b[A", b"\x1b[C", b"\x1b[D", b"a", b"s", b"?", b"?"]:
                os.write(master, key)
                end = time.monotonic() + 0.15
                while time.monotonic() < end:
                    pump(0.02)
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 8, 32, 0, 0))
            proc.send_signal(signal.SIGWINCH)
            end = time.monotonic() + 0.3
            while time.monotonic() < end:
                pump(0.02)
            os.write(master, b"q")
        elif action == "delete":
            wait_for(b"complete.")
            # Live results preserve the initially selected directory-self row.
            # Home explicitly selects our largest (and only) regular file.
            os.write(master, b"\x1b[H ")
            wait_for(b"marked;")
            os.write(master, b"x")
            wait_for(b"Confirm:")
            assert (path / "delete-me").exists()
            os.write(master, b"\x1b")
            end = time.monotonic() + 0.3
            while time.monotonic() < end:
                pump(0.02)
            assert (path / "delete-me").exists()
            os.write(master, b"xDELETE\r")
            wait_for(b"failed/skipped.")
            assert not (path / "delete-me").exists()
            assert b"reclaimed:" in captured
            os.write(master, b"\rq")
        elif action in ("cache_warm", "cache_refresh"):
            if action == "cache_refresh":
                wait_for(b"CACHED")
            wait_for(b"complete.")
            os.write(master, b"q")
        elif action == "cancel":
            wait_for(b"Scanning")
            os.write(master, b"\x03")
        elif action == "interrupt":
            wait_for(b"complete.")
            proc.send_signal(signal.SIGINT)
        deadline = time.monotonic() + 15
        while proc.poll() is None and time.monotonic() < deadline:
            pump()
        assert proc.poll() is not None, f"process did not exit: {bytes(captured)[-2000:]!r}"
        pump(0.01)
        assert proc.returncode == expected, (action, proc.returncode, bytes(captured)[-2000:])
        assert b"ZHANDU_TERMINAL_RESTORED=True" in captured, f"terminal modes not restored after {action}"
        assert b"\x1b[?1049l" in captured, "alternate screen not restored"
        print(f"PASS: {action}, exit {expected}, terminal restored")
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
        os.close(master)
        os.close(slave)


with tempfile.TemporaryDirectory(prefix="zhandu-pty-") as tmp:
    root = Path(tmp)
    (root / "nested").mkdir()
    (root / "nested" / "data").write_bytes(b"z" * 8192)
    session(root, "browse", 0)
    session(root / "missing", "error", 1)
    session(root, "interrupt", 130)
    with tempfile.TemporaryDirectory(prefix="zhandu-delete-pty-") as delete_tmp:
        delete_root = Path(delete_tmp)
        (delete_root / "delete-me").write_bytes(b"z" * 8192)
        session(delete_root, "delete", 0)
    for i in range(5000):
        (root / f"entry-{i:05}").touch()
    session(root, "cancel", 130)

with tempfile.TemporaryDirectory(prefix="zhandu-cache-pty-") as tmp:
    cache_root = Path(tmp) / "root"
    cache_root.mkdir()
    database = Path(tmp) / "scans.sqlite3"
    (cache_root / "gone").write_bytes(b"old")
    (cache_root / "changed").write_bytes(b"old")
    session(cache_root, "cache_warm", 0, database)
    (cache_root / "gone").unlink()
    (cache_root / "changed").write_bytes(b"x" * 16384)
    (cache_root / "new").write_bytes(b"new")
    session(cache_root, "cache_refresh", 0, database)
    with sqlite3.connect(database) as conn:
        nodes = dict(conn.execute("SELECT name,apparent FROM nodes"))
        assert os.fsencode("gone") not in nodes
        assert nodes[os.fsencode("changed")] == 16384
        assert os.fsencode("new") in nodes
    print("PASS: persistent cache updated across processes, removed entries pruned")
