"""仅本机的认证 CONNECT 代理与 TLS 上游桩；不解析或连接真实 ChatGPT。"""
import base64
import datetime
import json
import pathlib
import socketserver
import ssl
import tempfile
import threading
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID

class ProxyFixture:
    def __init__(self, owner):
        self.owner = owner
        self.requests = []
        self.connects = 0
        self.errors = []
        self.stop = threading.Event()
        self.temp = tempfile.TemporaryDirectory(prefix='quality-proxy-test-')
        directory = pathlib.Path(self.temp.name)
        now = datetime.datetime.now(datetime.timezone.utc)
        key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, 'Quality Test Ephemeral CA')])
        ca = x509.CertificateBuilder().subject_name(ca_name).issuer_name(ca_name).public_key(key.public_key()).serial_number(x509.random_serial_number()).not_valid_before(now-datetime.timedelta(minutes=1)).not_valid_after(now+datetime.timedelta(days=1)).add_extension(x509.BasicConstraints(ca=True,path_length=None),critical=True).sign(key,hashes.SHA256())
        leaf_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        leaf_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME,'chatgpt.com')])
        leaf = x509.CertificateBuilder().subject_name(leaf_name).issuer_name(ca_name).public_key(leaf_key.public_key()).serial_number(x509.random_serial_number()).not_valid_before(now-datetime.timedelta(minutes=1)).not_valid_after(now+datetime.timedelta(days=1)).add_extension(x509.BasicConstraints(ca=False,path_length=None),critical=True).add_extension(x509.SubjectAlternativeName([x509.DNSName('chatgpt.com')]),critical=False).sign(key,hashes.SHA256())
        self.ca_path = directory/'ca.pem'
        self.ca_path.write_bytes(ca.public_bytes(serialization.Encoding.PEM))
        cert_path = directory/'leaf.pem'; cert_path.write_bytes(leaf.public_bytes(serialization.Encoding.PEM))
        key_path = directory/'leaf.key'; key_path.write_bytes(leaf_key.private_bytes(serialization.Encoding.PEM,serialization.PrivateFormat.PKCS8,serialization.NoEncryption()))
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);context.load_cert_chain(cert_path,key_path)
        fixture = self
        class Server(socketserver.ThreadingTCPServer):
            allow_reuse_address = True
            daemon_threads = True
        class Handler(socketserver.BaseRequestHandler):
            def handle(self):
                try:
                    self.request.settimeout(5)
                    connect = b''
                    while not connect.endswith(b'\r\n\r\n'):
                        chunk=self.request.recv(1)
                        if not chunk: return
                        connect+=chunk
                        if len(connect)>8192:raise AssertionError('CONNECT headers too large')
                    assert connect.startswith(b'CONNECT chatgpt.com:443 HTTP/1.1\r\n')
                    expected=base64.b64encode(b'fixture-user:fixture-password')
                    connect_headers={line.split(b':',1)[0].lower():line.split(b':',1)[1].strip() for line in connect.split(b'\r\n')[1:] if b':' in line}
                    assert connect_headers.get(b'proxy-authorization')==b'Basic '+expected
                    fixture.connects+=1
                    self.request.sendall(b'HTTP/1.1 200 Connection Established\r\n\r\n')
                    with context.wrap_socket(self.request,server_side=True) as stream:
                        reader=stream.makefile('rb')
                        assert reader.readline()==b'POST /backend-api/codex/responses HTTP/1.1\r\n'
                        headers={}
                        while True:
                            line=reader.readline()
                            if line==b'\r\n':break
                            name,value=line.decode().split(':',1);headers[name.lower()]=value.strip()
                        body=json.loads(reader.read(int(headers['content-length'])))
                        assert body['stream'] is True
                        assert headers['authorization']=='Bearer fixture-token-not-real'
                        assert headers['chatgpt-account-id']=='fixture-upstream'
                        fixture.requests.append(headers)
                        if owner.on_stream:owner.on_stream()
                        if owner.stall_stream:
                            fixture.stop.wait(28);return
                        if owner.bad_stream:body=b'data: {"type":"response.failed"}\n\n'
                        else:body=b'data: {"type":"response.completed","response":{"status":"completed","model":"fixture-model"}}\n\n'
                        ticket='ticket-first' if 'x-codex-turn-state' not in headers else owner.ticket_second
                        response=f'HTTP/1.1 {owner.http_status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {len(body)}\r\nx-codex-turn-state: {ticket}\r\nSet-Cookie: __oailb=route-fixture; Secure; HttpOnly\r\nSet-Cookie: other=not-forwarded; Secure\r\nConnection: close\r\n\r\n'.encode()+body
                        stream.sendall(response);reader.close()
                except Exception as error:
                    if not fixture.stop.is_set():fixture.errors.append(type(error).__name__)
        self.server=Server(('127.0.0.1',0),Handler)
        self.url=f'http://fixture-user:fixture-password@127.0.0.1:{self.server.server_address[1]}'
        threading.Thread(target=self.server.serve_forever,daemon=True).start()
    def close(self):
        self.stop.set();self.server.shutdown();self.server.server_close();self.temp.cleanup()
