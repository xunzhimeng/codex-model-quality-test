"""公开SDK协议集成测试，所有宿主回调为内存桩，不访问真实上游或凭据"""
import json
import os
import pathlib
import queue
import struct
import subprocess
import threading
import time
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]

def dump(value):
    return json.dumps(value, ensure_ascii=False, separators=(',', ':')).encode()

def events(answer='21'):
    metadata = dump({'facts': [
        {'type':'text_delta','index':0,'text':answer},
        {'type':'completed','id':'fixture','model':'fixture-model','reason':'stop'}
    ]})
    event = b'GPE1' + struct.pack('>II', len(metadata), 0) + metadata
    return b'HME1' + struct.pack('>II', 1, len(event)) + event

class Host:
    def __init__(self):
        self.process = subprocess.Popen([str(ROOT/'target/debug'/('model-quality-test.exe' if os.name=='nt' else 'model-quality-test'))], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.messages = queue.Queue()
        self.states = {}
        self.calls = []
        self.streams = {}
        self.http = []
        self.ticket_second = 'ticket-new'
        self.bad_stream = False
        self.model_count = 0
        self.write({'type':'hello','handshake':{'protocol_version':1,'artifact_sha256':'0'*64,'plugin_id':'xunzhimeng.model-quality-test','instance_id':'fixture','generation':1,'incarnation':'fixture-1','configuration':{},'permissions':['models','accounts','network'],'contributes':{'management':{'id':'xunzhimeng.model-quality-test.management','version':1,'stages':['management']}}}})
        threading.Thread(target=self.reader, daemon=True).start()
        message,_ = self.messages.get(timeout=5)
        assert message['type'] == 'ready', message
        self.next_id = 11
    def write(self, metadata, payload=b''):
        data=dump(metadata)
        self.process.stdin.write(struct.pack('>IQ',len(data),len(payload))+data+payload)
        self.process.stdin.flush()
    def reader(self):
        try:
            while True:
                header=self.process.stdout.read(12)
                if not header: return
                m,b=struct.unpack('>IQ',header)
                message=json.loads(self.process.stdout.read(m)); payload=self.process.stdout.read(b)
                self.messages.put((message,payload))
        except Exception as error: self.messages.put(({'type':'reader_error','error':str(error)},b''))
    def callback(self, msg, body):
        method=msg['method']; params=msg['params']; self.calls.append(method)
        result={}; payload=b''
        account={'account_id':'fixture-account','provider_id':'openai','credential_revision':1,'name':'测试账号','email':None,'upstream_user_id':None,'upstream_account_id':'fixture-upstream','plan_type':'plus','authentication_kind':'oauth','enabled':True,'credential_state':'ready','has_refresh_token':False,'access_token_expires_at_ms':None,'next_refresh_at_ms':None}
        if method.startswith('host.auth.'):
            assert params=={}; request=json.loads(body)
            if method=='host.auth.list': payload=dump({'accounts':[account],'next_cursor':None})
            elif method=='host.auth.get_runtime': payload=dump(account)
            elif method=='host.auth.get': payload=dump({'account_id':'fixture-account','provider_id':'openai','credential_revision':1,'facts':{'name':'测试账号','authentication_kind':'oauth','material':{'access_token':'fixture-token-not-real'},'email':None,'upstream_user_id':None,'upstream_account_id':'fixture-upstream','plan_type':'plus','has_refresh_token':False,'access_token_expires_at_ms':None,'next_refresh_at_ms':None}})
        elif method=='host.keys.list':
            assert not body; assert params['limit']==100
            result={'keys':[{'id':'fixture-key','name':'测试Key','enabled':True}],'next_cursor':None}
        elif method=='host.models.list':
            assert not body; assert params['client_key_id']=='fixture-key'
            result={'models':['fixture-model']}
        elif method=='host.state.get':
            assert not body; assert params['namespace']=='quality'
            result={'record':self.states.get(params['key'])}
        elif method=='host.state.put':
            assert not body; key=params['key']; old=self.states.get(key)
            assert params['expected_version']==(old['version'] if old else None)
            assert len(dump(params['value']))<262144
            version=(old['version']+1) if old else 1
            self.states[key]={'value':params['value'],'version':version,'schema_version':1}; result={'version':version}
        elif method=='host.model.execute':
            assert params['client_key_id']=='fixture-key' and params['account_id']=='fixture-account'
            assert json.loads(body)['reasoning']['effort']=='medium'
            assert self.states['history']['value']['records'][-1]['status']=='running'
            self.model_count+=1
            result={'request_id':'fixture-request','events':1}; payload=events()
        elif method=='host.http.do_stream':
            assert params['url']=='https://chatgpt.com/backend-api/codex/responses'
            assert json.loads(body)['stream'] is True
            headers=dict(params['headers']); self.http.append(headers)
            number=len(self.http); stream=f'stream-{number}'
            ticket='ticket-first' if number%2 else self.ticket_second
            response_headers=[['x-codex-turn-state',ticket],['set-cookie','__oailb=route-fixture; Secure; HttpOnly'],['set-cookie','other=not-forwarded; Secure']]
            self.streams[stream]=0
            result={'status':200,'headers':response_headers,'stream':stream}
        elif method=='host.http.stream_read':
            stream=params['stream']; number=self.streams[stream]; self.streams[stream]+=1
            if number==0:
                payload=(b'data: {"type":"response.failed"}\n\n' if self.bad_stream else b'data: {"type":"response.completed","response":{"status":"completed","model":"fixture-model"}}\n\n')
                result={'eof':False}
            else: result={'eof':True}
        elif method=='host.http.stream_close': result={}
        else: raise AssertionError('unexpected callback '+method)
        self.write({'type':'result','id':msg['id'],'result':result},payload)
    def call(self, method, path=None, body=None, stage='management'):
        id=self.next_id; self.next_id+=2
        params={} if path is None else {'method':method,'path':path,'query':'','content_type':'application/json' if body is not None else None}
        self.write({'type':'call','id':id,'method':method if path is None else 'management.handle','context':{'call_id':id,'instance_id':'fixture','generation':1,'incarnation':'fixture-1','stage':stage,'timeout_ms':120000,'resource_scope_id':str(id)},'params':params},dump(body) if body is not None else b'')
        while True:
            msg,payload=self.messages.get(timeout=10)
            if msg['type']=='callback': self.callback(msg,payload); continue
            assert msg['type']=='result', msg
            assert msg['id']==id, msg
            if path is None:return json.loads(payload) if payload else msg['result']
            return msg['result']['status'],json.loads(payload)
    def close(self):
        self.write({'type':'shutdown'}); self.process.wait(timeout=5)
        assert self.process.returncode==0,self.process.stderr.read().decode()
        self.process.stdin.close(); self.process.stdout.close(); self.process.stderr.close()

class Integration(unittest.TestCase):
    def setUp(self): self.host=Host()
    def tearDown(self): self.host.close()
    def run_request(self,id,mode='question'):
        return self.host.call('POST','/run',{'id':id,'batch_id':'fixture-batch','account_id':'fixture-account','mode':mode,'model':'fixture-model','effort':'medium' if mode=='question' else 'default','client_key_id':'fixture-key' if mode=='question' else None,'question_id':'candy' if mode=='question' else None})
    def test_registration_catalog_model_and_persistent_replay(self):
        reg=self.host.call('plugin.register',stage='registration'); self.assertIn('management',reg['contributes'])
        self.assertEqual(self.host.call('management.register',stage='registration')['pages'][0]['title'],'模型质量测试')
        status,catalog=self.host.call('GET','/catalog'); self.assertEqual(status,200);self.assertEqual(len(catalog['questions']),10)
        self.assertEqual(self.host.call('POST','/models',{'client_key_id':'fixture-key'})[1]['models'],['fixture-model'])
        status,answer=self.run_request('fixture-operation'); self.assertEqual(status,200); self.assertEqual(answer['record']['status'],'correct')
        self.assertEqual(self.run_request('fixture-operation')[1]['replayed'],True); self.assertEqual(self.host.model_count,1)
        self.assertEqual(self.host.call('GET','/history')[1]['records'][0]['status'],'correct')
    def test_probe_ticket_cookie_and_no_secret_persistence(self):
        status,result=self.run_request('fixture-probe','probe'); self.assertEqual(status,200); self.assertEqual(result['record']['status'],'degraded')
        self.assertNotIn('x-codex-turn-state',self.host.http[0]); self.assertNotIn('cookie',self.host.http[0])
        self.assertEqual(self.host.http[1]['x-codex-turn-state'],'ticket-first'); self.assertEqual(self.host.http[1]['cookie'],'__oailb=route-fixture')
        state=dump(self.host.states).decode(); self.assertNotIn('ticket-first',state);self.assertNotIn('fixture-token-not-real',state);self.assertNotIn('route-fixture',state)
    def test_failed_stream_is_inconclusive(self):
        self.host.bad_stream=True
        self.assertEqual(self.run_request('fixture-failed','probe')[1]['record']['status'],'inconclusive')
        self.assertEqual(len(self.host.http),1)
    def test_custom_bank_compare_and_swap(self):
        q={'id':'custom_sum','title':'加法','category':'数学','prompt':'17+28，只输出整数','answer':'45'}
        self.assertEqual(self.host.call('POST','/questions',{'questions':[q],'expected_version':None})[0],200)
        self.assertEqual(self.host.call('POST','/questions',{'questions':[q],'expected_version':None})[0],409)
        self.assertEqual(len(self.host.call('GET','/catalog')[1]['questions']),11)

if __name__=='__main__': unittest.main(verbosity=2)
