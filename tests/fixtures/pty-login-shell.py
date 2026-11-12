#!/usr/bin/env python3
"""A login shell on a real PTY, the way the microsandbox agent opens one.

`bash -s` with a terminal stdin and a terminal stderr is an interactive shell: it prints
its prompt and echoes what it reads, and both land in the stream farm captures. This
stand-in lets a test drive that shell, so the script session must keep its output clean.

Its stdin is the session's input and its stdout the session's output. Closing stdin is not
an EOF for the shell (the agent keeps the PTY master open), so the shell runs until it
exits by itself; a departed client is that process being killed.
"""

import os
import pty
import select
import sys

pid, master = pty.fork()
if pid == 0:
    os.environ.setdefault("TERM", "xterm-256color")
    os.execv("/bin/bash", ["bash", "-l"])

# Read the session's input until it closes, then only the shell's output.
reading = True
while True:
    watch = [fd for fd in (0, master) if fd == master or reading]
    try:
        ready, _, _ = select.select(watch, [], [])
    except InterruptedError:
        continue
    if 0 in ready:
        data = os.read(0, 4096)
        if data:
            os.write(master, data)
        else:
            reading = False
    if master in ready:
        try:
            data = os.read(master, 4096)
        except OSError:
            break
        if not data:
            break
        os.write(1, data)
os.waitpid(pid, 0)
sys.exit(0)
