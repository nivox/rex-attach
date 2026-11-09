#!/usr/bin/env python3
"""Validate details, live activity, and parent badges in a disposable Rex session."""
import argparse
import base64
import codecs
import fcntl
import json
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time
import unicodedata
import uuid


class Screen:
    """Small VT receiver for the cursor-addressed text emitted by Ratatui."""
    def __init__(self, width=120, height=40):
        self.width, self.height = width, height
        self.cells = [[" "] * width for _ in range(height)]
        self.x = self.y = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data):
        self.pending += self.decoder.decode(data)
        while self.pending:
            if self.pending.startswith("\x1b["):
                match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.pending)
                if not match:
                    break
                raw, _, command = match.groups()
                args = [int(v or "0") for v in raw.lstrip("?").split(";")]
                n = args[0] or 1
                if command in "Hf":
                    self.y = min(self.height - 1, max(0, n - 1))
                    self.x = min(self.width - 1, max(0, (args[1] if len(args) > 1 else 1) - 1))
                elif command == "A": self.y = max(0, self.y - n)
                elif command == "B": self.y = min(self.height - 1, self.y + n)
                elif command == "C": self.x = min(self.width - 1, self.x + n)
                elif command == "D": self.x = max(0, self.x - n)
                elif command == "G": self.x = min(self.width - 1, n - 1)
                elif command == "J" and args[0] == 2:
                    self.cells = [[" "] * self.width for _ in range(self.height)]
                elif command == "K":
                    start, end = (0, self.width) if args[0] == 2 else ((0, self.x + 1) if args[0] == 1 else (self.x, self.width))
                    self.cells[self.y][start:end] = [" "] * (end - start)
                self.pending = self.pending[match.end():]
                continue
            if self.pending[0] == "\x1b":
                if len(self.pending) < 2: break
                self.pending = self.pending[2:]
                continue
            ch, self.pending = self.pending[0], self.pending[1:]
            if ch == "\r": self.x = 0
            elif ch == "\n": self.y = min(self.height - 1, self.y + 1)
            elif ch == "\b": self.x = max(0, self.x - 1)
            elif ch >= " " and not unicodedata.combining(ch):
                self.cells[self.y][self.x] = ch
                width = 2 if unicodedata.east_asian_width(ch) in "WF" else 1
                if width == 2 and self.x + 1 < self.width:
                    self.cells[self.y][self.x + 1] = " "
                self.x = min(self.width - 1, self.x + width)

    def lines(self):
        return ["".join(row) for row in self.cells]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--binary", default="target/debug/rex-attach")
    args = parser.parse_args()
    rex = ["rex", "--autostart=false", "--server", args.socket]

    def api(method, payload):
        return json.loads(subprocess.check_output(rex + ["api", "call", method, json.dumps(payload)], timeout=15))

    program = r'''
import base64,sys
sys.stdout.write("\033]2;Activity smoke\007\033]7;file://localhost/tmp\007READY\n")
sys.stdout.flush()
for line in sys.stdin:
    state=line.strip()
    message={"blocked":"Approve deployment","working":"Compiling project","done":"Build complete","error":"Build failed"}.get(state,"")
    extra=":kind=permission" if state=="blocked" else (":progress=42" if state=="working" else "")
    msg=base64.b64encode(message.encode()).decode()
    sys.stdout.write(f"\033]7501;state={state}:app=smoke{extra}:msg={msg}\007")
    sys.stdout.flush()
'''
    def block(label):
        return {"block": {"flavor": "com.superlogical.terminal.shell", "label": label,
                          "options": {"command": [sys.executable, "-u", "-c", program]}}}
    label = "activity-" + uuid.uuid4().hex[:6]
    created = api("session.create", {"label": label, "initial_windows": [
        {"window_label": "primary", "layout": block("alpha")},
        {"window_label": "split", "layout": {"split": {"direction": "horizontal", "before": block(""), "after": block("gamma")}}},
    ]})
    session = created["session_id"]
    beta, gamma = created["initial_windows"][1]["block_ids"]
    master = slave = process = None
    try:
        master, slave = pty.openpty()
        before = termios.tcgetattr(slave)
        flags = fcntl.fcntl(slave, fcntl.F_GETFL)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        process = subprocess.Popen([args.binary, "--socket", args.socket, "--session", session, "--preview", "text"], stdin=slave, stdout=slave, stderr=slave, preexec_fn=controlling_terminal, env={**os.environ, "TERM": "xterm-256color"})
        screen = Screen()
        def wait(predicate, description):
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if select.select([master], [], [], 0.05)[0]: screen.feed(os.read(master, 32768))
                if predicate(screen.lines()): return
                assert process.poll() is None, "client exited during " + description
            raise AssertionError(description + "\n" + "\n".join(screen.lines()))
        def info(lines): return " ".join(line[91:119].strip() for line in lines[2:20])
        def has_badge(lines, name, badge, left, right): return any(re.search(re.escape(badge) + r"\s+" + re.escape(name), line[left:right]) for line in lines)
        def report(block_id, state):
            api("com.superlogical.terminal.write", {"session_id": session, "block_id": block_id, "args": {"data": state + "\n"}})
        wait(lambda lines: "Activity smoke" in "\n".join(lines) and "Process: python3" in "\n".join(lines) and "Status:" not in "\n".join(lines), "initial metadata with unreported status hidden")
        report(gamma, "blocked")
        wait(lambda lines: has_badge(lines, label, "🙋", 0, 40) and has_badge(lines, "split", "🙋", 40, 80), "unselected block propagates to tab and session")
        os.write(master, b"jlj")
        wait(lambda lines: all(value in info(lines) for value in ["Label: gamma", "Type:", "Flavor:", "Activity smoke", "file://localhost/", "Process: python3", "needs input", "Approve deployment"]), "selected block details and blocked message")
        assert has_badge(screen.lines(), "gamma", "🙋", 60, 90)
        wait(lambda lines: any("Activity smoke" in line[60:90] for line in lines), "default shell label uses terminal title in block column")
        os.write(master, b"\r")
        wait(lambda lines: any(window.get("focused_block_id") == gamma for window in api("session.view", {"session_id": session})["windows"]), "interactive attachment focuses selected block")
        os.write(master, b"\x02d")
        wait(lambda lines: "Detach from a block with" in "\n".join(lines) and "Label: gamma" in info(lines), "detach restores browser selection")
        report(gamma, "working")
        wait(lambda lines: "working 42%" in info(lines) and "Compiling project" in info(lines) and has_badge(lines, label, "🔄", 0, 30), "working status and progress")
        report(gamma, "done")
        wait(lambda lines: "Build complete" in info(lines) and has_badge(lines, label, "✅", 0, 24), "done status")
        report(beta, "error")
        wait(lambda lines: has_badge(lines, label, "❌", 0, 30) and has_badge(lines, "split", "❌", 30, 60) and has_badge(lines, "Activity smoke", "❌", 60, 90), "sibling error takes precedence at parents")
        report(beta, "clear")
        wait(lambda lines: has_badge(lines, label, "✅", 0, 24), "cleared sibling restores parent's remaining status")
        report(gamma, "clear")
        wait(lambda lines: "Status:" not in info(lines) and "Build complete" not in info(lines) and any(label in line[:24] and not any(icon in line[:24] for icon in "🙋❌🔄✅💤") for line in lines), "clearing last record removes status decoration and stale messages")
        os.write(master, b"\x1b")
        process.wait(timeout=5)
        assert process.returncode == 0
        assert termios.tcgetattr(slave) == before
        assert fcntl.fcntl(slave, fcntl.F_GETFL) == flags
        print("PASS: default shell title, attachment focus, block details, half-height preview, live statuses/messages/progress, parent precedence, clearing, terminal cleanup")
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()
        for fd in (master, slave):
            if fd is not None: os.close(fd)
        subprocess.run(rex + ["kill", session], check=True, timeout=15)


if __name__ == "__main__":
    main()
