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
import sys
sys.path.insert(0,str(pathlib.Path(__file__).resolve().parent))
from proxy_fixture import ProxyFixture

ROOT = pathlib.Path(__file__).resolve().parents[1]

def dump(value):
    return json.dumps(value, ensure_ascii=False, separators=(',', ':')).encode()

def events(answer='21'):
    metadata = dump({'facts': [
        {'type':'text_delta','index':0,'text':answer},
        {'type':'completed','id':'fixture','model':'fixture-model','reason':'stop'}
    ]})
    event = b'GPE2' + struct.pack('>III', len(metadata), 0, 0) + metadata
    return b'HME1' + struct.pack('>II', 1, len(event)) + event

def frame_bytes(message,payload=b''):
    # 协议7沿用独立JSON前缀，另需配对调度控制帧与回调消费确认。
    import copy
    message=copy.deepcopy(message);prefix=b''
    if message['type']=='call':
        context=message['context']
        snapshot=context.pop('request',None)
        if snapshot is not None:
            request=dump(snapshot);message['request_context_bytes']=len(request);prefix+=request
    key={'hello':'configuration','call':'params','callback':'params','result':'result'}.get(message['type'])
    if key is not None:
        owner=message['handshake'] if message['type']=='hello' else message
        data=dump(owner.pop(key));message['data_bytes']=len(data);prefix+=data
    metadata=dump(message)
    assert 0<len(metadata)<=65536
    return struct.pack('>IQ',len(metadata),len(prefix)+len(payload))+metadata+prefix+payload

def read_frame(stream):
    header=stream.read(12)
    if not header:return None
    m,b=struct.unpack('>IQ',header);assert 0<m<=65536
    metadata=stream.read(m);message=json.loads(metadata);body=stream.read(b)
    request_bytes=message.pop('request_context_bytes',0);data_bytes=message.pop('data_bytes',0)
    assert request_bytes+data_bytes<=len(body)
    if request_bytes:message['context']['request']=json.loads(body[:request_bytes])
    key={'hello':'configuration','call':'params','callback':'params','result':'result'}.get(message['type'])
    assert (key is not None)==(data_bytes>0)
    if key:
        owner=message['handshake'] if message['type']=='hello' else message
        owner[key]=json.loads(body[request_bytes:request_bytes+data_bytes])
    return message,body[request_bytes+data_bytes:]

class Host:
    def __init__(self):
        self.proxy=ProxyFixture(self)
        self.proxy_url=self.proxy.url
        self.key_enabled=True
        self.key_groups=[]
        self.group_enabled=True
        self.model_visible=True
        self.model_names=['fixture-model']
        self.proxy_configured=True
        self.export_id='fixture-account'
        self.export_token='fixture-token-not-real'
        self.export_count=0
        self.model_policy={'mode':'all','models':[]}
        self.client_profile={'originator':'Codex Desktop','codexVersion':'0.153.4','userAgent':'Codex Desktop/0.153.4 (Mac OS 15.7.1; arm64) unknown (Codex Desktop; 26.901.51231)'}
        self.profile_error=False
        self.identity_snapshots=[]
        self.process = subprocess.Popen([str(ROOT/'target/debug'/('model-quality-test.exe' if os.name=='nt' else 'model-quality-test'))], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,env={**os.environ,'SSL_CERT_FILE':str(self.proxy.ca_path)})
        self.messages = queue.Queue()
        self.states = {}
        self.calls = []
        self.streams = {}
        self.http = self.proxy.requests
        self.ticket_second = 'ticket-new'
        self.model_answer = '21'
        self.model_fault = None
        self.model_http_status = None
        self.model_bad_payload = False
        self.on_model = None
        self.bad_stream = False
        self.model_count = 0
        self.account_enabled = True
        self.account_notes = '测试账号'
        self.disable_count = 0
        self.dispatch_status = 200
        self.http_status = 200
        self.http_responses = []
        self.error_body = None
        self.fail_disable = False
        self.body_streams = {}
        self.state_metadata_sizes = []
        self.on_stream = None
        self.on_account_read = None
        self.stall_stream = False
        self.scheduling = {}
        self.resident_callbacks = {}
        self.control_events = []
        self.defer_resumes = False
        self.pending_resumes = []
        self.resume_requested = threading.Event()
        self.write_lock = threading.Lock()
        self.write({'type':'hello','handshake':{'protocol_version':7,'artifact_sha256':'0'*64,'plugin_id':'xunzhimeng.model-quality-test','instance_id':'fixture','generation':1,'incarnation':'fixture-1','configuration':{},'contributes':{'maintenance':{'id':'xunzhimeng.model-quality-test.maintenance','version':1,'stages':['maintenance']},'management':{'id':'xunzhimeng.model-quality-test.management','version':2,'stages':['management']}}}})
        threading.Thread(target=self.reader, daemon=True).start()
        message,_ = self.messages.get(timeout=5)
        assert message['type'] == 'ready' and message['protocol_version'] == 7, message
        self.call_budgets=message['call_budgets']
        assert self.call_budgets=={'management':120000,'maintenance':210000},message
        self.next_id = 11
        self.registration = self.call('management.register', stage='registration')
        # 先检查相对路由，再按注册表分发，避免桩直接接受宿主会拒绝的路径。
        self.routes = {(route['method'], route['path']) for route in self.registration['routes']}
        assert len(self.routes) == len(self.registration['routes'])
        assert all(path and not path.startswith('/') for _, path in self.routes)
    def write(self, metadata, payload=b''):
        with self.write_lock:
            self.process.stdin.write(frame_bytes(metadata,payload))
            self.process.stdin.flush()
    def control(self, msg, payload):
        kind=msg['type']
        if kind not in ('suspend','resume','callback_consumed'):return False
        assert not payload,msg
        self.control_events.append(dict(msg))
        if kind=='callback_consumed':
            assert msg['id']>0 and msg['id']%2==0 and msg['parent_id']%2==1,msg
            parent=self.resident_callbacks.pop(msg['id'],None)
            assert parent is None or parent==msg['parent_id'],msg
            return True
        id=msg['id'];epoch=msg['epoch']
        if id not in self.scheduling:return True
        phase,previous=self.scheduling[id]
        if kind=='suspend':
            assert phase=='running' and epoch==previous+1,msg
            self.scheduling[id]=('suspended',epoch)
        else:
            assert phase=='suspended' and epoch==previous,msg
            self.scheduling[id]=('resuming',epoch)
            if self.defer_resumes:
                self.pending_resumes.append((id,epoch));self.resume_requested.set()
            else:self.grant_resume(id,epoch)
        return True
    def grant_resume(self,id,epoch):
        if self.scheduling.get(id)!=('resuming',epoch):return
        self.scheduling[id]=('running',epoch)
        self.write({'type':'resumed','id':id,'epoch':epoch})
    def finish_call(self,id):
        self.scheduling.pop(id,None)
        self.resident_callbacks={callback:parent for callback,parent in self.resident_callbacks.items() if parent!=id}
    def reader(self):
        try:
            while True:
                frame=read_frame(self.process.stdout)
                if frame is None:return
                if self.control(*frame):continue
                if frame[0]['type']=='callback':
                    msg=frame[0];assert msg['id'] not in self.resident_callbacks,msg
                    self.resident_callbacks[msg['id']]=msg['parent_id']
                self.messages.put(frame)
        except Exception as error: self.messages.put(({'type':'reader_error','error':str(error)},b''))
    def callback(self, msg, body):
        method=msg['method']; params=msg['params']; self.calls.append(method)
        # 解码后的业务JSON可超过64KiB；控制元数据上限由read_frame独立检查。
        result={}; payload=b''
        account={'account_id':'fixture-account','provider_id':'openai','credential_revision':1,'name':'OpenAI 账号','email':'fixture@example.test','upstream_user_id':None,'upstream_account_id':'fixture-upstream','plan_type':'plus','authentication_kind':'oauth','enabled':self.account_enabled,'credential_state':'ready','has_refresh_token':False,'access_token_expires_at_ms':None,'next_refresh_at_ms':None}
        if method.startswith('host.auth.'):
            assert params=={}; request=json.loads(body)
            if method=='host.auth.list': payload=dump({'accounts':[account],'next_cursor':None})
            elif method=='host.auth.get_runtime':
                if self.on_account_read:self.on_account_read()
                payload=dump(account)
            else:raise AssertionError('unexpected credential read '+method)
        elif method=='host.data.keys.get':
            assert params=={} and json.loads(body)=={'client_key_id':'fixture-key'}
            payload=dump({'schema_version':1,'client_key_id':'fixture-key','enabled':self.key_enabled,'group_ids':self.key_groups})
        elif method=='host.keys.list':
            assert not body; assert params['limit']==100
            result={'keys':[{'id':'fixture-key','name':'测试Key','enabled':True}],'next_cursor':None}
        elif method=='host.models.list':
            assert not body; assert params['client_key_id']=='fixture-key'
            result={'models':self.model_names if self.model_visible else []}
        elif method=='host.services.call':
            assert not body and params=={'operation':'settings.preview_client_profile','input':['openai',None]}
            if self.profile_error:
                result={'Err':{'kind':'unavailable','message':'fixture-profile-secret'}}
            else:
                self.identity_snapshots.append(dict(self.client_profile))
                result={'Ok':self.client_profile}
        elif method=='host.state.get':
            assert not body; assert params['namespace']=='quality'
            result={'record':self.states.get(params['key'])}
        elif method=='host.state.put':
            assert not body; key=params['key']; old=self.states.get(key)
            assert params['expected_version']==(old['version'] if old else None)
            self.state_metadata_sizes.append(len(dump(msg)))
            assert len(dump(msg)) <= 60*1024
            assert len(dump(params['value']))<262144
            version=(old['version']+1) if old else 1
            self.states[key]={'value':params['value'],'version':version,'schema_version':1}; result={'version':version}
        elif method=='host.model.execute':
            assert params['client_key_id']=='fixture-key' and params['account_id']=='fixture-account'
            assert json.loads(body)['reasoning']['effort']=='medium'
            assert self.states['history']['value']['records'][-1]['status']=='running'
            self.model_count+=1
            if self.on_model:self.on_model(msg)
            if self.model_fault:
                self.write({'type':'error','id':msg['id'],'error':{'code':self.model_fault,'message':'fixture model failure fixture-secret','send_state':'sent','http_status':self.model_http_status}})
                return
            if self.model_bad_payload:payload=b'invalid model batch'
            else:payload=events(self.model_answer)
            result={'request_id':'fixture-request','events':1}
        elif method=='host.http.dispatch':
            from urllib.parse import urlsplit,parse_qs
            assert params['settings'] is None
            assert not any(h['name'].lower() in ('authorization','cookie') for h in params['headers'])
            uri=urlsplit(params['uri']); code=self.dispatch_status
            if uri.path=='/api/admin/accounts':
                assert params['method']=='GET' and not body
                assert set(parse_qs(uri.query))=={'page','pageSize'}
                data={'items':[{'id':'fixture-account','name':'OpenAI 账号','email':'fixture@example.test','notes':self.account_notes,'enabled':self.account_enabled,'groups':[{'id':'fixture-group'}],'modelAccess':self.model_policy,'outboundProxyEndpoint':'http://127.0.0.1' if self.proxy_configured else None}], 'page':{'totalPages':1}}
            elif uri.path=='/api/admin/account-groups':
                assert set(parse_qs(uri.query))=={'page','pageSize'}
                data={'items':[{'id':'fixture-group','enabled':self.group_enabled}],'page':{'totalPages':1}}
            elif uri.path=='/api/admin/accounts/export':
                assert parse_qs(uri.query)=={'accountIds':['fixture-account'],'confirm':['export_sensitive_accounts']}
                assert params['method']=='GET' and not body
                self.export_count+=1
                data={'documents':[{'provider':'openai','document':{'accounts':[{'id':self.export_id,'accessToken':self.export_token,'accountId':'fixture-upstream','outboundProxyUrl':self.proxy_url}]}}]}
            elif uri.path=='/api/admin/accounts/batch-update':
                assert params['method']=='POST'
                assert json.loads(body)=={'accountIds':['fixture-account'],'enabled':False}
                assert any(t['state']['accounts'].get('fixture-account',{}).get('action')=='pending' for t in self.states['monitor']['value']['tasks'])
                self.disable_count+=1
                if self.fail_disable: code=503
                else: self.account_enabled=False
                data={'count':1}
            else: raise AssertionError('unexpected admin route '+uri.path)
            handle=f'admin-{len(self.body_streams)}'
            self.body_streams[handle]=dump({'code':200,'message':'OK','data':data})
            result={'status':code,'version':'HTTP/1.1','headers':[],'body':{'kind':'handle','handle':handle},'response':None,'session':False}
        elif method=='host.http.body_read':
            data=self.body_streams[params['handle']]
            result={'eof':data is None,'trailers':None}; payload=data or b''
            self.body_streams[params['handle']]=None
        elif method=='host.http.body_close': result={}

        else: raise AssertionError('unexpected callback '+method)
        self.write({'type':'result','id':msg['id'],'result':result},payload)
    def call(self, method, path=None, body=None, stage='management'):
        if path is not None:
            assert (method, path) in self.routes, 'request is not a registered relative route'
        id=self.next_id; self.next_id+=2
        self.scheduling[id]=('running',0)
        params={} if path is None else {'method':method,'path':path,'query':'','content_type':'application/json' if body is not None else None,'headers':[{'name':'authorization','value':list(b'fixture-admin-not-real')}]}
        self.write({'type':'call','id':id,'method':method if path is None else 'management.handle','context':{'call_id':id,'instance_id':'fixture','generation':1,'incarnation':'fixture-1','stage':stage,'timeout_ms':self.call_budgets.get(stage,5000 if stage=='registration' else 120000),'resource_stream':False,'resource_scope_id':str(id)},'params':params},dump(body) if body is not None else b'')
        while True:
            msg,payload=self.messages.get(timeout=self.call_budgets.get(stage,35000)/1000+10)
            if msg['type']=='cancel': continue
            if msg['type']=='cancelled':
                assert msg['id']==id,msg
                self.finish_call(id)
                return 'cancelled',{}
            if msg['type']=='callback': self.callback(msg,payload); continue
            assert msg['type']=='result', msg
            assert msg['id']==id, msg
            self.finish_call(id)
            if path is None:return json.loads(payload) if payload else msg['result']
            assert msg['result']['headers']==[]
            return msg['result']['status'],json.loads(payload)
    def close(self):
        self.write({'type':'shutdown'}); self.process.wait(timeout=5)
        assert self.process.returncode==0,self.process.stderr.read().decode()
        self.process.stdin.close(); self.process.stdout.close(); self.process.stderr.close()
        self.proxy.close()
        assert not self.proxy.errors,self.proxy.errors

