#!/usr/bin/env python3
"""Exercise browser CRUD and terminal shortcuts in disposable Rex sessions."""
import argparse
import fcntl
import json
import os
import pty
import select
import struct
import subprocess
import tempfile
import termios
import time
import uuid
import importlib.util
_spec = importlib.util.spec_from_file_location("activity_smoke", os.path.join(os.path.dirname(__file__), "activity-smoke.py"))
_module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_module)
Screen = _module.Screen


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--socket', required=True)
    parser.add_argument('--binary', default='target/debug/rex-attach')
    args = parser.parse_args()
    rex = ['rex', '--autostart=false', '--server', args.socket, 'api', 'call']
    def api(method, payload):
        return json.loads(subprocess.check_output(rex+[method, json.dumps(payload)], timeout=15))
    def layout(label):
        return {'block': {'flavor': 'com.superlogical.terminal.shell', 'label': label}}
    label = 'shortcuts-'+uuid.uuid4().hex[:6]
    created = api('session.create', {'label': label, 'initial_windows': [{'window_label': 'first', 'layout': layout('alpha')}]})
    session = created['session_id']
    extra_sessions = []
    master, slave = pty.openpty()
    before = termios.tcgetattr(slave)
    flags = fcntl.fcntl(slave, fcntl.F_GETFL)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 0, 0))
    process = None
    try:
        with tempfile.TemporaryDirectory(prefix='rex-attach-config-') as config:
            os.mkdir(config+'/rex-attach')
            # Verify that the program actually loads overrides from XDG_CONFIG_DIR.
            with open(config+'/rex-attach/config.json', 'w') as f:
                json.dump({'shortcuts': {'browser': {'z': 'client.rename'}, 'terminal': {'ctrl+b>q': 'client.detach'}}}, f)
            def setup():
                os.setsid()
                fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            process = subprocess.Popen([args.binary, '--socket', args.socket, '--session', session, '--preview', 'text'], stdin=slave, stdout=slave, stderr=slave, preexec_fn=setup, env={**os.environ, 'TERM': 'xterm-256color', 'XDG_CONFIG_DIR': config})
            screen = Screen()
            def drain():
                while select.select([master], [], [], .02)[0]:
                    screen.feed(os.read(master, 65536))
                assert process.poll() is None, '\n'.join(screen.lines())
            def wait(test, message):
                deadline = time.monotonic()+12
                while time.monotonic() < deadline:
                    drain()
                    if test(): return
                raise AssertionError(message+'\n'+'\n'.join(screen.lines()))
            def visible(text): return text in '\n'.join(screen.lines())
            def send(data): os.write(master, data)
            def view(): return api('session.view', {'session_id': session})
            def children(window): return [b for l in window['layers'] for b in l['blocks']]
            def total(): return sum(len(children(w)) for w in view().get('windows', []))
            def edit(key, title, text):
                send(key);wait(lambda:visible(title),title)
                send(b'\x15'+text.encode()+b'\r')
                wait(lambda:not visible(title),'dialog closes')
            wait(lambda:visible('Tabs') and visible('alpha'),'initial browser')
            send(b'?')
            wait(lambda:visible('Shortcut help') and visible('Browser') and visible('client.rename'), 'browser shortcut help')
            assert visible('z')  # configured override is listed
            send(b'xcr')  # management keys cannot act while help is open
            drain()
            assert visible('Shortcut help') and not visible('Close tab?')
            assert len(view()['windows'])==1
            send(b'\x1b')
            wait(lambda:visible('Detach from a block with') and not visible('Shortcut help'),'dismiss browser help')
            edit(b'z','Rename tab','renamed-tab')
            assert view()['windows'][0]['label']=='renamed-tab'
            edit(b'c','Create tab','second')
            wait(lambda:visible('second'),'created tab selection')
            assert len(view()['windows'])==2
            second = view()['windows'][1]['window_id']
            send(b'\r')
            # A marker ensures attachment and input are ready, and remains in the
            # remote terminal so navigation can prove which stream is displayed.
            send(b"printf 'SECOND_MARKER\\n'\r")
            wait(lambda:visible('SECOND_MARKER') and not visible('Detach from a block with'),'attach second')
            send(b"sleep 1; printf 'LIVE_UNDER_HELP\\n'\r")
            send(b'\x02?')
            wait(lambda:visible('Shortcut help') and visible('Browser'), 'terminal help includes browser section')
            send(b'G')
            wait(lambda:visible('Terminal') and visible('ctrl+b>q'), 'terminal help lists override')
            send(b'xc\x02c')  # plain keys and management shortcuts stay local
            deadline=time.monotonic()+1.2
            while time.monotonic()<deadline: drain()
            assert len(view()['windows'])==2 and visible('Shortcut help')
            send(b'q')
            wait(lambda:any(line.strip()=='LIVE_UNDER_HELP' for line in screen.lines()) and not visible('Shortcut help'),'live terminal restored after help')
            send(b'\x02\x1b[39;6u')  # requested Ctrl+B, Ctrl+Shift+'
            wait(lambda:total()==3,'horizontal screen split')
            assert view()['windows'][1]['layers'][0]['layout']['split']['direction']=='vertical'
            send(b"printf 'LOWER_MARKER\\n'\r")
            wait(lambda:visible('LOWER_MARKER'),'attach new horizontal split')
            send(b'\x02\x1b[53;6u')  # requested Ctrl+B, Ctrl+Shift+5
            wait(lambda:total()==4,'vertical screen split')
            send(b"printf 'RIGHT_MARKER\\n'\r")
            wait(lambda:visible('RIGHT_MARKER'),'attach new vertical split')
            send(b'\x02o')
            wait(lambda:visible('SECOND_MARKER') and not visible('RIGHT_MARKER'),'next pane wraps')
            send(b'\x02c')
            wait(lambda:len(view()['windows'])==3,'new terminal tab')
            send(b"printf 'THIRD_MARKER\\n'\r")
            wait(lambda:visible('THIRD_MARKER'),'attach new tab')
            send(b'\x02p')
            wait(lambda:visible('SECOND_MARKER'),'previous tab restores focused block')
            send(b'\x02n')
            wait(lambda:visible('THIRD_MARKER'),'next tab')
            send(b'\x02n')
            send(b"printf 'FIRST_MARKER\\n'\r")
            wait(lambda:visible('FIRST_MARKER'),'next tab wraps to first')
            send(b'\x02q')  # overridden detach binding
            wait(lambda:visible('Detach from a block with') and visible('Tabs'),'custom detach returns browser')
            send(b'jlj')  # second window, blocks column, second block
            wait(lambda:visible('Blocks'),'block column')
            edit(b'r','Rename block','renamed-block')
            assert any(b['label']=='renamed-block' for b in children(view()['windows'][1]))
            count=total()
            edit(b'c','Create block','browser-split')
            assert total()==count+1
            wait(lambda:visible('browser-split'),'new block selected')
            send(b'x');wait(lambda:visible('Close block?'),'close block prompt')
            send(b'\r');wait(lambda:not visible('Close block?'),'default close cancels')
            assert total()==count+1
            send(b'x');wait(lambda:visible('Close block?'),'close block again');send(b'y')
            wait(lambda:total()==count and not visible('Close block?'),'confirmed close block')
            send(b'h');wait(lambda:visible('Tabs'),'tabs focus')
            send(b'x');wait(lambda:visible('Close tab?'),'close tab prompt');send(b'n')
            wait(lambda:not visible('Close tab?'),'tab close cancelled')
            assert len(view()['windows'])==3
            send(b'x');wait(lambda:visible('Close tab?'),'close tab again');send(b'y')
            wait(lambda:len(view()['windows'])==2 and not visible('Close tab?'),'confirmed close tab')
            assert not any(w['window_id']==second for w in view()['windows'])
            send(b'h');wait(lambda:visible('Sessions'),'sessions focus')
            edit(b'r','Rename session',label+'-renamed')
            assert view()['label']==label+'-renamed'
            edit(b'c','Create session',label+'-new')
            listed=api('session.list',{})['sessions']
            extra_sessions.extend(s['session_id'] for s in listed if s.get('label')==label+'-new')
            assert len(extra_sessions)==1
            assert len(api('session.view',{'session_id':extra_sessions[0]})['windows'])==1
            send(b'x');wait(lambda:visible('Close session?'),'close session prompt');send(b'\x1b')
            wait(lambda:not visible('Close session?'),'session close cancelled')
            assert view()['session_id']==session
            send(b'x');wait(lambda:visible('Close session?'),'close session again');send(b'y')
            wait(lambda: not any(s['session_id']==session for s in api('session.list',{})['sessions']), 'confirmed close session')
            wait(lambda:not visible('Close session?'),'destroy completed')
            send(b'q');process.wait(timeout=5)
            assert process.returncode==0
            assert termios.tcgetattr(slave)==before
            assert fcntl.fcntl(slave,fcntl.F_GETFL)==flags
            print('PASS: configured browser/terminal help and live restoration; config overrides; session/tab/block CRUD and confirmations; terminal split/new/next/previous/wrap/detach; terminal cleanup')
    finally:
        if process is not None and process.poll() is None: process.kill();process.wait()
        os.close(master);os.close(slave)
        existing={s['session_id'] for s in api('session.list',{})['sessions']}
        extra_sessions.extend(s['session_id'] for s in api('session.list',{})['sessions'] if s.get('label')==label+'-new')
        for sid in set([session]+extra_sessions):
            if sid in existing: api('session.destroy',{'session_id':sid})


if __name__=='__main__':
    main()
