"""Shared infrastructure for disposable Rex integration checks."""
import codecs
import fcntl
import json
import os
import pty
import re
import struct
import subprocess
import termios
import time
import unicodedata


def api_client(socket):
    command = ["rex", "--autostart=false", "--server", socket, "api", "call"]
    def call(method, payload):
        return json.loads(subprocess.check_output(command + [method, json.dumps(payload)], timeout=15))
    return call


def set_size(fd, cols, rows):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def open_terminal(cols=120, rows=40):
    master, slave = pty.openpty()
    try:
        before = termios.tcgetattr(slave)
        flags = fcntl.fcntl(slave, fcntl.F_GETFL)
        set_size(slave, cols, rows)
        return master, slave, before, flags
    except BaseException:
        os.close(master)
        os.close(slave)
        raise


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def wait_until(predicate, message, pump, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        pump()
        if predicate():
            return
    raise AssertionError(message() if callable(message) else message)


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
