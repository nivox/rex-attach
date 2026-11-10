#!/usr/bin/env python3
"""Verify snapshot/live Kitty images, previews and cleanup in disposable Rex sessions."""
from smoke_support import Screen, api_client, open_terminal, controlling_terminal, set_size, wait_until
import argparse
import base64
import os
import re
import select
import subprocess
import sys
import tempfile
import termios
import time
import uuid
import zlib


class KittyReceiver:
    """Small receiver that validates pixels and physical placements, not a GUI."""
    def __init__(self):
        self.pending = bytearray()
        self.screen = Screen(120, 40)
        self.images = {}
        self.placements = {}
        self.frames = []
        self.deleted = set()
        self.upload = None
        self.cursor = (0, 0)

    def graphics(self, command):
        header, _, data = command.partition(b";")
        fields = dict(part.split(b"=", 1) for part in header.split(b",") if b"=" in part)
        action = fields.get(b"a")
        if action == b"q":
            return b"\x1b_Gi=" + fields[b"i"] + b";OK\x1b\\"
        if action in (b"t", b"T"):
            self.upload = (fields, bytearray())
        if data and self.upload is not None:
            metadata, pixels = self.upload
            pixels.extend(base64.b64decode(data, validate=True))
            if fields.get(b"m", b"0") == b"0":
                if metadata.get(b"o") == b"z": pixels = zlib.decompress(pixels)
                width, height = int(metadata[b"s"]), int(metadata[b"v"])
                assert len(pixels) == width * height * 4, "invalid transmitted RGBA"
                assert len(pixels) <= 64 * 1024 * 1024
                image = (int(metadata[b"i"]), width, height, bytes(pixels))
                self.images[image[0]] = image
                self.frames.append(image)
                self.upload = None
        if action == b"p":
            image, placement = int(fields[b"i"]), int(fields[b"p"])
            assert image in self.images, "placement refers to missing image"
            self.placements[(image, placement)] = (self.screen.x, self.screen.y, fields)
        if action == b"d":
            assert fields.get(b"d") in (b"i", b"I"), "global image deletion"
            image = int(fields[b"i"])
            self.placements = {key:value for key,value in self.placements.items() if key[0] != image}
            if fields[b"d"] == b"I":
                self.images.pop(image, None)
                self.deleted.add(image)
        return b""

    def feed(self, chunk):
        self.pending.extend(chunk)
        replies = bytearray()
        while self.pending:
            if self.pending.startswith(b"\x1b_G"):
                end = self.pending.find(b"\x1b\\", 3)
                if end < 0: break
                replies.extend(self.graphics(bytes(self.pending[3:end])))
                del self.pending[:end + 2]
            elif self.pending.startswith(b"\x1b["):
                match = re.match(rb"\x1b\[[0-?]*[ -/]*[@-~]", self.pending)
                if match is None: break
                command = match.group()
                if command == b"\x1b[16t": replies.extend(b"\x1b[6;20;10t")
                if command == b"\x1b[5n": replies.extend(b"\x1b[0n")
                self.screen.feed(command)
                del self.pending[:len(command)]
            elif self.pending[0] == 27:
                if len(self.pending) < 3: break
                if self.pending[1] == ord("7"): self.cursor = (self.screen.x, self.screen.y)
                if self.pending[1] == ord("8"): self.screen.x, self.screen.y = self.cursor
                del self.pending[:2]
            else:
                self.screen.feed(bytes(self.pending[:1]))
                del self.pending[:1]
        return replies

    def visible(self, text): return text in "\n".join(self.screen.lines())
    def color(self, pixel):
        return any(frame[3] and all(frame[3][at:at+4] == pixel for at in range(0,len(frame[3]),4))
                   for key, frame in self.images.items() if any(p[0] == key for p in self.placements))


