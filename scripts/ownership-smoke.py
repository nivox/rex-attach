#!/usr/bin/env python3
"""Verify automatic sizing with two attached clients in a disposable session."""
import argparse
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--socket', required=True)
    parser.add_argument('--binary', default='target/debug/rex-attach')
    args = parser.parse_args()
    rex = ['rex', '--autostart=false', '--server', args.socket, 'api', 'call']
    def api(method, payload):
        return json.loads(subprocess.check_output(rex+[method, json.dumps(payload)], timeout=15))
    created = api('session.create', {'label': 'ownership-'+uuid.uuid4().hex[:6], 'initial_windows': [
        {'layout': {'block': {'flavor': 'com.superlogical.terminal.shell'}}}
    ]})
    session = created['session_id']
    block = created['initial_windows'][0]['block_ids'][0]
    clients = []
    target = {'session_id': session, 'block_id': block}
    command = [args.binary, '--socket', args.socket, '--session', session, '--block', block]
    def dimensions():
        size = api('com.superlogical.terminal.size', target)
        return size['columns'], size['rows']
    def owner():
        return next(b for b in api('session.list_blocks', {'session_id': session})['blocks'] if b['block_id']==block).get('owner')
    def drain():
        for client in clients:
            while select.select([client['master']], [], [], .01)[0]:
                client['output'].extend(os.read(client['master'], 65536))
    def wait(test, message):
        deadline = time.monotonic()+12
        while time.monotonic()<deadline:
            drain()
            assert all(c['process'].poll() is None for c in clients), 'client exited unexpectedly'
            if test(): return
        raise AssertionError(message)
    def visible(client, text):
        return text in re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', client['output'])
    try:
        with tempfile.TemporaryDirectory(prefix='rex-attach-sizing-config-') as config:
            env = {**os.environ, 'TERM': 'xterm-256color', 'XDG_CONFIG_DIR': config}
            def spawn(cols, rows, extra=()):
                master, slave = pty.openpty()
                before = termios.tcgetattr(slave)
                flags = fcntl.fcntl(slave, fcntl.F_GETFL)
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
                def setup():
                    os.setsid()
                    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
                process = subprocess.Popen(command+['--preview','text']+list(extra), stdin=slave, stdout=slave, stderr=slave, preexec_fn=setup, env=env)
                client = dict(master=master, slave=slave, process=process, before=before, flags=flags, output=bytearray())
                clients.append(client)
                return client
            first = spawn(80, 24, ['--take'])  # compatibility flag still accepted
            wait(lambda:dimensions()==(80,24) and owner() is not None, 'first client sizes terminal')
            first_owner = owner()
            # Read-only captures must not take ownership or change the size.
            subprocess.run(command+['--capture'], env=env, check=True, stdout=subprocess.DEVNULL, timeout=15)
            assert dimensions()==(80,24) and owner()==first_owner
            second = spawn(120, 40)  # default behavior must override the first owner
            wait(lambda:dimensions()==(120,40) and owner()!=first_owner, 'second attachment takes sizing ownership')
            fcntl.ioctl(second['slave'], termios.TIOCSWINSZ, struct.pack('HHHH',30,100,0,0))
            os.kill(second['process'].pid, signal.SIGWINCH)
            wait(lambda:dimensions()==(100,30), 'local terminal resize updates shared block')
            os.write(second['master'], b'\x02d')
            wait(lambda:visible(second,b'Detach from a block with') and dimensions()==(80,24) and owner()==first_owner, 'detach returns ownership and original requested size')
            os.write(second['master'], b'\x1b')
            second['process'].wait(timeout=5)
            assert second['process'].returncode==0
            assert termios.tcgetattr(second['slave'])==second['before']
            assert fcntl.fcntl(second['slave'],fcntl.F_GETFL)==second['flags']
            clients.remove(second)
            os.close(second['master']);os.close(second['slave'])
            os.write(first['master'], b'\x02d')
            wait(lambda:visible(first,b'Detach from a block with') and owner() is None, 'last detach releases ownership')
            os.write(first['master'], b'\x1b')
            first['process'].wait(timeout=5)
            assert first['process'].returncode==0
            assert termios.tcgetattr(first['slave'])==first['before']
            assert fcntl.fcntl(first['slave'],fcntl.F_GETFL)==first['flags']
            print('PASS: two-client attach sizing, SIGWINCH resize, ownership/size restoration on detach, read-only capture, --take compatibility, terminal cleanup')
    finally:
        for client in clients:
            if client['process'].poll() is None: client['process'].kill();client['process'].wait()
            os.close(client['master']);os.close(client['slave'])
        api('session.destroy', {'session_id': session})


if __name__=='__main__':
    main()
