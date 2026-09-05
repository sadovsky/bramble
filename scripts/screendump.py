#!/usr/bin/env python3
"""Ask a running QEMU for a framebuffer screenshot over QMP."""
import json
import socket
import sys
import time

sock_path, out = sys.argv[1], sys.argv[2]

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(40):
    try:
        s.connect(sock_path)
        break
    except (FileNotFoundError, ConnectionRefusedError):
        time.sleep(0.25)
else:
    sys.exit("could not reach the qemu monitor")

f = s.makefile("rw")
f.readline()  # greeting
for cmd in ({"execute": "qmp_capabilities"}, {"execute": "screendump", "arguments": {"filename": out}}):
    f.write(json.dumps(cmd) + "\n")
    f.flush()
    while True:
        line = f.readline()
        if not line:
            sys.exit("qemu closed the monitor")
        msg = json.loads(line)
        if "return" in msg or "error" in msg:
            if "error" in msg:
                sys.exit(f"qmp error: {msg['error']}")
            break
