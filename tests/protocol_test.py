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
        self.bad_stream = False
        self.model_count = 0
        self.account_enabled = True
        self.account_notes = '测试账号'
        self.disable_count = 0
        self.dispatch_status = 200
        self.http_status = 200
        self.error_body = None
        self.fail_disable = False
        self.body_streams = {}
        self.on_stream = None
        self.stall_stream = False
        self.write({'type':'hello','handshake':{'protocol_version':2,'artifact_sha256':'0'*64,'plugin_id':'xunzhimeng.model-quality-test','instance_id':'fixture','generation':1,'incarnation':'fixture-1','configuration':{},'contributes':{'maintenance':{'id':'xunzhimeng.model-quality-test.maintenance','version':1,'stages':['maintenance']},'management':{'id':'xunzhimeng.model-quality-test.management','version':1,'stages':['management']}}}})
        threading.Thread(target=self.reader, daemon=True).start()
        message,_ = self.messages.get(timeout=5)
        assert message['type'] == 'ready' and message['protocol_version'] == 2, message
        self.next_id = 11
        self.registration = self.call('management.register', stage='registration')
        # 先检查相对路由，再按注册表分发，避免桩直接接受宿主会拒绝的路径。
        self.routes = {(route['method'], route['path']) for route in self.registration['routes']}
        assert len(self.routes) == len(self.registration['routes'])
        assert all(path and not path.startswith('/') for _, path in self.routes)
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
        account={'account_id':'fixture-account','provider_id':'openai','credential_revision':1,'name':'OpenAI 账号','email':'fixture@example.test','upstream_user_id':None,'upstream_account_id':'fixture-upstream','plan_type':'plus','authentication_kind':'oauth','enabled':self.account_enabled,'credential_state':'ready','has_refresh_token':False,'access_token_expires_at_ms':None,'next_refresh_at_ms':None}
        if method.startswith('host.auth.'):
            assert params=={}; request=json.loads(body)
            if method=='host.auth.list': payload=dump({'accounts':[account],'next_cursor':None})
            elif method=='host.auth.get_runtime': payload=dump(account)
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
            assert len(dump(params['value']))<262144
            version=(old['version']+1) if old else 1
            self.states[key]={'value':params['value'],'version':version,'schema_version':1}; result={'version':version}
        elif method=='host.model.execute':
            assert params['client_key_id']=='fixture-key' and params['account_id']=='fixture-account'
            assert json.loads(body)['reasoning']['effort']=='medium'
            assert self.states['history']['value']['records'][-1]['status']=='running'
            self.model_count+=1
            result={'request_id':'fixture-request','events':1}; payload=events()
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
                data={'documents':[{'provider':'openai','document':{'accounts':[{'id':self.export_id,'accessToken':'fixture-token-not-real','accountId':'fixture-upstream','outboundProxyUrl':self.proxy_url}]}}]}
            elif uri.path=='/api/admin/accounts/batch-update':
                assert params['method']=='POST'
                assert json.loads(body)=={'accountIds':['fixture-account'],'enabled':False}
                assert self.states['monitor']['value']['accounts']['fixture-account']['action']=='pending'
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
        params={} if path is None else {'method':method,'path':path,'query':'','content_type':'application/json' if body is not None else None,'headers':[{'name':'authorization','value':list(b'fixture-admin-not-real')}]}
        self.write({'type':'call','id':id,'method':method if path is None else 'management.handle','context':{'call_id':id,'instance_id':'fixture','generation':1,'incarnation':'fixture-1','stage':stage,'timeout_ms':30000 if stage=='maintenance' else 120000,'resource_stream':False,'resource_scope_id':str(id)},'params':params},dump(body) if body is not None else b'')
        while True:
            msg,payload=self.messages.get(timeout=35)
            if msg['type']=='cancel': continue
            if msg['type']=='cancelled':
                assert msg['id']==id,msg
                return 'cancelled',{}
            if msg['type']=='callback': self.callback(msg,payload); continue
            assert msg['type']=='result', msg
            assert msg['id']==id, msg
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
    def configure_monitor(self,scheduled=True,auto_disable=True):
        snapshot=self.host.call('GET','monitor')[1]
        status,result=self.host.call('POST','monitor',{'settings':{'scheduled':scheduled,'auto_disable':auto_disable,'interval_minutes':30,'model':'fixture-model','client_key_id':'fixture-key','account_ids':['fixture-account']},'expected_generation':snapshot['state']['generation'] or None})
        self.assertEqual(status,200,result)
        return result
    def due(self):
        state=self.host.states['monitor']['value']
        state['accounts']['fixture-account']['next_due_ms']=0
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
    def test_missing_proxy_and_export_mismatch_stop_before_network(self):
        self.host.proxy_configured=False
        status,_=self.run_request('fixture-no-proxy','probe');self.assertEqual(status,400);self.assertEqual(self.host.export_count,0)
        self.host.proxy_configured=True;self.host.proxy_url=None
        result=self.run_request('fixture-empty-export-proxy','probe')[1]['record']
        self.assertEqual(result['status'],'inconclusive');self.assertIn('禁止直连',result['detail']);self.assertEqual(self.host.proxy.connects,0)
    def test_export_only_selected_id_and_rejects_mismatch(self):
        self.host.export_id='other-account'
        r=self.run_request('fixture-export-mismatch','probe')[1]['record']
        self.assertEqual(r['status'],'inconclusive');self.assertEqual(self.host.proxy.connects,0)
    def test_old_monitor_pauses_and_resets_before_any_probe(self):
        config=self.configure_monitor()
        state=self.host.states['monitor']['value'];state.pop('probe_version');state['settings'].pop('client_key_id')
        state['accounts']['fixture-account']['streak']=1;state['accounts']['fixture-account']['next_due_ms']=0
        self.host.call('plugin.reconcile',stage='maintenance')
        snapshot=self.host.call('GET','monitor')[1]['state']
        self.assertFalse(snapshot['settings']['scheduled']);self.assertFalse(snapshot['settings']['auto_disable'])
        self.assertIsNone(snapshot['settings']['client_key_id']);self.assertEqual(snapshot['accounts']['fixture-account']['streak'],0)
        self.assertEqual(self.host.export_count,0);self.assertEqual(len(self.host.http),0)
        self.assertEqual(snapshot['generation'],self.host.call('GET','monitor')[1]['state']['generation'])
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
        self.host.stall_stream=True;self.due();started=time.monotonic()
        self.host.call('plugin.reconcile',stage='maintenance')
        elapsed=time.monotonic()-started;self.assertGreaterEqual(elapsed,17);self.assertLess(elapsed,29)
        record=self.host.states['history']['value']['records'][-1]
        self.assertEqual(record['status'],'inconclusive');self.assertGreaterEqual(record['metrics']['rounds'][0]['elapsed_ms'],17000)
        self.assertEqual(self.host.states['monitor']['value']['accounts']['fixture-account']['streak'],0)
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
        s=self.host.states['monitor']['value']['accounts']['fixture-account'];self.assertEqual(s['streak'],1)
        # 同一到期时间只消费一次，维护回调重入不重发。
        self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(len(self.host.http),2)
        self.due();self.host.call('plugin.reconcile',stage='maintenance')
        self.assertEqual(self.host.disable_count,1);self.assertFalse(self.host.account_enabled)
        self.assertEqual(self.host.states['monitor']['value']['accounts']['fixture-account']['action'],'disabled')
        self.due();self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(len(self.host.http),4)
        self.assertEqual(self.host.disable_count,1)
    def test_manual_opt_in_and_failure_does_not_disable(self):
        self.configure_monitor(scheduled=False)
        self.run_request('fixture-monitor-1','probe');self.assertEqual(self.host.disable_count,0)
        self.host.http_status=429
        result=self.run_request('fixture-monitor-2','probe')[1]['record']
        self.assertEqual(result['status'],'inconclusive');self.assertEqual(result['metrics']['rounds'][0]['http_status'],429)
        self.assertEqual(self.host.states['monitor']['value']['accounts']['fixture-account']['streak'],0)
        self.assertEqual(self.host.disable_count,0)
    def test_stale_settings_and_inflight_switch_off(self):
        config=self.configure_monitor()
        status,_=self.host.call('POST','monitor',{'settings':config['state']['settings'],'expected_generation':None});self.assertEqual(status,400)
        self.run_request('fixture-toggle-1','probe')
        def change():
            state=self.host.states['monitor'];state['value']['generation']='different';state['value']['settings']['auto_disable']=False;state['version']+=1
        self.host.on_stream=change
        self.run_request('fixture-toggle-2','probe');self.assertEqual(self.host.disable_count,0)
    def test_disable_failure_keeps_unconfirmed_and_no_retry(self):
        self.configure_monitor(scheduled=False);self.host.fail_disable=True
        self.run_request('fixture-action-1','probe');result=self.run_request('fixture-action-2','probe')[1]['record']
        self.assertEqual(self.host.disable_count,1);self.assertEqual(result['metrics']['monitor']['action'],'unconfirmed')
        self.run_request('fixture-action-3','probe');self.assertEqual(self.host.disable_count,1)
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
        self.due();self.host.call('plugin.reconcile',stage='maintenance');self.assertEqual(self.host.disable_count,1)
    def test_status_ticks_do_not_conflict_with_settings_edit(self):
        config=self.configure_monitor()
        self.host.call('plugin.reconcile',stage='maintenance')
        status,result=self.host.call('POST','monitor',{'settings':config['state']['settings'],'expected_generation':config['state']['generation']})
        self.assertEqual(status,200,result)
    def test_healthy_and_interrupted_round_reset_streak(self):
        self.configure_monitor(scheduled=False)
        self.run_request('fixture-streak-1','probe')
        self.host.ticket_second='ticket-first'
        self.run_request('fixture-streak-2','probe')
        self.assertEqual(self.host.states['monitor']['value']['accounts']['fixture-account']['streak'],0)
        self.host.ticket_second='ticket-new'
        self.run_request('fixture-streak-3','probe')
        self.host.states['monitor']['value']['accounts']['fixture-account']['last_status']='running'
        self.run_request('fixture-streak-4','probe')
        self.assertEqual(self.host.disable_count,0)
        self.assertEqual(self.host.states['monitor']['value']['accounts']['fixture-account']['streak'],1)
    def test_manually_restored_account_starts_new_cycle(self):
        self.configure_monitor(scheduled=False)
        self.run_request('fixture-restore-1','probe');self.run_request('fixture-restore-2','probe')
        self.assertEqual(self.host.disable_count,1)
        self.host.account_enabled=True
        self.run_request('fixture-restore-3','probe');self.assertEqual(self.host.disable_count,1)
        self.run_request('fixture-restore-4','probe');self.assertEqual(self.host.disable_count,2)
    def test_probe_details_cover_both_rounds_without_secrets(self):
        r=self.run_request('fixture-details','probe')[1]['record']
        rounds=r['metrics']['rounds'];self.assertEqual(len(rounds),2)
        self.assertTrue(all(x['completed'] and x['http_status']==200 and x['ticket_length']>0 for x in rounds))
        self.assertTrue(all(x['response_bytes']>0 and x['routing_cookie_count']==1 for x in rounds))
        self.assertNotIn('ticket-first',dump(r).decode())
        self.assertNotIn('fixture-token-not-real',dump(r).decode())
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
        self.assertEqual(self.run_request('fixture-failed','probe')[1]['record']['status'],'inconclusive')
        self.assertEqual(len(self.host.http),1)
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
    def test_uncertain_host_question_still_keeps_isolation(self):
        self.run_request('fixture-prior-question')
        prior=self.host.states['history']['value']['records'][-1]
        prior['status']='running';prior['finished_at_ms']=None
        self.assertEqual(self.run_request('fixture-after-question','probe')[0],400)
    def test_interrupted_local_probe_history_does_not_block_new_operation(self):
        self.run_request('fixture-prior-probe','probe')
        prior=self.host.states['history']['value']['records'][-1]
        prior['status']='running';prior['finished_at_ms']=None
        self.assertEqual(self.run_request('fixture-after-interrupted','probe')[0],200)
    def test_custom_bank_compare_and_swap(self):
        q={'id':'custom_sum','title':'加法','category':'数学','prompt':'17+28，只输出整数','answer':'45'}
        self.assertEqual(self.host.call('POST','questions',{'questions':[q],'expected_version':None})[0],200)
        self.assertEqual(self.host.call('POST','questions',{'questions':[q],'expected_version':None})[0],409)
        self.assertEqual(len(self.host.call('GET','catalog')[1]['questions']),11)

if __name__=='__main__': unittest.main(verbosity=2)
