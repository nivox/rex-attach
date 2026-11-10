#!/usr/bin/env python3
"""Exercise mouse input and local history in disposable Rex sessions, on both renderers."""
from smoke_support import Screen, api_client, open_terminal, controlling_terminal, wait_until, set_size
import argparse
import os
import select
import subprocess
import sys
import tempfile
import termios
import time
import uuid


def run(args, backend):
    api = api_client(args.socket)
    with tempfile.TemporaryDirectory(prefix="rex-attach-mouse-") as directory:
        log = directory + "/input"
        program = r'''
import os,sys,tty
tty.setraw(0)
def emit(data): os.write(1,data)
emit(b"\x1b[?1000h\x1b[?1006hMOUSE_READY\r\n")
with open(sys.argv[1],"ab",buffering=0) as log:
    while True:
        data=os.read(0,4096)
        if not data: break
        log.write(data)
        if data == b"L": emit(b"\x1b[?1006lLEGACY_READY\r\n")
        if data == b"M": emit(b"\x1b[?1000l\x1b[?1003h\x1b[?1006hMOTION_READY\r\n")
        if data == b"N":
            emit(b"\x1b[?1000l\x1b[?1003l")
            for n in range(80): emit(f"HISTORY_{n:03}\r\n".encode())
            emit(b"HISTORY_BOTTOM\r\n")
        if data == b"T": emit(b"TYPING_READY\r\n")
'''
        layout = {"block": {"flavor": "com.superlogical.terminal.shell", "label": "recorder",
                            "options": {"command": [sys.executable, "-u", "-c", program, log]}}}
        session = api("session.create", {"label": "mouse-" + uuid.uuid4().hex[:6],
                      "initial_windows": [{"window_label": "recording", "layout": layout}]})["session_id"]
        master, slave, before, _ = open_terminal(120, 20)
        process = None
        try:
            process = subprocess.Popen([args.binary, "--socket", args.socket, "--session", session,
                                        "--preview", "text", "--renderer", backend],
                                       stdin=slave, stdout=slave, stderr=slave, preexec_fn=controlling_terminal,
                                       env={**os.environ, "TERM": "xterm-256color"})
            screen = Screen(120, 20)
            output = bytearray()

            def pump():
                while select.select([master], [], [], .02)[0]:
                    chunk = os.read(master, 65536)
                    output.extend(chunk)
                    screen.feed(chunk)
                assert process.poll() is None, "\n".join(screen.lines())

            def wait(predicate, message):
                wait_until(predicate, lambda: message + "\n" + "\n".join(screen.lines()), pump)

            def visible(text): return text in "\n".join(screen.lines())
            def send(data): os.write(master, data)
            def recorded():
                with open(log, "rb") as source: return source.read()
            def report(code=0, x=110, y=5, end="M"):
                return f"\x1b[<{code};{x};{y}{end}".encode()
            def check_input(data, expected):
                start = len(recorded())
                send(data)
                wait(lambda: recorded()[start:] == expected, f"expected remote bytes {expected!r}, got {recorded()[start:]!r}")

            wait(lambda: visible("recording") and visible("Tabs"), "browser ready")
            assert b"\x1b[?1006h" in output and b"\x1b[?1000h" in output
            row = next(y for y, line in enumerate(screen.lines()) if "recording" in line)
            col = screen.lines()[row].index("recording")
            send(report(x=col + 1, y=row + 1) + report(x=col + 1, y=row + 1, end="m"))
            pump()
            send(report(x=col + 1, y=row + 1))
            wait(lambda: visible("MOUSE_READY") and not visible("Detach from a block with"), "double-click attaches")
            # Trailing mouse events from the browser must not reach the application.
            check_input(report() + report(end="m"), report() + report(end="m"))
            check_input(report(64), report(64))
            # Fragmented reports, with a delay beyond the keyboard escape timeout.
            start = len(recorded())
            send(b"\x1b[<0;")
            deadline = time.monotonic() + .12
            while time.monotonic() < deadline: pump()
            send(b"110;5M")
            wait(lambda: recorded()[start:] == report(), "fragmented click")
            check_input(report(end="m"), report(end="m"))
            check_input(b"L", b"L")
            wait(lambda: visible("LEGACY_READY"), "legacy protocol enabled")
            check_input(report(), b"\x1b[M " + bytes([110 + 32, 5 + 32]))
            check_input(report(end="m"), b"\x1b[M#" + bytes([110 + 32, 5 + 32]))
            check_input(b"M", b"M")
            wait(lambda: visible("MOTION_READY") and b"\x1b[?1003h" in output, "motion enabled")
            check_input(report(35), report(35))
            check_input(report(), report())
            release_start = len(recorded())
            send(b"\x02?")
            wait(lambda: visible("Shortcut help"), "help opens")
            wait(lambda: recorded()[release_start:] == report(end="m"), "help releases held mouse button")
            start = len(recorded())
            send(report(65) + report())
            pump()
            assert len(recorded()) == start, "help leaked mouse events"
            send(b"q")
            wait(lambda: visible("MOTION_READY") and not visible("Shortcut help"), "help closes")
            check_input(report(35, x=111), report(35, x=111))
            check_input(b"N", b"N")
            wait(lambda: visible("HISTORY_BOTTOM"), "tracking disabled")
            start = len(recorded())
            send(report(64) * 6)
            wait(lambda: not visible("HISTORY_BOTTOM") and visible("HISTORY_"), "wheel scrolls history")
            assert len(recorded()) == start, "local scroll leaked input"
            check_input(b"T", b"T")
            wait(lambda: visible("HISTORY_BOTTOM") and visible("TYPING_READY"), "typing returns to live viewport")
            set_size(slave, 100, 20)
            wait(lambda: api("com.superlogical.terminal.size", {"session_id":session,
                 "block_id": api("session.view", {"session_id":session})["windows"][0]["layers"][0]["blocks"][0]["block_id"], "args": {}})["columns"] == 100, "resize")
            send(b"\x02d")
            wait(lambda: visible("Detach from a block with"), "detach to browser")
            send(b"q")
            process.wait(timeout=5)
            while select.select([master], [], [], .02)[0]: output.extend(os.read(master, 65536))
            assert process.returncode == 0
            assert termios.tcgetattr(slave) == before
            assert b"\x1b[?1006l" in output[-256:], "mouse mode leaked on exit"
            print(f"PASS ({backend}): browser clicks, SGR/legacy/fragmented mouse, wheel, motion, modal help, history, resize, detach and cleanup")
        finally:
            if process is not None and process.poll() is None:
                process.kill()
                process.wait()
            os.close(master)
            os.close(slave)
            api("session.destroy", {"session_id": session})


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--binary", default="target/debug/rex-attach")
    args = parser.parse_args()
    for backend in ("ansi", "ratatui"):
        run(args, backend)
