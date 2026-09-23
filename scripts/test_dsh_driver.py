"""Exercise the actual terminal/ACP boundary with a deterministic peer.

Run with HERDR_TEST_BIN=target/debug/herdr python3 -m unittest scripts.test_dsh_driver.
No installed DSH, credentials, network, or running Herdr server is used.
"""
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import time
import unittest

FAKE = r'''import json, os, sys
trace = open(os.environ['DSH_TEST_TRACE'], 'a', buffering=1)
waiting = None
model = '["a","m"]'
effort = 'low'
def config_options():
    return [dict(id='model',currentValue=model,options=[dict(value='["a","m"]'),dict(value='["b","m"]')]),
            dict(id='reasoning_effort',currentValue=effort,options=[dict(value='low' if model=='["a","m"]' else 'high')])]
for line in sys.stdin:
    frame = json.loads(line)
    trace.write(json.dumps(frame)+'\n')
    method, ident = frame.get('method'), frame.get('id')
    def send(value):
        print(json.dumps(dict(jsonrpc='2.0', **value)), flush=True)
    def result(value):
        send(dict(id=ident,result=value))
    def answer(text, stop='end_turn', request=None):
        send(dict(method='session/update',params=dict(sessionId='fixture',update=dict(sessionUpdate='agent_message_chunk',content=dict(type='text',text=text)))))
        send(dict(id=request if request is not None else ident,result=dict(stopReason=stop)))
    if method == 'initialize':
        result(dict(protocolVersion=1, agentCapabilities=dict(sessionCapabilities=dict(close={},resume={}))))
    elif method in ('session/new','session/resume'):
        result(dict(sessionId='fixture',configOptions=config_options()))
    elif method == 'session/set_config_option':
        if frame['params']['configId']=='model':
            model=frame['params']['value']
            effort='low' if model=='["a","m"]' else 'high'
        else:
            effort=frame['params']['value']
        result(dict(configOptions=config_options()))
    elif method == 'session/prompt':
        text=frame['params']['prompt'][0]['text']
        if text == 'lost':
            sys.exit(1)
        elif text == 'ask':
            waiting=ident
            send(dict(id='permission-'+str(ident),method='session/request_permission',params=dict(sessionId='fixture',toolCall=dict(title='fixture permission'),options=[dict(optionId='yes',kind='allow_once'),dict(optionId='no',kind='reject_once')])) )
        elif text == 'delay':
            waiting=ident
        else:
            answer('reply: '+text)
    elif method == 'session/cancel':
        answer('cancelled',stop='cancelled',request=waiting)
    elif method == 'session/close':
        result({})
    elif str(ident).startswith('permission-'):
        answer('permission '+frame['result']['outcome'].get('optionId','cancelled'),request=waiting)
'''

