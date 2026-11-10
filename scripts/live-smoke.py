#!/usr/bin/env python3
"""Exercise interactive attachment in a disposable session; requires Rex and a PTY."""
from smoke_support import api_client, open_terminal, controlling_terminal
import argparse
import fcntl
import os
import re
import select
import struct
import subprocess
import termios
import time
import uuid
import base64
import zlib


class KittyTerminal:
    """Minimal graphics receiver: negotiate, decode RGBA frames, track deletions."""
    def __init__(self, enabled):
        self.enabled = enabled
        self.pending = bytearray()
        self.queries = bytearray()
        self.font_replied = self.status_replied = False
        self.current = None
        self.frames = []
        self.deleted = set()

    def feed(self, chunk):
        replies = bytearray()
        if self.enabled:
            self.queries.extend(chunk)
            if not self.font_replied and b"\x1b[16t" in self.queries:
                replies.extend(b"\x1b[6;20;10t")
                self.font_replied = True
            if not self.status_replied and b"\x1b[5n" in self.queries:
                replies.extend(b"\x1b[0n")
                self.status_replied = True
            self.queries = self.queries[-64:]
        self.pending.extend(chunk)
        while True:
            start = self.pending.find(b"\x1b_G")
            if start < 0:
                self.pending = self.pending[-2:]
                break
            end = self.pending.find(b"\x1b\\", start + 3)
            if end < 0:
                self.pending = self.pending[start:]
                break
            command = bytes(self.pending[start + 3:end])
            del self.pending[:end + 2]
            header, _, payload = command.partition(b";")
            fields = dict(part.split(b"=", 1) for part in header.split(b",") if b"=" in part)
            action = fields.get(b"a")
            if action == b"q" and self.enabled:
                # Reply in front of final status, preserving terminal request order.
                replies[:0] = b"\x1b_Gi=" + fields[b"i"] + b";OK\x1b\\"
            elif action == b"d" and fields.get(b"d") == b"I":
                self.deleted.add(int(fields[b"i"]))
            elif action == b"T":
                self.current = [fields, bytearray()]
            if self.current is not None and payload and action != b"q":
                self.current[1].extend(base64.b64decode(payload, validate=True))
                if fields.get(b"m", b"0") == b"0":
                    metadata, data = self.current
                    if metadata.get(b"o") == b"z":
                        data = zlib.decompress(data)
                    width, height = int(metadata[b"s"]), int(metadata[b"v"])
                    assert len(data) == width * height * 4
                    assert width <= 1200 and height <= 800
                    self.frames.append((int(metadata[b"i"]), width, height, bytes(data)))
                    self.current = None
        return replies

    def save(self, path):
        _, width, height, pixels = self.frames[-1]
        def part(kind, data):
            return struct.pack("!I", len(data)) + kind + data + struct.pack("!I", zlib.crc32(kind + data))
        raw = b"".join(b"\0" + pixels[y * width * 4:(y + 1) * width * 4] for y in range(height))
        with open(path, "wb") as out:
            out.write(b"\x89PNG\r\n\x1a\n" + part(b"IHDR", struct.pack("!IIBBBBB", width, height, 8, 6, 0, 0, 0)) + part(b"IDAT", zlib.compress(raw)) + part(b"IEND", b""))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--binary", default="target/debug/rex-attach")
    parser.add_argument("--mode", choices=["direct", "session", "all"], default="direct")
    parser.add_argument("--exit-key", choices=["ctrl-c", "esc"], default="ctrl-c")
    parser.add_argument("--multi-block-tab", action="store_true")
    parser.add_argument("--preview-mode", choices=["auto", "text", "kitty"], default="text")
    parser.add_argument("--preview-layout", choices=["column", "row"], default="column")
    parser.add_argument("--preview-font")
    parser.add_argument("--kitty-terminal", action="store_true")
    parser.add_argument("--image-out")
    args = parser.parse_args()
    api = api_client(args.socket)

    label = "smoke-" + uuid.uuid4().hex[:6]
    def shell():
        return {"block": {"flavor": "com.superlogical.terminal.shell",
                          "options": {"command": ["sh", "-c", "printf '\\033[38;2;80;200;140mREX_PREVIEW_READY\\033[0m\\n'; exec sh"]}}}
    windows = [{"window_label": "single", "layout": shell()}]
    if args.multi_block_tab:
        windows.append({"window_label": "split", "layout": {"split": {
            "direction": "horizontal", "before": shell(), "after": shell()}}})
    created = api("session.create", {
        "label": label,
        "initial_windows": windows,
    })
    session = created["session_id"]
    master = slave = None
    process = None
    try:
        block = created["initial_windows"][1 if args.multi_block_tab else 0]["block_ids"][1 if args.multi_block_tab else 0]
        before_size = api("com.superlogical.terminal.size", {"session_id": session, "block_id": block})
        def owner():
            return next(b for b in api("session.list_blocks", {"session_id": session})["blocks"] if b["block_id"] == block).get("owner")
        before_owner = owner()
        command = [args.binary, "--socket", args.socket,
                   "--session", session, "--block", block]
        interactive_command = [args.binary, "--socket", args.socket]
        interactive_command += ["--preview", args.preview_mode]
        interactive_command += ["--preview-layout", args.preview_layout]
        if args.preview_font:
            interactive_command += ["--preview-font", args.preview_font]
        if args.mode in ("session", "direct"):
            interactive_command += ["--session", session]
        if args.mode == "direct":
            interactive_command += ["--block", block]
        master, slave, before, before_flags = open_terminal(120, 40)


        process = subprocess.Popen(interactive_command, stdin=slave, stdout=slave, stderr=slave,
                                   preexec_fn=controlling_terminal,
                                   env={**os.environ, "TERM": "xterm-256color"})
        output = bytearray()
        graphics = KittyTerminal(args.kitty_terminal)
        seed_frames = 0
        sent_input = False
        sent_detach = False
        picked = args.mode == "direct"
        navigated = False
        preview_seen = False
        preview_seeded = False
        preview_toggle = 0
        saw_blocks_column = False
        returned_to_browser = False
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.05)[0]:
                chunk = os.read(master, 4096)
                output.extend(chunk)
                reply = graphics.feed(chunk)
                if reply:
                    os.write(master, reply)
                # Deliberately drain slowly to exercise output backpressure.
                time.sleep(0.002)
            text = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", output)
            compact = re.sub(rb"\s+", b"", text)
            saw_blocks_column |= b"Blocks" in compact
            if not picked and not navigated and b"Detachfromablockwith" in compact and label.encode() in text:
                keys = b""
                if args.mode == "all":
                    sessions = api("session.list", {})["sessions"]
                    index = next(i for i, s in enumerate(sessions) if s["session_id"] == session)
                    keys += b"\x1b[B" * index + b"l"
                if args.multi_block_tab:
                    keys += b"j\rj"  # second tab, enter blocks column, second block
                if keys:
                    os.write(master, keys)
                navigated = True
                output.clear()
                continue
            ready = len(graphics.frames) > 0 if args.kitty_terminal else b"REX_PREVIEW_READY" in text
            updated = len(graphics.frames) > seed_frames if args.kitty_terminal else b"REX_PREVIEW_UPDATED" in text
            if preview_toggle == 1:
                if {frame[0] for frame in graphics.frames}.issubset(graphics.deleted):
                    os.write(master, b"p")
                    seed_frames = len(graphics.frames)
                    preview_toggle = 2
                continue
            if not picked and navigated and (not args.multi_block_tab or saw_blocks_column) and ((not preview_seeded and ready) or (preview_seeded and updated)):
                if not preview_seeded:
                    assert owner() == before_owner, "preview claimed block ownership"
                    api("com.superlogical.terminal.write", {"session_id": session, "block_id": block, "args": {"data": "printf '%s%s\\n' REX_PREVIEW_ UPDATED\r"}})
                    preview_seeded = True
                    seed_frames = len(graphics.frames)
                    output.clear()
                    continue
                if args.kitty_terminal and preview_toggle == 0:
                    os.write(master, b"p")
                    preview_toggle = 1
                    continue
                preview_seen = True
                assert api("com.superlogical.terminal.size", {"session_id": session, "block_id": block}) == before_size, "preview changed terminal size"
                if args.multi_block_tab:
                    assert saw_blocks_column, "split tab did not expose blocks column"
                os.write(master, b"\r")
                picked = True
                output.clear()
                continue
            if picked and not sent_input and b"\x1b[0;38;2;" in output:
                # The echoed command cannot itself contain the expected marker.
                os.write(master, b"printf '%s%s\\n' REX_ATTACH_ INPUT_OK\r")
                sent_input = True
            if sent_input and not sent_detach and b"REX_ATTACH_INPUT_OK" in text:
                os.write(master, b"\x02d")
                sent_detach = True
                output.clear()
                continue
            if sent_detach and not returned_to_browser and b"DetachfromablockwithCtrl-Bd" in compact:
                assert b"Blockinformation" in compact, "browser did not restore block information"
                assert (b"Blocks" in compact) == args.multi_block_tab, "browser did not restore block column"
                returned_to_browser = True
                os.write(master, b"\x03" if args.exit_key == "ctrl-c" else b"\x1b")
            if process.poll() is not None:
                break
        assert process.poll() == 0, f"interactive client failed or stalled: {process.poll()}, tail={output[-500:]!r}"
        assert sent_detach, "input marker never rendered"
        assert args.mode == "direct" or preview_seen, "selected block was not previewed"
        assert returned_to_browser, "detaching did not return to browser"
        while select.select([master], [], [], 0.05)[0]:
            graphics.feed(os.read(master, 4096))
        if args.kitty_terminal:
            assert len(graphics.frames) >= 2, "live image preview did not update"
            assert graphics.frames[0][3] != graphics.frames[1][3], "image pixels did not change"
            assert {frame[0] for frame in graphics.frames}.issubset(graphics.deleted), "image data was not deleted on detach/exit"
            pixels = graphics.frames[0][3]
            assert any(pixels[i + 1] > pixels[i] + 30 for i in range(0, len(pixels), 4)), "terminal text colors not rasterized"
            if args.image_out:
                graphics.save(args.image_out)
        assert termios.tcgetattr(slave) == before, "terminal attributes not restored"
        assert fcntl.fcntl(slave, fcntl.F_GETFL) == before_flags, "terminal descriptor flags changed"
        capture = subprocess.check_output(command + ["--capture"], timeout=15)
        assert b"REX_ATTACH_INPUT_OK" in capture, "block did not survive detach"
        print(f"PASS ({args.mode}, {args.exit_key}): selection, redraw, input, detach to browser, exit, terminal restoration, surviving block")
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()
        for fd in (master, slave):
            if fd is not None:
                os.close(fd)
        api("session.destroy", {"session_id": session})


if __name__ == "__main__":
    main()
