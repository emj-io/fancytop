#!/usr/bin/env python3
"""Run fancytop in a pseudo-terminal and check it quits and restores the terminal on each signal.

usage: scripts/pty-smoke.py target/release/fancytop
"""
import os
import pty
import select
import signal
import sys
import time

ALT_SCREEN_OFF = b"\x1b[?1049l"
MOUSE_OFF = b"\x1b[?1000l"


def run(binary, how):
    pid, fd = pty.fork()
    if pid == 0:
        os.execv(binary, [binary])
    time.sleep(2)
    if how == "q":
        os.write(fd, b"q")
    elif how == "ctrl-c":
        os.write(fd, b"\x03")
    else:
        os.kill(pid, getattr(signal, how))
    out = b""
    end = time.time() + 5
    while time.time() < end:
        ready, _, _ = select.select([fd], [], [], 0.2)
        if ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            out += chunk
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            return os.waitstatus_to_exitcode(status), out
    # The pty closing ends the read loop first; give the process a moment to be reaped.
    while time.time() < end:
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            return os.waitstatus_to_exitcode(status), out
        time.sleep(0.05)
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
    return None, out


failed = False
for how in ["q", "ctrl-c", "SIGINT", "SIGTERM", "SIGHUP"]:
    code, out = run(sys.argv[1], how)
    ok = code == 0 and ALT_SCREEN_OFF in out and MOUSE_OFF in out
    print(f"{how:8} exit={code} restored={ALT_SCREEN_OFF in out and MOUSE_OFF in out} {'ok' if ok else 'FAIL'}")
    failed |= not ok
sys.exit(failed)