def run(args, backend):
    api = api_client(args.socket)
    with tempfile.TemporaryDirectory(prefix="rex-attach-images-") as directory:
        ready = directory + "/ready"
        program = r'''
import base64,os,sys,tty,zlib,struct
tty.setraw(0)
def emit(data): os.write(1,data)
def raw(pixel, fmt=32):
    data=base64.b64encode(bytes(pixel)*4)
    emit(b"\x1b[2;3H\x1b_Ga=T,f="+str(fmt).encode()+b",s=2,v=2,i=42,p=1,c=3,r=2,C=1,q=2;"+data+b"\x1b\\")
raw([255,0,0,255])
emit(b"\x1b[8;1HIMAGE_READY")
open(sys.argv[1],"w").close()
while True:
    data=os.read(0,4096)
    if not data: break
    with open(sys.argv[1]+".input","ab") as log: log.write(data)
    for command in data:
        if command == ord("G"):
            def chunk(kind, data):
                return struct.pack(">I",len(data))+kind+data+struct.pack(">I",zlib.crc32(kind+data))
            png=base64.b64encode(b"\x89PNG\r\n\x1a\n"+chunk(b"IHDR",struct.pack(">IIBBBBB",1,1,8,6,0,0,0))+chunk(b"IDAT",zlib.compress(b"\0\xff\0\0\xff"))+chunk(b"IEND",b""))
            emit(b"\x1b[2;3H\x1b_Ga=T,f=100,i=42,p=1,c=3,r=2,C=1,q=2;"+png+b"\x1b\\")
            emit(b"\x1b[8;1HPNG_READY")
        if command == ord("B"):
            data=base64.b64encode(bytes([0,0,255])*4)
            emit(b"\x1b[2;3H\x1b_Ga=T,f=24,s=2,v=2,i=42,p=1,c=3,r=2,C=1,q=2,m=1;"+data[:8]+b"\x1b\\")
            emit(b"\x1b_Gm=0;"+data[8:]+b"\x1b\\")
        if command == ord("C"):
            data=base64.b64encode(zlib.compress(bytes([0,255,0,255])*4))
            emit(b"\x1b[2;3H\x1b_Ga=T,f=32,o=z,s=2,v=2,i=42,p=1,c=3,r=2,C=1,q=2;"+data+b"\x1b\\")
        if command == ord("D"): emit(b"\x1b_Ga=d,d=I,i=42,q=2\x1b\\\x1b[8;1HDELETED_READY")
        if command == ord("F"):
            path=sys.argv[1]+".rgba"
            with open(path,"wb") as image: image.write(bytes([255,255,0,255])*4)
            encoded=base64.b64encode(path.encode())
            emit(b"\x1b[2;3H\x1b_Ga=T,t=f,f=32,s=2,v=2,i=42,p=1,c=3,r=2,C=1,q=2;"+encoded+b"\x1b\\\x1b[8;1HFILE_READY")
        if command == ord("S"):
            emit(b"\x1b[40;1H")
            for n in range(50): emit(f"IMAGE_HISTORY_{n:03}\r\n".encode())
        if command == ord("T"): emit(b"TYPED_READY\r\n")
'''
        session = api("session.create", {"label":"graphics-"+uuid.uuid4().hex[:6],
            "initial_windows":[{"window_label":"pictures","layout":{"block":{
                "flavor":"com.superlogical.terminal.shell","label":"image-recorder",
                "options":{"command":[sys.executable,"-u","-c",program,ready]}}}}]})
        sid = session["session_id"]
        block = session["initial_windows"][0]["block_ids"][0]
        process = None
        master, slave, before, _ = open_terminal()
        receiver = KittyReceiver()
        try:
            deadline = time.monotonic() + 10
            while not os.path.exists(ready):
                assert time.monotonic() < deadline, "fixture failed to start"
                time.sleep(.02)
            original_size = api("com.superlogical.terminal.size", {"session_id":sid,"block_id":block})
            process = subprocess.Popen([args.binary,"--socket",args.socket,"--session",sid,
                "--preview","kitty","--renderer",backend],stdin=slave,stdout=slave,stderr=slave,
                preexec_fn=controlling_terminal,env={**os.environ,"TERM":"xterm-256color"})

            def pump():
                while select.select([master],[],[],.02)[0]:
                    reply = receiver.feed(os.read(master,65536))
                    if reply: os.write(master,reply)
                assert process.poll() is None, "\n".join(receiver.screen.lines())
            def wait(predicate,message):
                def diagnostic():
                    log = open(ready+".input","rb").read() if os.path.exists(ready+".input") else b""
                    remote = api("com.superlogical.terminal.format",{"session_id":sid,"block_id":block,"args":{"format":"text"}})
                    return message+"\ninput="+repr(log)+"\nremote="+repr(remote)+"\n"+"\n".join(receiver.screen.lines())
                wait_until(predicate,diagnostic,pump)
            def send(data): os.write(master,data)

            wait(lambda:receiver.visible("Tabs") and receiver.frames,"browser image preview")
            assert any(frame[3][n:n+4] == b"\xff\0\0\xff" for frame in receiver.frames for n in range(0,len(frame[3]),4)), "preview omitted embedded image"
            assert api("com.superlogical.terminal.size", {"session_id":sid,"block_id":block}) == original_size, "preview resized or took ownership"
            send(b"\r")
            wait(lambda:receiver.color(b"\xff\0\0\xff"),"snapshot image displayed")
            image_ids={key[0] for key in receiver.placements}
            count=len(receiver.frames)
            deadline=time.monotonic()+.15
            while time.monotonic()<deadline: pump()
            assert len(receiver.frames)==count, "unchanged image retransmitted"
            send(b"G")
            wait(lambda:receiver.visible("PNG_READY") and len(receiver.frames)>count,"live PNG update")
            send(b"C")
            wait(lambda:receiver.color(b"\0\xff\0\xff"),"live compressed RGBA image")
            send(b"B")
            wait(lambda:receiver.color(b"\0\0\xff\xff"),"live chunked RGB image")
            assert image_ids.issubset(receiver.deleted),"replaced image leaked"
            if args.probe_file_media:
                send(b"F")
                wait(lambda:receiver.visible("FILE_READY"),"file-medium probe")
                deadline=time.monotonic()+.5
                while time.monotonic()<deadline: pump()
                live_file=receiver.color(b"\xff\xff\0\xff")
                send(b"\x02d")
                wait(lambda:receiver.visible("Detach from a block with"),"file-medium probe detach")
                send(b"\r")
                wait(lambda:bool(receiver.placements),"file-medium probe reattach")
                snapshot_file=receiver.color(b"\xff\xff\0\xff")
                assert live_file and snapshot_file, "Rex did not normalize server file images for viewers"
                print(f"FILE MEDIA ({backend}): live inline pixels={live_file}, server snapshot pixels={snapshot_file}")
                send(b"B")
                wait(lambda:receiver.color(b"\0\0\xff\xff"),"inline image after file-medium probe")
            send(b"\x02?")
            wait(lambda:receiver.visible("Shortcut help"),"help opens")
            assert not receiver.placements,"images cover help"
            send(b"q")
            wait(lambda:receiver.color(b"\0\0\xff\xff"),"image restored after help")
            send(b"S")
            wait(lambda:receiver.visible("IMAGE_HISTORY_049") and not receiver.placements,"scroll removes off-screen placements")
            send(b"\x1b[<64;5;5M"*20)
            wait(lambda:bool(receiver.placements),"local history restores scrolled image")
            send(b"T")
            wait(lambda:receiver.visible("TYPED_READY") and not receiver.placements,"typing returns to live screen")
            send(b"B")
            wait(lambda:receiver.color(b"\0\0\xff\xff"),"new image after scrolling")
            set_size(slave,100,30)
            wait(lambda:api("com.superlogical.terminal.size", {"session_id":sid,"block_id":block})["columns"]==100,"resize")
            send(b"D")
            wait(lambda:receiver.visible("DELETED_READY") and not receiver.placements,"image deletion")
            send(b"\x02d")
            wait(lambda:receiver.visible("Detach from a block with"),"detach")
            send(b"q")
            process.wait(timeout=5)
            while select.select([master],[],[],.02)[0]: receiver.feed(os.read(master,65536))
            assert process.returncode==0
            assert not receiver.images,"image data leaked on exit"
            assert termios.tcgetattr(slave)==before
            print(f"PASS ({backend}): embedded preview, snapshot/live PNG/RGB, chunking, deduplication, scrolling, help, resize, deletion and cleanup")
        finally:
            if process is not None and process.poll() is None: process.kill();process.wait()
            os.close(master);os.close(slave)
            api("session.destroy",{"session_id":sid})


if __name__ == "__main__":
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket",required=True)
    parser.add_argument("--binary",default="target/debug/rex-attach")
    parser.add_argument("--probe-file-media",action="store_true")
    args=parser.parse_args()
    for backend in ("ansi","ratatui"): run(args,backend)