@unittest.skipUnless(os.name == 'posix' and os.environ.get('HERDR_TEST_BIN'), 'requires POSIX PTY and HERDR_TEST_BIN')
class DriverTests(unittest.TestCase):
    def setUp(self):
        import pty
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.script = self.root/'fake-dsh'
        self.script.write_text('#!'+sys.executable+'\n'+FAKE)
        self.script.chmod(0o700)
        self.journal = self.root/'evidence.json'
        self.trace = self.root/'trace.jsonl'
        self.env = {k:v for k,v in os.environ.items() if not k.startswith('HERDR_')}
        self.env.update(DSH_TEST_TRACE=str(self.trace),XDG_STATE_HOME=str(self.root/'state'))
        self.binary = str(Path(os.environ['HERDR_TEST_BIN']).resolve())
        self.master, self.slave = pty.openpty()
        self.addCleanup(os.close,self.master)
        self.addCleanup(os.close,self.slave)
        self.process = None
        self.output = b''

    def test_catalog_has_no_terminal_prompt_or_herdr_binding(self):
        proc = subprocess.run([self.binary,'agent','dsh','--catalog','--dsh-bin',str(self.script),
            '--model','["b","m"]','--effort','high'], capture_output=True,text=True,timeout=10,
            env={**self.env,'HERDR_PANE_ID':'must-not-contact-herdr'},cwd=self.root)
        self.assertEqual(proc.returncode,0,proc.stderr)
        value=json.loads(proc.stdout)
        self.assertEqual(value['config_options'][0]['currentValue'],'["b","m"]')
        self.assertEqual(value['config_options'][1]['currentValue'],'high')
        methods=[json.loads(line)['method'] for line in self.trace.read_text().splitlines()]
        self.assertEqual(methods,['initialize','session/new','session/set_config_option','session/set_config_option','session/close'])
        self.assertFalse(self.journal.exists())
        self.assertFalse((self.root/'state').exists())

    def test_invalid_catalog_selection_closes_empty_session(self):
        for extra in (['--model','m'],['--model','missing'],['--effort','high']):
            self.trace.write_text('')
            proc = subprocess.run([self.binary,'agent','dsh','--catalog','--dsh-bin',str(self.script),*extra],
                capture_output=True,text=True,timeout=10,env=self.env,cwd=self.root)
            self.assertNotEqual(proc.returncode,0)
            methods=[json.loads(line)['method'] for line in self.trace.read_text().splitlines()]
            self.assertEqual(methods,['initialize','session/new','session/close'])

    def start(self, extra=()):
        self.process = subprocess.Popen([self.binary,'agent','dsh','--dsh-bin',str(self.script),'--journal',str(self.journal),*extra],
            stdin=self.slave,stdout=self.slave,stderr=self.slave,env=self.env,cwd=self.root)
        self.addCleanup(self.cleanup_process)
        self.until(lambda: self.journal.exists())

    def cleanup_process(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:self.process.kill();self.process.wait()

    def drain(self):
        if select.select([self.master],[],[],.02)[0]:
            try:self.output += os.read(self.master,65536)
            except OSError:pass

    def until(self, predicate, timeout=10):
        deadline=time.monotonic()+timeout
        while time.monotonic()<deadline:
            self.drain()
            if predicate():return
        self.fail('timeout: '+self.output.decode(errors='replace')[-2000:])

    def state(self):
        try:return json.loads(self.journal.read_text())
        except (OSError,ValueError):return {}

    def calls(self):
        return [json.loads(line) for line in self.trace.read_text().splitlines()]

    def send(self,text):
        os.write(self.master,b'\x1b[200~'+text.encode()+b'\x1b[201~\r')

    def test_multiline_prompt_and_settlement_are_not_terminal_echo(self):
        self.start()
        self.until(lambda:b'dsh> ' in self.output)
        self.send('hello\nworld')
        self.until(lambda:self.state().get('last_assistant_text')=='reply: hello\nworld')
        self.assertEqual(self.state()['last_user_text'],'hello\nworld')
        self.assertEqual(self.state()['stop_reason'],'end_turn')
        prompts=[v for v in self.calls() if v.get('method')=='session/prompt']
        self.assertEqual(len(prompts),1)
        self.assertEqual(prompts[0]['params']['prompt'][0]['text'],'hello\nworld')
        self.send('/exit')
        self.until(lambda:self.process.poll() is not None)
        self.assertEqual(self.process.returncode,0,self.output)
        self.assertEqual(self.state()['lifecycle'],'exited')
        self.assertEqual(len([v for v in self.calls() if v.get('method')=='session/close']),1)

    def test_permissions_require_explicit_input(self):
        self.start();self.until(lambda:b'dsh> ' in self.output)
        self.send('ask')
        self.until(lambda:self.state().get('lifecycle')=='blocked')
        self.assertFalse(any(str(v.get('id')).startswith('permission-') for v in self.calls()))
        os.write(self.master,b'2')
        self.until(lambda:self.state().get('last_assistant_text')=='permission no')
        self.assertEqual(self.state()['lifecycle'],'idle')

    def test_cancel_does_not_resend_work(self):
        self.start();self.until(lambda:b'dsh> ' in self.output)
        self.send('delay');self.until(lambda:self.state().get('lifecycle')=='working')
        os.write(self.master,b'\x03')
        self.until(lambda:self.state().get('stop_reason')=='cancelled')
        self.assertEqual(len([v for v in self.calls() if v.get('method')=='session/prompt']),1)

    def test_cancelled_permission_cannot_receive_a_later_turns_choice(self):
        self.start();self.until(lambda:b'dsh> ' in self.output)
        self.send('ask');self.until(lambda:self.state().get('lifecycle')=='blocked')
        os.write(self.master,b'\x03')
        self.until(lambda:self.state().get('stop_reason')=='cancelled')
        self.send('ask');self.until(lambda:self.state().get('lifecycle')=='blocked')
        os.write(self.master,b'2')
        self.until(lambda:self.state().get('last_assistant_text')=='permission no')
        prompts=[v for v in self.calls() if v.get('method')=='session/prompt']
        choices=[v for v in self.calls() if str(v.get('id')).startswith('permission-')]
        self.assertEqual([v['id'] for v in choices],['permission-'+str(prompts[-1]['id'])])

    def test_lost_response_does_not_claim_acceptance_or_retry(self):
        self.start();self.until(lambda:b'dsh> ' in self.output)
        self.send('lost');self.until(lambda:self.process.poll() is not None)
        self.assertNotEqual(self.process.returncode,0)
        self.assertIsNone(self.state().get('last_user_text'))
        self.assertEqual(len([v for v in self.calls() if v.get('method')=='session/prompt']),1)

    def test_resume_does_not_send_initializer(self):
        self.start(('--resume','fixture'))
        self.until(lambda:b'dsh> ' in self.output)
        self.assertTrue(any(v.get('method')=='session/resume' for v in self.calls()))
        self.assertFalse(any(v.get('method') in ('session/new','session/prompt') for v in self.calls()))

if __name__ == '__main__':unittest.main()