class Integration(unittest.TestCase):
    def setUp(self): self.host=Host()
    def tearDown(self): self.host.close()
    def run_request(self,id,mode='question',model='fixture-model'):
        return self.host.call('POST','run',{'id':id,'batch_id':'fixture-batch','account_id':'fixture-account','mode':mode,'model':model,'effort':'medium' if mode=='question' else 'default','client_key_id':'fixture-key','question_id':'candy' if mode=='question' else None})
    def task_state(self):
        return self.host.states['monitor']['value']['tasks'][0]['state']
    def monitor_snapshot(self):
        status,snapshot=self.host.call('GET','monitor')
        self.assertEqual(status,200,snapshot)
        if snapshot['tasks']:
            self.assertEqual(len(snapshot['tasks']),1)
            return snapshot['tasks'][0]
        return {'id':None,'state':{'settings':snapshot['defaults'],'generation':'','accounts':{},'paused':False,'control_revision':0},'availability':{},'total_runs':0,'daily_runs':0}
    def save_monitor(self, body):
        body={**body,'task_id':self.monitor_snapshot()['id'],'name':'测试监控任务'}
        status,result=self.host.call('POST','monitor',body)
        return (status,result['tasks'][0]) if status==200 else (status,result)
    def configure_monitor(self,scheduled=True,auto_disable=True,**settings):
        snapshot=self.monitor_snapshot()
        config={'scheduled':scheduled,'auto_disable':auto_disable,'interval_minutes':30,'model':'fixture-model','client_key_id':'fixture-key','account_ids':['fixture-account'],'daily_max':48,'quiet_hours':None}
        config.update(settings)
        status,result=self.save_monitor({'settings':config,'expected_generation':snapshot['state']['generation'] or None})
        self.assertEqual(status,200,result)
        return result
    def due(self,confirmation=False):
        status=self.task_state()['accounts']['fixture-account']
        if confirmation:status['confirm_due_ms']=int(time.time()*1000)-1000
        else:status['next_due_ms']=0
    def control(self,paused,account_id=None,snapshot=None):
        state=(snapshot or self.monitor_snapshot())['state']
        status,result=self.host.call('POST','monitor/control',{'task_id':self.monitor_snapshot()['id'],'paused':paused,'account_id':account_id,'expected_generation':state['generation'],'expected_control_revision':state['control_revision']})
        return (status,result['tasks'][0]) if status==200 else (status,result)
    def seed_near_limit_history(self,size):
        import copy
        base=self.run_request('fixture-seed','probe')[1]['record']
        records=[]
        for i in range(40):
            record=copy.deepcopy(base);record['id']=f'fixture-history-{i}';records.append(record)
        history={'records':records,'evicted':0}
        gap=size-len(dump(history));self.assertGreater(gap,0)
        history['records'][0]['detail']+='x'*gap
        self.assertEqual(len(dump(history)),size)
        self.host.states['history']['value']=history
    def test_protocol_seven_resume_authorization_gates_callback_consumption(self):
        self.host.defer_resumes=True
        self.host.resume_requested.clear()
        results=[];errors=[]
        def run():
            try:results.append(self.run_request('fixture-v7-resume-gate'))
            except BaseException as error:errors.append(error)
        worker=threading.Thread(target=run);worker.start()
        self.assertTrue(self.host.resume_requested.wait(5))
        self.assertTrue(worker.is_alive())
        id,epoch=self.host.pending_resumes.pop(0)
        self.assertEqual(self.host.scheduling[id],('resuming',epoch))
        resident=[callback for callback,parent in self.host.resident_callbacks.items() if parent==id]
        self.assertTrue(resident)
        self.assertFalse(any(e['type']=='callback_consumed' and e['id'] in resident for e in self.host.control_events))
        self.host.defer_resumes=False;self.host.grant_resume(id,epoch)
        worker.join(10);self.assertFalse(worker.is_alive());self.assertEqual(errors,[])
        self.assertEqual(results[0][0],200);self.assertEqual(results[0][1]['record']['status'],'correct')
        self.assertTrue(all(any(e['type']=='callback_consumed' and e['id']==callback for e in self.host.control_events) for callback in resident))
        self.assertEqual(self.host.resident_callbacks,{})
    def test_protocol_seven_cancel_while_waiting_resume_allows_next_test(self):
        self.host.defer_resumes=True;self.host.resume_requested.clear()
        results=[];errors=[];call_id=self.host.next_id
        def run():
            try:results.append(self.run_request('fixture-v7-cancel-resume'))
            except BaseException as error:errors.append(error)
        worker=threading.Thread(target=run);worker.start()
        self.assertTrue(self.host.resume_requested.wait(5))
        self.host.write({'type':'cancel','id':call_id})
        worker.join(10);self.assertFalse(worker.is_alive());self.assertEqual(errors,[])
        self.assertEqual(results[0][0],'cancelled');self.assertEqual(self.host.resident_callbacks,{})
        self.host.defer_resumes=False
        status,result=self.run_request('fixture-v7-after-cancel-resume')
        self.assertEqual(status,200,result);self.assertEqual(result['record']['status'],'correct')
    def delete_history(self,account_id='fixture-account',snapshot=None):
        if snapshot is None:snapshot=self.host.call('GET','history')[1]
        return self.host.call('POST','history/delete',{'account_id':account_id,'expected_version':snapshot['version']})
    def test_delete_account_history_preserves_other_records_bank_monitor_and_counters(self):
        import copy
        self.configure_monitor(auto_disable=False);self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.run_request('fixture-delete-question')
        other=copy.deepcopy(self.host.states['history']['value']['records'][-1]);other.update(id='fixture-other-record',account_id='removed-host-account',account_name='已移除账号')
        self.host.states['history']['value']['records'].append(other)
        self.host.states['history']['value']['evicted']=12
        self.host.call('POST','questions',{'questions':[{'id':'custom_keep','title':'保留题目','category':'数学','prompt':'1+1','answer':'2'}],'expected_version':None})
        before=copy.deepcopy(self.host.states);snapshot=self.host.call('GET','history')[1]
        status,result=self.delete_history(snapshot=snapshot)
        self.assertEqual(status,200,result);self.assertEqual(result['deleted'],2)
        self.assertEqual(result['history']['records'],[other]);self.assertEqual(result['history']['evicted'],12)
        self.assertGreater(result['history']['version'],snapshot['version'])
        self.assertEqual(self.host.states['monitor'],before['monitor']);self.assertEqual(self.host.states['bank'],before['bank'])
        self.assertTrue(self.host.account_enabled);self.assertEqual(self.host.disable_count,0)
        self.assertNotIn('last_result',self.monitor_snapshot()['state']['accounts']['fixture-account'])
        # 宿主目录已经没有的账号仍可按历史内部ID清除。
        status,result=self.delete_history('removed-host-account')
        self.assertEqual(status,200,result);self.assertEqual(result['deleted'],1);self.assertEqual(result['history']['records'],[])
    def test_delete_history_rejects_stale_snapshot_and_keeps_new_results(self):
        self.run_request('fixture-delete-cas-old');snapshot=self.host.call('GET','history')[1]
        self.run_request('fixture-delete-cas-new')
        status,result=self.delete_history(snapshot=snapshot)
        self.assertEqual(status,409,result);self.assertEqual(len(self.host.states['history']['value']['records']),2)
        self.assertEqual(self.delete_history()[1]['deleted'],2)
    def test_delete_history_blocks_only_actual_inflight_test(self):
        self.run_request('fixture-delete-before-active')
        attempts=[]
        self.host.on_model=lambda msg:attempts.append(self.delete_history())
        self.assertEqual(self.run_request('fixture-delete-active')[0],200)
        self.assertEqual(attempts[0][0],400);self.assertIn('正在执行',attempts[0][1]['error'])
        self.host.on_model=None
        self.assertEqual(self.delete_history()[1]['deleted'],2)
    def test_delete_old_cancelled_unconfirmed_record_does_not_require_permanent_wait(self):
        call_id=self.host.next_id
        self.host.on_model=lambda msg:self.host.write({'type':'cancel','id':call_id})
        self.assertEqual(self.run_request('fixture-delete-cancelled')[0],'cancelled')
        self.host.on_model=None
        self.assertEqual(self.host.states['history']['value']['records'][0]['status'],'running')
        status,result=self.delete_history()
        self.assertEqual(status,200,result);self.assertEqual(result['deleted'],1)
        self.assertEqual(self.run_request('fixture-after-clearing-history')[0],200)
    def test_delete_history_uses_state_cas_and_releases_lock_after_write_failure(self):
        self.run_request('fixture-delete-write-failure')
        original=self.host.callback
        def reject_write(msg,body):
            if msg['method']=='host.state.put':
                self.host.write({'type':'error','id':msg['id'],'error':{'code':'conflict','message':'fixture conflict','send_state':'not_sent'}})
                return
            original(msg,body)
        self.host.callback=reject_write
        status,result=self.delete_history()
        self.assertEqual(status,400,result);self.assertEqual(len(self.host.states['history']['value']['records']),1)
        self.host.callback=original
        self.assertEqual(self.delete_history()[1]['deleted'],1)
        self.assertEqual(self.run_request('fixture-after-clearing-failure')[0],200)
    def test_delete_empty_history_and_invalid_account_do_not_write(self):
        status,result=self.delete_history()
        self.assertEqual(status,200,result);self.assertEqual(result['deleted'],0);self.assertIsNone(result['history']['version'])
        self.assertNotIn('history',self.host.states)
        for account in ['', 'x'*129, 'bad\naccount']:
            self.assertEqual(self.delete_history(account)[0],400)
        self.assertEqual(self.host.model_count,0)
    def test_question_error_displays_host_status_without_plugin_retry_or_secret(self):
        self.host.model_fault='upstream';self.host.model_http_status=429
        record=self.run_request('fixture-question-status')[1]['record']
        self.assertEqual(record['status'],'failed');self.assertIn('HTTP 429',record['detail'])
        self.assertIn('upstream',record['detail']);self.assertNotIn('fixture-secret',dump(record).decode())
        self.assertEqual(self.host.model_count,1)
    def test_terminal_question_errors_release_account_immediately(self):
        for i,fault in enumerate(['invalid_input','timeout']):
            self.host.model_fault=fault
            first=self.run_request(f'fixture-terminal-fault-{i}')[1]['record']
            self.assertEqual(first['status'],'failed');self.assertIn('宿主已结束',first['detail'])
            self.host.model_fault=None
            status,result=self.run_request(f'fixture-after-fault-{i}')
            self.assertEqual(status,200,result);self.assertEqual(result['record']['status'],'correct')
        self.assertEqual(self.host.model_count,4)
    def test_invalid_completed_model_payload_does_not_quarantine(self):
        self.host.model_bad_payload=True
        self.assertEqual(self.run_request('fixture-invalid-payload')[1]['record']['status'],'failed')
        self.host.model_bad_payload=False
        self.assertEqual(self.run_request('fixture-valid-after-payload')[1]['record']['status'],'correct')
        self.host.model_answer='x'*9000
        self.assertEqual(self.run_request('fixture-long-answer')[1]['record']['status'],'failed')
        self.host.model_answer='21'
        self.assertEqual(self.run_request('fixture-after-long-answer')[1]['record']['status'],'correct')
    def test_parent_cancelled_question_allows_new_operation_without_replaying_old(self):
        call_id=self.host.next_id
        self.host.on_model=lambda msg:self.host.write({'type':'cancel','id':call_id})
        self.assertEqual(self.run_request('fixture-cancel-question')[0],'cancelled')
        self.host.on_model=None
        status,result=self.run_request('fixture-after-cancel-question')
        self.assertEqual(status,200,result);self.assertEqual(result['record']['status'],'correct')
        old=next(r for r in self.host.states['history']['value']['records'] if r['id']=='fixture-cancel-question')
        self.assertEqual(old['status'],'running');self.assertIsNone(old['finished_at_ms'])
        before=self.host.model_count
        replay=self.run_request('fixture-cancel-question')[1]
        self.assertTrue(replay['replayed']);self.assertEqual(replay['record']['status'],'running')
        self.assertEqual(self.host.model_count,before)
    def test_repeated_parent_cancellations_do_not_exhaust_execution_slots(self):
        for index in range(5):
            call_id=self.host.next_id
            self.host.on_model=lambda msg:self.host.write({'type':'cancel','id':call_id})
            self.assertEqual(self.run_request(f'fixture-cancel-slot-{index}')[0],'cancelled')
        self.host.on_model=None
        status,result=self.run_request('fixture-after-cancel-slots')
        self.assertEqual(status,200,result);self.assertEqual(result['record']['status'],'correct')
    def test_result_write_failure_does_not_block_new_operation(self):
        original=self.host.callback
        writes=0
        def fail_result(msg,body):
            nonlocal writes
            if msg['method']=='host.state.put' and msg['params']['key']=='history':
                writes+=1
                if writes==2:
                    self.host.write({'type':'error','id':msg['id'],'error':{'code':'conflict','message':'fixture write failed','send_state':'not_sent'}})
                    return
            original(msg,body)
        self.host.callback=fail_result
        status,result=self.run_request('fixture-result-write-failure')
        self.assertEqual(status,400,result);self.assertIn('状态写入未确认',result['error'])
        self.host.callback=original
        prior=self.host.states['history']['value']['records'][-1].copy()
        self.assertEqual(prior['status'],'running')
        status,result=self.run_request('fixture-after-result-write-failure')
        self.assertEqual(status,200,result);self.assertEqual(result['record']['status'],'correct')
        self.assertEqual(self.host.states['history']['value']['records'][0],prior)
    def test_actually_running_account_still_rejects_overlap(self):
        overlaps=[]
        self.host.on_model=lambda msg:overlaps.append(self.run_request('fixture-overlap-attempt'))
        status,result=self.run_request('fixture-active-question')
        self.assertEqual(status,200,result)
        self.assertEqual(overlaps[0][0],400);self.assertIn('正在执行',overlaps[0][1]['error'])
        self.assertEqual(self.host.model_count,1)
        self.host.on_model=None
        self.assertEqual(self.run_request('fixture-after-active-question')[0],200)
    def new_task(self,name='第二任务',**settings):
        config={'scheduled':True,'auto_disable':False,'interval_minutes':30,'model':'fixture-model','client_key_id':'fixture-key','account_ids':['fixture-account'],'daily_max':48,'quiet_hours':None}
        config.update(settings)
        return self.host.call('POST','monitor',{'task_id':None,'name':name,'settings':config,'expected_generation':None})
    def delete_task(self,task):
        return self.host.call('POST','monitor/delete',{'task_id':task['id'],'expected_generation':task['state']['generation'],'expected_control_revision':task['state']['control_revision']})
    def test_delete_task_preserves_history_other_tasks_and_account_status(self):
        self.configure_monitor(auto_disable=False);self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.new_task('保留任务')
        before=self.host.call('GET','monitor')[1];history=dump(self.host.states['history']);account=self.host.account_enabled
        first,second=before['tasks']
        status,result=self.delete_task(first)
        self.assertEqual(status,200,result);self.assertEqual(result['tasks'],[second])
        self.assertEqual(result['total_runs'],second['total_runs'])
        self.assertGreater(result['control_revision'],before['control_revision'])
        self.assertEqual(dump(self.host.states['history']),history);self.assertEqual(self.host.account_enabled,account)
        self.assertEqual(self.host.disable_count,0)
        self.assertEqual(self.delete_task(first)[0],400)
        self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(self.host.proxy.connects,2)
        import copy
        states=copy.deepcopy(self.host.states)
        self.host.close();self.host=Host();self.host.states=states
        # 重启不重新迁移生成已删除任务。
        self.assertEqual([t['id'] for t in self.host.call('GET','monitor')[1]['tasks']],[second['id']])
    def test_delete_last_task_keeps_empty_registry_and_frees_creation_slot(self):
        for i in range(10):self.assertEqual(self.new_task(str(i),scheduled=False,auto_disable=False)[0],200)
        first=self.host.call('GET','monitor')[1]['tasks'][0]
        self.assertEqual(self.delete_task(first)[0],200)
        self.assertEqual(self.new_task('重新创建',scheduled=False,auto_disable=False)[0],200)
        for task in self.host.call('GET','monitor')[1]['tasks']:self.assertEqual(self.delete_task(task)[0],200)
        snapshot=self.host.call('GET','monitor')[1]
        self.assertEqual(snapshot['tasks'],[]);self.assertEqual(snapshot['total_runs'],0)
        self.assertEqual(self.host.states['monitor']['value']['tasks_version'],1)
    def test_delete_rejects_stale_edit_or_pause_version(self):
        self.configure_monitor()
        old=self.host.call('GET','monitor')[1]['tasks'][0]
        self.control(True)
        self.assertEqual(self.delete_task(old)[0],400)
        task=self.host.call('GET','monitor')[1]['tasks'][0]
        settings={**task['state']['settings'],'interval_minutes':45}
        self.host.call('POST','monitor',{'task_id':task['id'],'name':task['name'],'settings':settings,'expected_generation':task['state']['generation']})
        self.assertEqual(self.delete_task(task)[0],400)
        self.assertEqual(len(self.host.call('GET','monitor')[1]['tasks']),1)
    def test_delete_active_task_waits_for_completion_or_expired_isolation(self):
        self.configure_monitor();self.host.stall_stream=True;self.due()
        call_id=self.host.next_id
        self.host.on_stream=lambda:self.host.write({'type':'cancel','id':call_id})
        self.assertEqual(self.host.call('plugin.reconcile',stage='maintenance')[0],'cancelled')
        self.host.on_stream=None;self.host.stall_stream=False
        task=self.host.call('GET','monitor')[1]['tasks'][0]
        status,result=self.delete_task(task);self.assertEqual(status,400,result);self.assertIn('执行中或隔离',result['error'])
        self.task_state()['accounts']['fixture-account']['active']['started_ms']=0
        self.assertEqual(self.delete_task(task)[0],200)
        self.assertEqual(self.host.proxy.connects,1)
    def test_delete_cannot_drop_unconfirmed_disable_guard(self):
        self.configure_monitor(degradation_policy='immediate');self.host.fail_disable=True
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        task=self.host.call('GET','monitor')[1]['tasks'][0]
        status,result=self.delete_task(task)
        self.assertEqual(status,400,result);self.assertIn('未确认停用',result['error'])
        self.assertEqual(self.host.disable_count,1)
        self.assertEqual(len(self.host.call('GET','monitor')[1]['tasks']),1)
        self.task_state()['accounts']['fixture-account']['action']='pending'
        status,result=self.delete_task(task)
        self.assertEqual(status,400,result);self.assertIn('未确认停用',result['error'])
    def test_delete_confirmed_disabled_task_does_not_restore_account(self):
        self.configure_monitor(degradation_policy='immediate');self.due()
        self.host.call('plugin.reconcile',stage='maintenance')
        task=self.host.call('GET','monitor')[1]['tasks'][0]
        self.assertEqual(task['state']['accounts']['fixture-account']['action'],'disabled')
        self.assertFalse(self.host.account_enabled);self.assertEqual(self.host.disable_count,1)
        history=dump(self.host.states['history'])
        self.assertEqual(self.delete_task(task)[0],200)
        self.assertFalse(self.host.account_enabled);self.assertEqual(self.host.disable_count,1)
        self.assertEqual(dump(self.host.states['history']),history)
    def test_delete_does_not_refresh_stale_global_control(self):
        self.configure_monitor();before=self.host.call('GET','monitor')[1]
        self.assertEqual(self.delete_task(before['tasks'][0])[0],200)
        body={'task_id':None,'account_id':None,'paused':True,'expected_generation':'','expected_control_revision':before['control_revision']}
        self.assertEqual(self.host.call('POST','monitor/control',body)[0],400)
    def test_multiple_tasks_share_account_but_keep_settings_counts_and_confirmations_separate(self):
        self.configure_monitor(auto_disable=False,daily_max=1)
        status,snapshot=self.new_task('sol探针',daily_max=2,interval_minutes=60)
        self.assertEqual(status,200,snapshot);self.assertEqual(len(snapshot['tasks']),2)
        tasks=self.host.states['monitor']['value']['tasks']
        for task in tasks:task['state']['accounts']['fixture-account']['next_due_ms']=0
        self.host.call('plugin.reconcile',stage='maintenance')
        self.host.call('plugin.reconcile',stage='maintenance')
        snap=self.host.call('GET','monitor')[1]
        self.assertEqual([t['total_runs'] for t in snap['tasks']],[1,1]);self.assertEqual(snap['total_runs'],2)
        self.assertIsNone(snap['tasks'][0]['state']['accounts']['fixture-account']['confirm_due_ms'])
        self.assertIsNotNone(snap['tasks'][1]['state']['accounts']['fixture-account']['confirm_due_ms'])
        records=self.host.states['history']['value']['records']
        self.assertNotEqual(records[-1]['metrics']['monitor']['task_id'],records[-2]['metrics']['monitor']['task_id'])
        self.assertEqual(records[-1]['metrics']['monitor']['task_name'],'sol探针')
        self.assertEqual(self.host.disable_count,0)
    def test_edit_one_task_does_not_modify_other_task_or_allow_stale_save(self):
        self.configure_monitor();self.new_task()
        snap=self.host.call('GET','monitor')[1];first,second=snap['tasks']
        settings={**first['state']['settings'],'interval_minutes':45}
        body={'task_id':first['id'],'name':'已编辑','settings':settings,'expected_generation':first['state']['generation']}
        status,result=self.host.call('POST','monitor',body);self.assertEqual(status,200,result)
        self.assertEqual(result['tasks'][1],second)
        self.assertEqual(self.host.call('POST','monitor',body)[0],400)
        self.assertEqual(self.host.call('GET','monitor')[1]['tasks'][0]['name'],'已编辑')
    def test_task_and_global_controls_and_stale_global_revision(self):
        self.configure_monitor();self.new_task()
        snap=self.host.call('GET','monitor')[1]
        body={'task_id':None,'account_id':None,'paused':True,'expected_generation':'','expected_control_revision':snap['control_revision']}
        status,snap=self.host.call('POST','monitor/control',body);self.assertEqual(status,200)
        self.assertTrue(all(t['state']['paused'] for t in snap['tasks']))
        self.assertEqual(self.host.call('POST','monitor/control',body)[0],400)
        task=snap['tasks'][1]
        status,snap=self.host.call('POST','monitor/control',{'task_id':task['id'],'account_id':None,'paused':False,'expected_generation':task['state']['generation'],'expected_control_revision':task['state']['control_revision']})
        self.assertEqual(status,200);self.assertTrue(snap['tasks'][0]['state']['paused']);self.assertFalse(snap['tasks'][1]['state']['paused'])
    def test_old_v060_monitor_migrates_to_paused_task_with_counters_and_audit(self):
        self.configure_monitor();self.due();self.host.call('plugin.reconcile',stage='maintenance')
        import copy
        old=copy.deepcopy(self.task_state());old['accounts']['fixture-account']['action']='unconfirmed'
        self.host.states['monitor']['value']=old
        before=self.host.proxy.connects
        snap=self.host.call('GET','monitor')[1]
        self.assertEqual(len(snap['tasks']),1);task=snap['tasks'][0]
        self.assertTrue(task['state']['paused']);self.assertTrue(task['state']['settings']['scheduled'])
        self.assertEqual(task['total_runs'],1);self.assertEqual(task['daily_runs'],1)
        self.assertEqual(task['state']['accounts']['fixture-account']['action'],'unconfirmed')
        self.assertIsNone(task['state']['accounts']['fixture-account']['confirm_due_ms'])
        self.assertEqual(self.host.call('GET','monitor')[1]['tasks'][0]['id'],task['id'])
        self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(self.host.proxy.connects,before)
    def test_unconfirmed_disable_is_not_repeated_by_other_task(self):
        self.configure_monitor(degradation_policy='immediate');self.new_task(degradation_policy='immediate',auto_disable=True)
        self.host.fail_disable=True
        for task in self.host.states['monitor']['value']['tasks']:task['state']['accounts']['fixture-account']['next_due_ms']=0
        self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.disable_count,1)
        self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.disable_count,1)
        records=self.host.states['history']['value']['records']
        self.assertNotEqual(records[-1]['metrics']['monitor']['task_id'],records[-2]['metrics']['monitor']['task_id'])
        snap=self.host.call('GET','monitor')[1]
        self.assertEqual([t['total_runs'] for t in snap['tasks']],[1,1])
    def test_multiple_tasks_do_not_overlap_cancelled_active_account(self):
        self.configure_monitor();self.new_task()
        tasks=self.host.states['monitor']['value']['tasks']
        for task in tasks:task['state']['accounts']['fixture-account']['next_due_ms']=0
        self.host.stall_stream=True;call_id=self.host.next_id
        self.host.on_stream=lambda:self.host.write({'type':'cancel','id':call_id})
        self.assertEqual(self.host.call('plugin.reconcile',stage='maintenance')[0],'cancelled')
        self.host.stall_stream=False;self.host.on_stream=None
        self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.proxy.connects,1)
        self.assertEqual(self.host.call('GET','monitor')[1]['tasks'][1]['total_runs'],0)
    def test_task_capacity_admission_rejects_growth_but_allows_pause_and_disable(self):
        self.configure_monitor(auto_disable=False)
        task=self.host.states['monitor']['value']['tasks'][0]
        # 模拟接近预算的既有审计，不裁剪真实审计换取创建成功。
        task['state']['accounts']['fixture-account']['action_detail']='x'*56000
        snapshot=self.host.call('GET','monitor')[1];before=dump(self.host.states['monitor'])
        status,result=self.new_task('无法安全容纳的任务')
        self.assertEqual(status,400,result);self.assertIn('总容量',result['error'])
        self.assertEqual(dump(self.host.states['monitor']),before)
        task=snapshot['tasks'][0]
        status,result=self.host.call('POST','monitor/control',{'task_id':task['id'],'account_id':None,'paused':True,'expected_generation':task['state']['generation'],'expected_control_revision':task['state']['control_revision']})
        self.assertEqual(status,200,result)
        task=result['tasks'][0]
        settings={**task['state']['settings'],'scheduled':False,'auto_disable':False}
        status,result=self.host.call('POST','monitor',{'task_id':task['id'],'name':task['name'],'settings':settings,'expected_generation':task['state']['generation']})
        self.assertEqual(status,200,result)
        self.assertEqual(result['tasks'][0]['state']['accounts']['fixture-account']['action_detail'],'x'*56000)
    def test_task_limit_and_name_validation(self):
        for i in range(10):self.assertEqual(self.new_task(str(i),scheduled=False,auto_disable=False)[0],200)
        self.assertEqual(self.new_task('十一')[0],400)
        self.assertEqual(len(self.host.call('GET','monitor')[1]['tasks']),10)
    def test_legacy_65287_history_allows_new_probe_and_persistent_result(self):
        self.seed_near_limit_history(65287)
        before=self.host.proxy.connects
        status,result=self.run_request('fixture-frame-recovery','probe')
        self.assertEqual(status,200,result);self.assertEqual(result['record']['status'],'degraded')
        self.assertEqual(self.host.proxy.connects,before+2)
        history=self.host.call('GET','history')[1]
        self.assertGreater(history['evicted'],0)
        self.assertEqual(history['records'][-1]['id'],'fixture-frame-recovery')
        self.assertNotEqual(history['records'][-1]['status'],'running')
        self.assertLess(len(dump(history)),60*1024)
        self.assertTrue(self.run_request('fixture-frame-recovery','probe')[1]['replayed'])
        self.assertEqual(self.host.proxy.connects,before+2)
    def test_completion_capacity_growth_does_not_leave_running(self):
        self.seed_near_limit_history(64299)
        status,result=self.run_request('fixture-frame-completion','probe')
        self.assertEqual(status,200,result)
        history=self.host.call('GET','history')[1]
        self.assertEqual(history['records'][-1]['status'],'degraded')
        self.assertIsNotNone(history['records'][-1]['finished_at_ms'])
        self.assertTrue(all(size<=60*1024 for size in self.host.state_metadata_sizes))
    def test_many_probe_results_never_cross_metadata_budget(self):
        for i in range(60):
            status,result=self.run_request(f'fixture-growth-{i}','probe')
            self.assertEqual(status,200,result);self.assertEqual(result['record']['status'],'degraded')
        history=self.host.call('GET','history')[1]
        self.assertGreater(history['evicted'],0)
        self.assertEqual(len(history['records'])+history['evicted'],60)
        self.assertTrue(all(r['status']!='running' for r in history['records']))
        self.assertEqual(self.host.proxy.connects,120)
    def test_unconfirmed_legacy_records_are_not_fabricated_or_replayed(self):
        self.seed_near_limit_history(65287)
        history=self.host.states['history']['value']
        pending=history['records'][-1];pending['id']='fixture-old-unconfirmed';pending['status']='running'
        pending['started_at_ms']=int(time.time()*1000)-180000
        pending['finished_at_ms']=None;pending['latency_ms']=None
        before=self.host.proxy.connects
        result=self.run_request('fixture-old-unconfirmed','probe')[1]
        self.assertTrue(result['replayed']);self.assertEqual(result['record']['status'],'running')
        self.assertEqual(self.host.proxy.connects,before)
        self.assertEqual(self.run_request('fixture-new-after-old','probe')[0],200)
        existing=next((r for r in self.host.call('GET','history')[1]['records'] if r['id']=='fixture-old-unconfirmed'),None)
        if existing:self.assertEqual(existing['status'],'running');self.assertIsNone(existing['finished_at_ms'])
    def test_large_escaped_answer_records_capacity_failure_not_running(self):
        question={'id':'custom_escaped','title':'容量边界','category':'测试','prompt':'\0'*7999,'answer':'1'}
        self.assertEqual(self.host.call('POST','questions',{'questions':[question],'expected_version':None})[0],200)
        self.host.model_answer='\0'*8192
        request={'id':'fixture-large-answer','batch_id':'fixture-batch','account_id':'fixture-account','mode':'question','model':'fixture-model','effort':'medium','client_key_id':'fixture-key','question_id':question['id']}
        status,result=self.host.call('POST','run',request)
        self.assertEqual(status,200,result)
        record=result['record']
        self.assertEqual(record['status'],'failed');self.assertEqual(record['metrics']['storage_error'],'result_too_large')
        self.assertIsNone(record['answer']);self.assertIsNotNone(record['finished_at_ms'])
        self.assertEqual(self.host.states['history']['value']['records'][-1]['status'],'failed')
        self.assertTrue(self.host.call('POST','run',request)[1]['replayed']);self.assertEqual(self.host.model_count,1)
    def test_bank_transport_limit_rejects_before_state_write(self):
        bank=[{'id':f'custom_large_{i}','title':'题目','category':'数学','prompt':'x'*7999,'answer':'1'} for i in range(9)]
        status,result=self.host.call('POST','questions',{'questions':bank,'expected_version':None})
        self.assertEqual(status,400,result);self.assertIn('60KiB',result['error'])
        self.assertNotIn('bank',self.host.states)
        bank=bank[:6]
        self.assertEqual(self.host.call('POST','questions',{'questions':bank,'expected_version':None})[0],200)
        self.assertEqual(len([q for q in self.host.call('GET','catalog')[1]['questions'] if q['id'].startswith('custom_')]),6)
    def test_immediate_policy_disables_on_first_degraded_without_confirmation(self):
        self.configure_monitor(degradation_policy='immediate',daily_max=1)
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        snapshot=self.monitor_snapshot()
        state=snapshot['state']['accounts']['fixture-account']
        self.assertEqual(state['streak'],1);self.assertIsNone(state['confirm_due_ms'])
        self.assertEqual(snapshot['total_runs'],1);self.assertEqual(snapshot['daily_runs'],1)
        self.assertEqual(self.host.disable_count,1);self.assertFalse(self.host.account_enabled)
        metrics=self.host.states['history']['value']['records'][-1]['metrics']['monitor']
        self.assertEqual(metrics['policy'],'immediate');self.assertEqual(metrics['threshold'],1)
        self.assertFalse(metrics['confirmation']);self.assertIn('首次疑似降级',metrics['detail'])
        self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.proxy.connects,2);self.assertEqual(self.host.disable_count,1)
    def test_immediate_policy_requires_switch_and_explicit_degraded(self):
        self.configure_monitor(degradation_policy='immediate',auto_disable=False)
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.disable_count,0)
        self.assertIsNone(self.task_state()['accounts']['fixture-account']['confirm_due_ms'])
        self.configure_monitor(degradation_policy='immediate')
        self.host.http_status=400;self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.disable_count,0)
        self.assertEqual(self.task_state()['accounts']['fixture-account']['streak'],0)
        self.host.http_status=200;self.host.ticket_second='ticket-first'
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.disable_count,0)
    def test_configured_delay_longer_than_interval_is_not_overridden(self):
        self.configure_monitor(confirmation_delay_seconds=600,interval_minutes=5)
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        state=self.task_state()['accounts']['fixture-account']
        record=self.host.states['history']['value']['records'][-1]
        self.assertEqual(state['confirm_due_ms'],record['finished_at_ms']+600000)
        self.assertEqual(state['next_due_ms'],state['confirm_due_ms']+300000)
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.proxy.connects,2)
        state=self.task_state()['accounts']['fixture-account']
        state['next_due_ms']=int(time.time()*1000)+300000
        self.due(confirmation=True);self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.disable_count,1)
        self.assertEqual(self.monitor_snapshot()['total_runs'],2)
    def test_failed_confirmation_never_disables_or_retries_again(self):
        self.configure_monitor(confirmation_delay_seconds=5)
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        state=self.task_state()['accounts']['fixture-account']
        record=self.host.states['history']['value']['records'][-1]
        self.assertEqual(state['confirm_due_ms'],record['finished_at_ms']+5000)
        self.host.http_status=429;self.due(confirmation=True);self.host.call('plugin.reconcile',stage='maintenance')
        state=self.task_state()['accounts']['fixture-account']
        self.assertEqual(state['total_runs'],2);self.assertEqual(state['streak'],0)
        self.assertIsNone(state['confirm_due_ms']);self.assertEqual(self.host.disable_count,0)
        # 确认测试内部重试3次，仍只计1轮确认；后续tick不再追加确认。
        record=self.host.states['history']['value']['records'][-1]
        self.assertEqual(record['metrics']['attempt_count'],3)
        self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(self.host.proxy.connects,5)
    def test_policy_change_discards_old_confirmation_and_keeps_counts(self):
        self.configure_monitor();self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertIsNotNone(self.task_state()['accounts']['fixture-account']['confirm_due_ms'])
        self.configure_monitor(degradation_policy='immediate')
        state=self.task_state()['accounts']['fixture-account']
        self.assertIsNone(state['confirm_due_ms']);self.assertEqual(state['streak'],0)
        self.assertEqual(state['total_runs'],1);self.assertEqual(self.host.disable_count,0)
        self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(self.host.proxy.connects,2)
    def test_invalid_policy_or_delay_rejected_without_side_effects(self):
        config=self.configure_monitor()
        for field,value in [('degradation_policy','unknown'),('confirmation_delay_seconds',0),('confirmation_delay_seconds',86401),('confirmation_delay_seconds',1.5)]:
            with self.subTest(field=field,value=value):
                settings={**config['state']['settings'],field:value}
                status,_=self.save_monitor({'settings':settings,'expected_generation':config['state']['generation']})
                self.assertEqual(status,400)
        self.assertEqual(self.monitor_snapshot()['state']['generation'],config['state']['generation'])
        self.assertEqual(self.host.export_count,0);self.assertEqual(self.host.disable_count,0)
    def test_existing_settings_default_to_confirmation_not_immediate(self):
        self.configure_monitor()
        state=self.task_state();state['settings'].pop('degradation_policy');state['settings'].pop('confirmation_delay_seconds')
        snapshot=self.monitor_snapshot()['state']
        self.assertEqual(snapshot['settings']['degradation_policy'],'confirm')
        self.assertEqual(snapshot['settings']['confirmation_delay_seconds'],60)
        self.assertTrue(snapshot['settings']['scheduled'])
    def test_v033_plan_migration_preserves_audit_and_is_idempotent(self):
        self.configure_monitor()
        state=self.task_state();state['probe_version']=1
        state['settings'].pop('daily_max');state['settings'].pop('quiet_hours')
        state['accounts']['fixture-account'].update({'action':'unconfirmed','action_detail':'fixture audit','streak':1})
        self.host.states['monitor']['value']=state
        first=self.monitor_snapshot()['state']
        self.assertTrue(first['settings']['scheduled']);self.assertTrue(first['settings']['auto_disable']);self.assertTrue(first['paused'])
        self.assertEqual(first['settings']['daily_max'],48);self.assertEqual(first['settings']['client_key_id'],'fixture-key')
        self.assertEqual(first['accounts']['fixture-account']['action'],'unconfirmed')
        self.assertEqual(first['accounts']['fixture-account']['streak'],0)
        self.assertEqual(first['generation'],self.monitor_snapshot()['state']['generation'])
        self.assertEqual(self.host.proxy.connects,0)
    def test_pause_before_begin_does_not_leave_running_history_or_count(self):
        self.configure_monitor();self.due()
        def pause():
            state=self.host.states['monitor'];state['value']['tasks'][0]['state']['paused']=True;state['version']+=1
        self.host.on_account_read=pause
        self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.proxy.connects,0);self.assertEqual(self.monitor_snapshot()['total_runs'],0)
        self.assertEqual(self.host.call('GET','history')[1]['records'],[])
    def test_expired_confirmation_starts_fresh_without_disabling(self):
        self.configure_monitor();self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.due(confirmation=True);self.due()
        self.host.call('plugin.reconcile',stage='maintenance')
        state=self.task_state()['accounts']['fixture-account']
        self.assertEqual(state['total_runs'],2);self.assertEqual(state['streak'],1)
        self.assertEqual(self.host.disable_count,0)
        self.assertFalse(self.host.states['history']['value']['records'][-1]['metrics']['monitor']['confirmation'])
    def test_confirmation_waits_one_minute_and_counts_both_attempts(self):
        self.configure_monitor();self.due();self.host.call('plugin.reconcile',stage='maintenance')
        state=self.task_state()['accounts']['fixture-account']
        record=self.host.states['history']['value']['records'][-1]
        self.assertEqual(state['confirm_due_ms'],record['finished_at_ms']+60000)
        self.assertEqual(state['total_runs'],1);self.assertEqual(state['daily_runs'],1)
        self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(len(self.host.http),2);self.assertEqual(self.host.disable_count,0)
        self.due(confirmation=True);self.host.call('plugin.reconcile',stage='maintenance')
        state=self.monitor_snapshot()
        self.assertEqual(state['total_runs'],2);self.assertEqual(state['daily_runs'],2)
        self.assertEqual(self.host.disable_count,1)
    def test_daily_quota_blocks_confirmation_and_save_does_not_reset_counts(self):
        config=self.configure_monitor(daily_max=1);self.due();self.host.call('plugin.reconcile',stage='maintenance')
        state=self.task_state()['accounts']['fixture-account']
        self.assertIsNone(state['confirm_due_ms']);self.assertEqual(state['streak'],0)
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(len(self.host.http),2);self.assertEqual(self.host.disable_count,0)
        status,snapshot=self.save_monitor({'settings':config['state']['settings'],'expected_generation':config['state']['generation']})
        self.assertEqual(status,200);self.assertEqual(snapshot['daily_runs'],1)
        self.assertEqual(snapshot['availability']['fixture-account']['status'],'今日已达上限')
        self.task_state()['accounts']['fixture-account']['count_day']-=1
        self.assertEqual(self.monitor_snapshot()['daily_runs'],0)
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.monitor_snapshot()['total_runs'],2)
    def test_quiet_hours_block_background_not_manual(self):
        minute=(int(time.time())//60+480)%1440
        hhmm=lambda value:f'{value//60:02d}:{value%60:02d}'
        self.configure_monitor(quiet_hours={'start':hhmm((minute-2)%1440),'end':hhmm((minute+2)%1440)})
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.proxy.connects,0)
        snap=self.monitor_snapshot();self.assertEqual(snap['availability']['fixture-account']['status'],'不测试时段')
        self.assertGreater(snap['availability']['fixture-account']['next_allowed_ms'],int(time.time()*1000))
        self.run_request('fixture-manual-during-quiet','probe')
        self.assertEqual(self.host.proxy.connects,2);self.assertEqual(self.monitor_snapshot()['total_runs'],0)
    def test_pause_resume_and_stale_control_preserve_counts(self):
        self.configure_monitor();self.due();self.host.call('plugin.reconcile',stage='maintenance')
        stale=self.monitor_snapshot()
        self.assertEqual(self.control(True,'fixture-account')[0],200)
        self.assertEqual(self.control(True,snapshot=stale)[0],400)
        state=self.task_state()['accounts']['fixture-account']
        self.assertIsNone(state['confirm_due_ms']);self.assertEqual(state['streak'],0)
        self.due();self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(self.host.proxy.connects,2)
        self.assertEqual(self.control(True)[0],200);self.assertEqual(self.control(False,'fixture-account')[0],200)
        self.due();self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(self.host.proxy.connects,2)
        self.assertEqual(self.control(False)[0],200)
        self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(self.host.proxy.connects,2)
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.monitor_snapshot()['total_runs'],2);self.assertEqual(self.host.disable_count,0)
    def test_pause_inflight_confirmation_prevents_disable_and_counts_once(self):
        self.configure_monitor();self.due();self.host.call('plugin.reconcile',stage='maintenance')
        def pause():
            state=self.host.states['monitor'];state['value']['tasks'][0]['state']['paused']=True
            state['value']['tasks'][0]['state']['accounts']['fixture-account']['active']['valid']=False;state['version']+=1
        self.host.on_stream=pause
        self.due(confirmation=True);self.host.call('plugin.reconcile',stage='maintenance')
        snap=self.monitor_snapshot();self.assertEqual(snap['total_runs'],2)
        self.assertEqual(self.host.disable_count,0);self.assertEqual(snap['state']['accounts']['fixture-account']['streak'],0)
    def test_http_errors_do_not_schedule_confirmation(self):
        self.configure_monitor();self.host.http_status=429;self.due();self.host.call('plugin.reconcile',stage='maintenance')
        state=self.task_state()['accounts']['fixture-account']
        self.assertEqual(state['total_runs'],1);self.assertIsNone(state['confirm_due_ms']);self.assertEqual(state['streak'],0)
    def test_cancelled_background_run_counts_once_and_stale_lease_resets_confirmation(self):
        self.configure_monitor();self.host.stall_stream=True
        self.due();call_id=self.host.next_id
        self.host.on_stream=lambda:self.host.write({'type':'cancel','id':call_id})
        self.assertEqual(self.host.call('plugin.reconcile',stage='maintenance')[0],'cancelled')
        state=self.task_state()['accounts']['fixture-account']
        self.assertEqual(state['total_runs'],1);self.assertIsNotNone(state['active'])
        self.host.on_stream=None;self.host.stall_stream=False
        self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(self.host.proxy.connects,1)
        self.task_state()['accounts']['fixture-account']['active']['started_ms']=0
        self.host.call('plugin.reconcile',stage='maintenance')
        self.assertIsNone(self.task_state()['accounts']['fixture-account']['active'])
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.monitor_snapshot()['total_runs'],2);self.assertEqual(self.host.disable_count,0)
    def test_probe_preserves_selected_model_with_host_identity(self):
        self.host.model_names=['gpt-6.1-sol','gpt-6-astra']
        for index,model in enumerate(self.host.model_names):
            status,result=self.run_request(f'fixture-selected-model-{index}','probe',model)
            self.assertEqual(status,200,result)
            self.assertEqual(result['record']['model'],model)
            for headers in self.host.http[index*2:index*2+2]:
                self.assertEqual(headers['x-codex-routing-hint'],'model='+model)
                self.assertEqual(headers['user-agent'],self.host.client_profile['userAgent'])
        self.assertEqual(self.host.proxy.connects,4)
    def test_probe_identity_is_frozen_per_run_and_reloaded_next_run(self):
        first_profile=dict(self.host.client_profile)
        def change_profile():
            self.host.client_profile={'originator':'codex_cli_rs','codexVersion':'0.155.0','userAgent':'codex_cli_rs/0.155.0 (Linux 6.8; x86_64) xterm-256color'}
        self.host.on_stream=change_profile
        self.assertEqual(self.run_request('fixture-profile-first','probe')[0],200)
        self.assertEqual(len(self.host.identity_snapshots),1)
        self.assertEqual(self.host.http[0]['user-agent'],first_profile['userAgent'])
        self.assertEqual(self.host.http[1]['user-agent'],first_profile['userAgent'])
        self.assertNotEqual(self.host.http[0]['session-id'],self.host.http[1]['session-id'])
        self.assertNotEqual(self.host.http[0]['x-client-request-id'],self.host.http[1]['x-client-request-id'])
        self.host.on_stream=None
        self.assertEqual(self.run_request('fixture-profile-next','probe')[0],200)
        self.assertEqual(len(self.host.identity_snapshots),2)
        self.assertEqual(self.host.http[2]['user-agent'],self.host.client_profile['userAgent'])
        self.assertEqual(self.host.http[3]['version'],'0.155.0')
        state=dump(self.host.states).decode()
        self.assertNotIn(first_profile['userAgent'],state)
        self.assertNotIn(self.host.client_profile['userAgent'],state)
    def test_unavailable_or_invalid_identity_stops_before_export_and_network(self):
        self.host.profile_error=True
        result=self.run_request('fixture-profile-unavailable','probe')[1]['record']
        self.assertEqual(result['status'],'inconclusive');self.assertIn('客户端身份读取失败',result['detail'])
        self.assertNotIn('fixture-profile-secret',dump(self.host.states).decode())
        self.host.profile_error=False
        for index,(field,value) in enumerate([('originator',''),('codexVersion',None),('userAgent','bad\r\nfixture-profile-secret')]):
            original=self.host.client_profile[field];self.host.client_profile[field]=value
            result=self.run_request(f'fixture-profile-invalid-{index}','probe')[1]['record']
            self.assertEqual(result['status'],'inconclusive')
            self.assertIn('客户端身份',result['detail'])
            self.host.client_profile[field]=original
        self.assertEqual(self.host.export_count,0);self.assertEqual(self.host.proxy.connects,0)
        self.assertNotIn('fixture-profile-secret',dump(self.host.states).decode())
        self.assertEqual(self.run_request('fixture-profile-recovered','probe')[0],200)
    def test_key_and_scope_failures_do_not_export_credentials(self):
        for field,value in [('key_enabled',False),('model_visible',False),('key_groups',['other-group'])]:
            with self.subTest(field=field):
                original=getattr(self.host,field);setattr(self.host,field,value)
                status,_=self.run_request('fixture-scope-'+field,'probe')
                self.assertEqual(status,400);self.assertEqual(self.host.export_count,0);self.assertEqual(self.host.proxy.connects,0)
                setattr(self.host,field,original)
        self.host.key_groups=['fixture-group'];self.host.group_enabled=False
        status,_=self.run_request('fixture-disabled-group','probe');self.assertEqual(status,400);self.assertEqual(self.host.export_count,0)
    def test_configured_proxy_missing_from_export_stops_before_network(self):
        self.host.proxy_configured=True;self.host.proxy_url=None
        result=self.run_request('fixture-empty-export-proxy','probe')[1]['record']
        self.assertEqual(result['status'],'inconclusive');self.assertIn('导出缺少代理',result['detail']);self.assertEqual(self.host.proxy.connects,0)
        self.assertNotIn('transport',result['metrics'])
    def test_no_proxy_account_is_allowed_for_monitor_configuration(self):
        self.host.proxy_configured=False;self.host.proxy_url=None
        task=self.configure_monitor(auto_disable=False)
        self.assertEqual(task['state']['settings']['account_ids'],['fixture-account'])
        self.assertEqual(self.host.export_count,0)
        self.assertFalse(self.host.call('GET','catalog')[1]['accounts'][0]['proxy_configured'])
    def test_no_proxy_manual_probe_reaches_direct_preparation_not_proxy_rejection(self):
        self.host.proxy_configured=False;self.host.proxy_url=None
        # 本地故意提供无效头，验证真实进程选择无代理路径后安全停止，绝不访问真实上游。
        self.host.export_token='invalid\ntoken'
        status,result=self.run_request('fixture-direct-preparation','probe')
        self.assertEqual(status,200,result);self.assertEqual(self.host.export_count,1)
        self.assertIn('访问令牌无法用于请求头',result['record']['detail'])
        self.assertEqual(self.host.proxy.connects,0)
        self.assertNotIn('invalid',dump(result['record']).decode())
    def test_no_proxy_scheduled_probe_reaches_preparation_and_counts_once(self):
        self.host.proxy_configured=False;self.host.proxy_url=None
        self.host.export_token='invalid\ntoken'
        self.configure_monitor(auto_disable=False);self.due()
        self.host.call('plugin.reconcile',stage='maintenance')
        record=self.host.states['history']['value']['records'][-1]
        self.assertEqual(record['metrics']['source'],'scheduled')
        self.assertIn('访问令牌无法用于请求头',record['detail'])
        self.assertEqual(self.monitor_snapshot()['total_runs'],1)
        self.assertEqual(self.host.export_count,1);self.assertEqual(self.host.proxy.connects,0)
    def test_export_only_selected_id_and_rejects_mismatch(self):
        self.host.export_id='other-account'
        r=self.run_request('fixture-export-mismatch','probe')[1]['record']
        self.assertEqual(r['status'],'inconclusive');self.assertEqual(self.host.proxy.connects,0)
    def test_old_monitor_pauses_and_resets_before_any_probe(self):
        config=self.configure_monitor()
        state=self.task_state();state.pop('probe_version');state['settings'].pop('client_key_id')
        state['accounts']['fixture-account']['streak']=1;state['accounts']['fixture-account']['next_due_ms']=0
        self.host.states['monitor']['value']=state
        self.host.call('plugin.reconcile',stage='maintenance')
        snapshot=self.monitor_snapshot()['state']
        self.assertTrue(snapshot['settings']['scheduled']);self.assertTrue(snapshot['settings']['auto_disable']);self.assertTrue(snapshot['paused'])
        self.assertIsNone(snapshot['settings']['client_key_id']);self.assertEqual(snapshot['accounts']['fixture-account']['streak'],0)
        self.assertEqual(self.host.export_count,0);self.assertEqual(len(self.host.http),0)
        self.assertEqual(snapshot['generation'],self.monitor_snapshot()['state']['generation'])
    def test_proxy_redirect_is_not_followed(self):
        self.host.http_status=302
        r=self.run_request('fixture-proxy-redirect','probe')[1]['record']
        self.assertEqual(r['status'],'inconclusive');self.assertEqual(self.host.proxy.connects,1)
    def test_account_model_policy_is_enforced(self):
        self.host.model_policy={'mode':'denylist','models':['fixture-model']}
        self.assertEqual(self.run_request('fixture-account-model','probe')[0],400);self.assertEqual(self.host.export_count,0)
    def test_probe_missing_key_is_rejected(self):
        status,_=self.host.call('POST','run',{'id':'fixture-missing-key','batch_id':'fixture-batch','account_id':'fixture-account','mode':'probe','model':'fixture-model','effort':'default','client_key_id':None,'question_id':None})
        self.assertEqual(status,400);self.assertEqual(self.host.export_count,0)
    def test_background_timeout_preserves_details_without_disabling(self):
        self.configure_monitor();self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.host.stall_stream=True;self.due(confirmation=True);started=time.monotonic()
        self.host.call('plugin.reconcile',stage='maintenance')
        elapsed=time.monotonic()-started;self.assertGreaterEqual(elapsed,179);self.assertLess(elapsed,195)
        record=self.host.states['history']['value']['records'][-1]
        self.assertEqual(record['status'],'inconclusive')
        self.assertEqual(record['metrics']['attempt_count'],3)
        self.assertEqual(record['metrics']['failure']['kind'],'timeout')
        self.assertGreaterEqual(record['metrics']['rounds'][0]['elapsed_ms'],59000)
        self.assertLessEqual(record['metrics']['rounds'][0]['elapsed_ms'],62000)
        self.assertEqual(record['metrics']['retry_stop'],'max_attempts')
        self.assertEqual(self.task_state()['accounts']['fixture-account']['streak'],0)
        self.assertEqual(self.host.disable_count,0)
        self.host.stall_stream=False
        status,result=self.run_request('fixture-after-timeout','probe')
        self.assertEqual(status,200,result)
        self.assertEqual(result['record']['status'],'degraded')
    def test_monitor_defaults_do_not_probe(self):
        self.assertEqual(self.host.call('plugin.reconcile',stage='maintenance'),{})
        self.assertEqual(len(self.host.http),0)
        self.assertEqual(self.host.disable_count,0)
    def test_background_two_degraded_disables_once(self):
        self.configure_monitor();self.due()
        self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.disable_count,0)
        s=self.task_state()['accounts']['fixture-account'];self.assertEqual(s['streak'],1)
        # 同一到期时间只消费一次，维护回调重入不重发。
        self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(len(self.host.http),2)
        self.due(confirmation=True);self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.disable_count,1);self.assertFalse(self.host.account_enabled)
        self.assertEqual(self.task_state()['accounts']['fixture-account']['action'],'disabled')
        self.due();self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(len(self.host.http),4)
        self.assertEqual(self.host.disable_count,1)
    def test_manual_probes_ignore_monitor_rules_and_never_disable(self):
        self.configure_monitor(daily_max=1)
        self.control(True)
        self.run_request('fixture-monitor-1','probe');self.run_request('fixture-monitor-2','probe')
        state=self.monitor_snapshot()
        self.assertEqual(state['total_runs'],0);self.assertEqual(state['daily_runs'],0)
        self.assertEqual(self.host.disable_count,0)
        self.host.http_status=429
        result=self.run_request('fixture-monitor-3','probe')[1]['record']
        self.assertEqual(result['status'],'inconclusive');self.assertEqual(result['metrics']['rounds'][0]['http_status'],429)
        self.assertNotIn('monitor',result['metrics'])
    def test_stale_settings_and_inflight_switch_off(self):
        config=self.configure_monitor();self.due();self.host.call('plugin.reconcile',stage='maintenance')
        status,_=self.save_monitor({'settings':config['state']['settings'],'expected_generation':None});self.assertEqual(status,400)
        def change():
            state=self.host.states['monitor'];state['value']['tasks'][0]['state']['generation']='different';state['value']['tasks'][0]['state']['settings']['auto_disable']=False;state['version']+=1
        self.host.on_stream=change
        self.due(confirmation=True);self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.disable_count,0)
    def test_disable_failure_keeps_unconfirmed_and_no_retry(self):
        self.configure_monitor();self.host.fail_disable=True
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.due(confirmation=True);self.host.call('plugin.reconcile',stage='maintenance')
        result=self.host.states['history']['value']['records'][-1]
        self.assertEqual(self.host.disable_count,1);self.assertEqual(result['metrics']['monitor']['action'],'unconfirmed')
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.due(confirmation=True);self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.disable_count,1)
    def test_safe_identity_and_preserved_history_name(self):
        self.host.account_notes='账号别称'
        self.assertEqual(self.host.call('GET','catalog')[1]['accounts'][0]['name'],'账号别称')
        record=self.run_request('fixture-name-1','probe')[1]['record'];self.assertEqual(record['account_name'],'账号别称')
        self.host.account_notes=None
        self.assertEqual(self.host.call('GET','catalog')[1]['accounts'][0]['name'],'fixture@example.test')
    def test_monitor_state_survives_process_restart(self):
        self.configure_monitor();self.due();self.host.call('plugin.reconcile',stage='maintenance')
        states=self.host.states
        self.host.close();self.host=Host();self.host.states=states
        self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(len(self.host.http),0)
        self.due(confirmation=True);self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(self.host.disable_count,1)
    def test_status_ticks_do_not_conflict_with_settings_edit(self):
        config=self.configure_monitor()
        self.host.call('plugin.reconcile',stage='maintenance')
        status,result=self.save_monitor({'settings':config['state']['settings'],'expected_generation':config['state']['generation']})
        self.assertEqual(status,200,result)
    def test_healthy_confirmation_resets_streak(self):
        self.configure_monitor();self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.host.ticket_second='ticket-first'
        self.due(confirmation=True);self.host.call('plugin.reconcile',stage='maintenance')
        state=self.task_state()['accounts']['fixture-account']
        self.assertEqual(state['streak'],0);self.assertIsNone(state['confirm_due_ms']);self.assertEqual(self.host.disable_count,0)
    def test_manually_restored_account_starts_new_cycle(self):
        self.configure_monitor();self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.due(confirmation=True);self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.disable_count,1)
        self.host.account_enabled=True
        self.due();self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(self.host.disable_count,1)
        self.due(confirmation=True);self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(self.host.disable_count,2)
    def test_probe_details_cover_both_rounds_without_secrets(self):
        r=self.run_request('fixture-details','probe')[1]['record']
        rounds=r['metrics']['rounds'];self.assertEqual(len(rounds),2)
        self.assertTrue(all(x['completed'] and x['http_status']==200 and x['ticket_length']>0 for x in rounds))
        self.assertTrue(all(x['response_bytes']>0 and x['routing_cookie_count']==1 for x in rounds))
        self.assertNotIn('ticket-first',dump(r).decode())
        self.assertNotIn('fixture-token-not-real',dump(r).decode())
    def test_v041_state_is_reused_without_reset_or_reexecution(self):
        import copy
        original=self.run_request('fixture-v041-history','probe')[1]['record']
        bank=[{'id':'custom_kept','title':'保留题库','category':'数学','prompt':'1加1','answer':'2'}]
        self.host.call('POST','questions',{'questions':bank,'expected_version':None})
        self.configure_monitor();self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.control(True,'fixture-account');self.control(True)
        old=copy.deepcopy(self.host.states)
        # v0.4.1与v0.5.0的namespace/schema/value结构保持一致，重启后只读取。
        self.host.close();self.host=Host();self.host.states=copy.deepcopy(old)
        history=self.host.call('GET','history')[1]
        self.assertTrue(any(r['id']==original['id'] for r in history['records']))
        self.assertTrue(any(q['id']=='custom_kept' for q in self.host.call('GET','catalog')[1]['questions']))
        snap=self.monitor_snapshot()
        self.assertEqual(snap['state']['generation'],old['monitor']['value']['tasks'][0]['state']['generation'])
        self.assertTrue(snap['state']['paused']);self.assertTrue(snap['state']['accounts']['fixture-account']['paused'])
        self.assertEqual(snap['total_runs'],1)
        self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.proxy.connects,0);self.assertEqual(self.host.export_count,0)
        self.assertEqual(self.host.states['history'],old['history'])
        self.assertEqual(self.host.states['bank'],old['bank'])
        self.assertEqual(self.task_state()['accounts']['fixture-account']['total_runs'],1)
    def test_v071_task_registry_survives_sdk_upgrade_without_extra_execution(self):
        import copy
        self.configure_monitor(auto_disable=False)
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.control(True)
        self.new_task('升级保留任务',scheduled=False,auto_disable=False)
        self.host.call('POST','questions',{'questions':[{'id':'custom_upgrade','title':'保留题目','category':'数学','prompt':'1+1','answer':'2'}],'expected_version':None})
        old=copy.deepcopy(self.host.states)
        self.host.close();self.host=Host();self.host.states=copy.deepcopy(old)
        snapshot=self.host.call('GET','monitor')[1]
        self.assertEqual(len(snapshot['tasks']),2)
        self.assertEqual(snapshot['total_runs'],1)
        self.assertTrue(snapshot['tasks'][0]['state']['paused'])
        self.assertFalse(snapshot['tasks'][1]['state']['settings']['scheduled'])
        self.assertEqual([t['id'] for t in snapshot['tasks']],[t['id'] for t in old['monitor']['value']['tasks']])
        self.assertEqual(self.host.states['monitor'],old['monitor'])
        self.assertEqual(self.host.states['history'],old['history'])
        self.assertEqual(self.host.states['bank'],old['bank'])
        self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.export_count,0);self.assertEqual(self.host.proxy.connects,0)
        self.assertEqual(self.host.disable_count,0)
    def test_protocol_seven_large_business_input_and_payload_remain_separate(self):
        bank=[{'id':f'custom_frame_{i}','title':'协议四','category':'数学','prompt':'x'*7999,'answer':'1'} for i in range(9)]
        status,result=self.host.call('POST','questions',{'questions':bank,'expected_version':None})
        self.assertEqual(status,400,result)
        self.assertIn('60KiB',result['error'])
        self.assertNotIn('bank',self.host.states)
        self.assertEqual(self.host.call('GET','history')[0],200)
    def test_registration_catalog_model_and_persistent_replay(self):
        with self.assertRaises(AssertionError):
            self.host.call('GET', '/catalog')
        reg=self.host.call('plugin.register',stage='registration'); self.assertIn('management',reg['contributes'])
        self.assertEqual(self.host.call('management.register',stage='registration')['pages'][0]['title'],'模型质量测试')
        status,catalog=self.host.call('GET','catalog'); self.assertEqual(status,200);self.assertEqual(len(catalog['questions']),10)
        self.assertEqual(self.host.call('POST','models',{'client_key_id':'fixture-key'})[1]['models'],['fixture-model'])
        status,answer=self.run_request('fixture-operation'); self.assertEqual(status,200); self.assertEqual(answer['record']['status'],'correct')
        self.assertEqual(self.run_request('fixture-operation')[1]['replayed'],True); self.assertEqual(self.host.model_count,1)
        self.assertEqual(self.host.call('GET','history')[1]['records'][0]['status'],'correct')
    def test_probe_ticket_cookie_and_no_secret_persistence(self):
        status,result=self.run_request('fixture-probe','probe'); self.assertEqual(status,200); self.assertEqual(result['record']['status'],'degraded')
        self.assertNotIn('x-codex-turn-state',self.host.http[0]); self.assertNotIn('cookie',self.host.http[0])
        self.assertEqual(self.host.http[1]['x-codex-turn-state'],'ticket-first'); self.assertEqual(self.host.http[1]['cookie'],'__oailb=route-fixture')
        state=dump(self.host.states).decode(); self.assertNotIn('ticket-first',state);self.assertNotIn('fixture-token-not-real',state);self.assertNotIn('route-fixture',state);self.assertNotIn('fixture-admin-not-real',state);self.assertNotIn('fixture-password',state);self.assertNotIn(self.host.proxy.url,state)
        self.assertEqual(self.host.proxy.connects,2)
        self.assertEqual(result['record']['metrics']['client_key_id'],'fixture-key');self.assertFalse(result['record']['metrics']['key_billed'])
    def test_failed_stream_is_inconclusive(self):
        self.host.bad_stream=True
        record=self.run_request('fixture-failed','probe')[1]['record']
        self.assertEqual(record['status'],'inconclusive')
        self.assertEqual(record['metrics']['failure']['kind'],'stream_error')
        self.assertEqual(record['metrics']['attempt_count'],3)
        self.assertEqual(len(self.host.http),3)
    def test_parent_cancel_releases_probe_account_without_replay(self):
        # 在上游请求已抵达本机代理时取消真实父调用，不能用改写历史代替取消验证。
        self.host.stall_stream=True
        call_id=self.host.next_id
        def cancel_parent():
            self.host.write({'type':'cancel','id':call_id})
        self.host.on_stream=cancel_parent
        self.assertEqual(self.run_request('fixture-parent-cancel','probe')[0],'cancelled')
        self.host.on_stream=None;self.host.stall_stream=False
        self.assertEqual(self.run_request('fixture-after-parent-cancel','probe')[0],200)
        self.assertEqual(self.host.proxy.connects,3)
        prior=self.host.states['history']['value']['records'][0]
        self.assertEqual(prior['status'],'running')
        self.assertTrue(self.run_request('fixture-parent-cancel','probe')[1]['replayed'])
        self.assertEqual(self.host.proxy.connects,3)
    def test_detail_and_response_errors_are_classified_without_secret_persistence(self):
        self.host.http_status=400
        message="The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account fixture-token-not-real"
        for index,body in enumerate([
            {'detail':message},
            {'detail':{'message':message}},
            {'error':message},
            {'response':{'error':{'message':message}}},
            {'message':message},
            {'error':None,'detail':message},
        ]):
            with self.subTest(body=body):
                self.host.error_body=body
                status,result=self.run_request(f'fixture-envelope-{index}','probe')
                self.assertEqual(status,200,result)
                record=result['record']
                self.assertEqual(record['status'],'inconclusive')
                self.assertIn('上游不支持该模型',record['detail'])
                self.assertIn('HTTP 400',record['detail'])
                self.assertEqual(record['metrics']['rounds'][0]['error'],record['detail'])
                self.assertGreater(record['metrics']['rounds'][0]['response_bytes'],0)
                self.assertNotIn(message,dump(self.host.states).decode())
                self.assertNotIn('fixture-token-not-real',dump(self.host.states).decode())
        self.assertEqual(self.host.proxy.connects,6)
    def test_rejection_reason_is_classified_without_echoing_secrets(self):
        self.host.http_status=400
        self.host.error_body={'error':{'code':'model_not_found','message':'fixture-token-not-real model does not exist'}}
        r=self.run_request('fixture-model-rejected','probe')[1]['record']
        self.assertIn('上游不支持该模型',r['detail']);self.assertGreater(r['metrics']['rounds'][0]['response_bytes'],0)
        self.assertNotIn('fixture-token-not-real',dump(r).decode())
        self.host.error_body={'error':{'code':'unsupported_parameter','param':'parallel_tool_calls','message':'secret fixture-password'}}
        r=self.run_request('fixture-param-rejected','probe')[1]['record']
        self.assertIn('上游不支持探针请求中的参数',r['detail'])
        self.assertNotIn('fixture-password',dump(self.host.states).decode())
    def test_rejected_probe_releases_account_for_next_operation(self):
        self.host.http_status=400
        first=self.run_request('fixture-rejected-1','probe')[1]['record']
        self.assertEqual(first['status'],'inconclusive')
        self.assertIn('HTTP 400',first['detail'])
        status,replayed=self.run_request('fixture-rejected-1','probe')
        self.assertEqual(status,200);self.assertTrue(replayed['replayed'])
        self.assertEqual(self.host.proxy.connects,1)
        self.host.http_status=200
        status,result=self.run_request('fixture-after-rejected','probe')
        self.assertEqual(status,200,result)
        self.assertEqual(result['record']['status'],'degraded')
        self.assertEqual(self.host.proxy.connects,3)
    def test_failed_stream_releases_account_for_next_operation(self):
        self.host.bad_stream=True
        self.assertEqual(self.run_request('fixture-stream-failed','probe')[1]['record']['status'],'inconclusive')
        self.host.bad_stream=False
        self.assertEqual(self.run_request('fixture-after-stream','probe')[1]['record']['status'],'degraded')
    def test_unconfirmed_question_history_does_not_block_new_operation(self):
        self.run_request('fixture-prior-question')
        prior=self.host.states['history']['value']['records'][-1]
        prior['status']='running';prior['finished_at_ms']=None
        status,result=self.run_request('fixture-after-question')
        self.assertEqual(status,200,result);self.assertEqual(result['record']['status'],'correct')
        self.assertEqual(prior['status'],'running');self.assertIsNone(prior['finished_at_ms'])
        self.assertEqual(self.run_request('fixture-after-question-probe','probe')[0],200)
    def test_interrupted_local_probe_history_does_not_block_new_operation(self):
        self.run_request('fixture-prior-probe','probe')
        prior=self.host.states['history']['value']['records'][-1]
        prior['status']='running';prior['finished_at_ms']=None
        self.assertEqual(self.run_request('fixture-after-interrupted','probe')[0],200)
    def test_transient_probe_errors_retry_twice_then_succeed_without_reusing_ticket(self):
        self.host.http_responses=[{}, {'status':503}, {'status':429,'error':{'error':{'code':'rate_limit_exceeded'}},'headers':{'Retry-After':'0'}}]
        record=self.run_request('fixture-probe-retry-success','probe')[1]['record']
        self.assertEqual(record['status'],'degraded')
        self.assertEqual(record['metrics']['attempt_count'],3)
        self.assertNotIn('failure',record['metrics'])
        self.assertEqual(self.host.export_count,1);self.assertEqual(len(self.host.identity_snapshots),1)
        self.assertEqual([r['attempt'] for r in record['metrics']['rounds']],[1,1,2,3,3])
        self.assertNotIn('x-codex-turn-state',self.host.http[2]);self.assertNotIn('x-codex-turn-state',self.host.http[3])
        self.assertNotIn('cookie',self.host.http[2]);self.assertNotIn('cookie',self.host.http[3])
    def test_retriable_failure_stops_after_three_attempts_with_exact_http_status(self):
        self.host.http_status=503
        record=self.run_request('fixture-probe-retry-limit','probe')[1]['record']
        self.assertEqual(record['status'],'inconclusive');self.assertEqual(record['metrics']['attempt_count'],3)
        self.assertEqual(record['metrics']['retry_stop'],'max_attempts')
        self.assertEqual(record['metrics']['failure']['http_status'],503)
        self.assertIn('HTTP 503',record['detail']);self.assertNotIn('无法判断',record['detail'])
        self.assertEqual(self.host.proxy.connects,3)
    def test_nonretriable_errors_do_not_repeat_or_expose_upstream_secrets(self):
        for index,(status,code,kind) in enumerate([(401,'invalid_token','authentication'),(403,'fixture_unknown','permission_denied'),(400,'unsupported_parameter','invalid_request'),(400,'model_not_found','model_unavailable'),(429,'insufficient_quota','quota_exhausted')]):
            self.host.http_status=status;self.host.error_body={'error':{'code':code,'message':'fixture-token-not-real fixture-password'}}
            before=self.host.proxy.connects
            record=self.run_request(f'fixture-probe-no-retry-{index}','probe')[1]['record']
            self.assertEqual(record['metrics']['failure']['kind'],kind)
            self.assertEqual(record['metrics']['failure']['http_status'],status)
            self.assertEqual(record['metrics']['attempt_count'],1)
            self.assertEqual(record['metrics']['retry_stop'],'not_retryable')
            self.assertEqual(self.host.proxy.connects,before+1)
            self.assertNotIn('fixture-password',dump(record).decode());self.assertNotIn('fixture-token-not-real',dump(record).decode())
    def test_retry_after_exceeding_budget_is_not_ignored(self):
        self.host.http_responses=[{'status':429,'headers':{'Retry-After':'120'},'error':{'error':{'code':'rate_limit_exceeded'}}}]
        record=self.run_request('fixture-probe-long-retry-after','probe')[1]['record']
        self.assertEqual(record['metrics']['attempt_count'],1)
        self.assertEqual(record['metrics']['retry_stop'],'retry_after_exceeds_budget')
        self.assertEqual(record['metrics']['failure']['retry_after_ms'],120000)
        self.assertEqual(record['metrics']['failure']['http_status'],429)
    def test_http_429_body_timeout_keeps_retry_after_and_does_not_retry_early(self):
        self.configure_monitor(auto_disable=True)
        self.host.http_responses=[{'status':429,'headers':{'Retry-After':'240'},'stall_body':True}]
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        record=self.host.states['history']['value']['records'][-1]
        self.assertEqual(record['metrics']['attempt_count'],1)
        self.assertEqual(record['metrics']['failure']['http_status'],429)
        self.assertEqual(record['metrics']['failure']['retry_after_ms'],240000)
        self.assertEqual(record['metrics']['retry_stop'],'retry_after_exceeds_budget')
        self.assertEqual(self.host.proxy.connects,1);self.assertEqual(self.host.disable_count,0)
    def test_retry_after_wait_is_observed_before_success(self):
        self.host.http_responses=[{'status':429,'headers':{'Retry-After':'1'}}]
        start=time.monotonic()
        record=self.run_request('fixture-probe-wait-retry-after','probe')[1]['record']
        self.assertGreaterEqual(time.monotonic()-start,1)
        self.assertEqual(record['metrics']['attempt_count'],2);self.assertEqual(record['status'],'degraded')
    def test_background_timeout_retries_then_succeeds_and_counts_run_once(self):
        self.configure_monitor(auto_disable=True,daily_max=1)
        self.host.http_responses=[{'stall':True}]
        self.due();start=time.monotonic();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertGreaterEqual(time.monotonic()-start,60);self.assertLess(time.monotonic()-start,80)
        record=self.host.states['history']['value']['records'][-1]
        self.assertEqual(record['status'],'degraded');self.assertEqual(record['metrics']['attempt_count'],2)
        self.assertEqual(record['metrics']['attempts'][0]['failure']['kind'],'timeout')
        self.assertEqual(self.monitor_snapshot()['daily_runs'],1);self.assertEqual(self.host.disable_count,0)
    def test_background_slow_success_is_not_cut_off_by_old_six_second_attempt(self):
        self.configure_monitor(auto_disable=False,daily_max=1)
        self.host.http_responses=[{'delay':7},{}]
        self.due();start=time.monotonic();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertGreaterEqual(time.monotonic()-start,7)
        record=self.host.states['history']['value']['records'][-1]
        self.assertEqual(record['status'],'degraded');self.assertEqual(record['metrics']['attempt_count'],1)
        self.assertEqual(self.monitor_snapshot()['daily_runs'],1);self.assertEqual(self.host.disable_count,0)
    def test_background_retry_failure_is_projected_without_persisting_duplicate_result(self):
        self.configure_monitor(auto_disable=True)
        self.host.http_status=503;self.due();self.host.call('plugin.reconcile',stage='maintenance')
        snapshot=self.monitor_snapshot();status=snapshot['state']['accounts']['fixture-account']
        self.assertEqual(status['last_result']['metrics']['failure']['http_status'],503)
        self.assertEqual(status['last_result']['metrics']['attempt_count'],3)
        self.assertEqual(snapshot['daily_runs'],1);self.assertEqual(self.host.disable_count,0)
        self.assertNotIn('last_result',self.task_state()['accounts']['fixture-account'])
        self.host.states['history']['value']['records']=[]
        self.assertNotIn('last_result',self.monitor_snapshot()['state']['accounts']['fixture-account'])
    def test_sse_specific_errors_keep_reason_and_retry_policy(self):
        self.host.http_responses=[{'body':b'data: {"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded"}}}\n\n'}]
        record=self.run_request('fixture-probe-sse-retry','probe')[1]['record']
        self.assertEqual(record['status'],'degraded');self.assertEqual(record['metrics']['attempt_count'],2)
        self.host.http_responses=[{'body':b'data: {"type":"response.failed","response":{"error":{"code":"invalid_token"}}}\n\n'}]
        record=self.run_request('fixture-probe-sse-auth','probe')[1]['record']
        self.assertEqual(record['metrics']['failure']['kind'],'authentication');self.assertEqual(record['metrics']['attempt_count'],1)
    def test_cancelling_retry_wait_does_not_send_another_attempt(self):
        call_id=self.host.next_id
        self.host.http_responses=[{'status':429,'headers':{'Retry-After':'3'}}]
        timers=[]
        def cancel_during_wait():
            timer=threading.Timer(0.15,lambda:self.host.write({'type':'cancel','id':call_id}))
            timers.append(timer);timer.start()
        self.host.on_stream=cancel_during_wait
        self.assertEqual(self.run_request('fixture-probe-cancel-wait','probe')[0],'cancelled')
        self.host.on_stream=None
        for timer in timers:timer.join()
        self.assertEqual(self.host.proxy.connects,1)
        status,result=self.run_request('fixture-probe-after-cancel-wait','probe')
        self.assertEqual(status,200,result);self.assertEqual(result['record']['metrics']['attempt_count'],1)
        self.assertEqual(self.host.proxy.connects,3)
    def test_missing_ticket_retries_without_fabricating_a_success(self):
        self.host.http_responses=[{'ticket':''} for _ in range(3)]
        record=self.run_request('fixture-probe-no-ticket','probe')[1]['record']
        self.assertEqual(record['metrics']['failure']['kind'],'missing_ticket');self.assertEqual(record['metrics']['attempt_count'],3)
        self.assertEqual(record['status'],'inconclusive')
    def test_custom_bank_compare_and_swap(self):
        q={'id':'custom_sum','title':'加法','category':'数学','prompt':'17+28，只输出整数','answer':'45'}
        self.assertEqual(self.host.call('POST','questions',{'questions':[q],'expected_version':None})[0],200)
        self.assertEqual(self.host.call('POST','questions',{'questions':[q],'expected_version':None})[0],409)
        self.assertEqual(len(self.host.call('GET','catalog')[1]['questions']),11)

if __name__=='__main__': unittest.main(verbosity=2)
