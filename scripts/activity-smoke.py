#!/usr/bin/env python3
"""Validate details, live activity, and parent badges in a disposable Rex session."""
from smoke_support import Screen, api_client, open_terminal, controlling_terminal
import argparse
import fcntl
import os
import re
import select
import subprocess
import sys
import termios
import time
import uuid



def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--binary", default="target/debug/rex-attach")
    args = parser.parse_args()
    api = api_client(args.socket)

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
        master, slave, before, flags = open_terminal(120, 40)
        process = subprocess.Popen([args.binary, "--socket", args.socket, "--session", session, "--preview", "text", "--preview-layout", "row"], stdin=slave, stdout=slave, stderr=slave, preexec_fn=controlling_terminal, env={**os.environ, "TERM": "xterm-256color"})
        screen = Screen()
        def wait(predicate, description):
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if select.select([master], [], [], 0.05)[0]: screen.feed(os.read(master, 32768))
                if predicate(screen.lines()): return
                assert process.poll() is None, "client exited during " + description
            raise AssertionError(description + "\n" + "\n".join(screen.lines()))
        def info(lines):
            header = next((i for i, line in enumerate(lines) if line.startswith("┌Block information")), None)
            if header is None: return ""
            return " ".join(line[1:59].strip() for line in lines[header + 1:-2])
        def has_badge(lines, name, badge, left, right): return any(re.search(re.escape(badge) + r"\s+" + re.escape(name), line[left:right]) for line in lines)
        def report(block_id, state):
            api("com.superlogical.terminal.write", {"session_id": session, "block_id": block_id, "args": {"data": state + "\n"}})
        wait(lambda lines: "Activity smoke" in "\n".join(lines) and "Process: python3" in "\n".join(lines) and "Status:" not in "\n".join(lines), "initial metadata with unreported status hidden")
        report(gamma, "blocked")
        wait(lambda lines: has_badge(lines, label, "🙋", 0, 40) and has_badge(lines, "split", "🙋", 40, 80), "unselected block propagates to tab and session")
        os.write(master, b"jlj")
        wait(lambda lines: all(value in info(lines) for value in ["Label: gamma", "Type:", "Flavor:", "Activity smoke", "file://localhost/", "Process: python3", "needs input", "Approve deployment"]), "selected block details and blocked message")
        assert has_badge(screen.lines(), "gamma", "🙋", 80, 120)
        wait(lambda lines: any("Activity smoke" in line[80:120] for line in lines), "default shell label uses terminal title in block column")
        os.write(master, b"\r")
        wait(lambda lines: any(window.get("focused_block_id") == gamma for window in api("session.view", {"session_id": session})["windows"]), "interactive attachment focuses selected block")
        os.write(master, b"\x02d")
        wait(lambda lines: "Detach from a block with" in "\n".join(lines) and "Label: gamma" in info(lines), "detach restores browser selection")
        report(gamma, "working")
        wait(lambda lines: "working 42%" in info(lines) and "Compiling project" in info(lines) and has_badge(lines, label, "🔄", 0, 30), "working status and progress")
        report(gamma, "done")
        wait(lambda lines: "Build complete" in info(lines) and has_badge(lines, label, "✅", 0, 24), "done status")
        report(beta, "error")
        wait(lambda lines: has_badge(lines, label, "❌", 0, 40) and has_badge(lines, "split", "❌", 40, 80) and has_badge(lines, "Activity smoke", "❌", 80, 120), "sibling error takes precedence at parents")
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
        api("session.destroy", {"session_id": session})


if __name__ == "__main__":
    main()
