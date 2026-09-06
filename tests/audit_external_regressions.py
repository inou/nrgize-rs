#!/usr/bin/env python3
"""Supplemental local checks with real age and Caddy binaries supplied by the auditor.
Usage: python3 external_checks.py /absolute/repo /absolute/test-tools/bin
Only temporary files and loopback ports are used; no production services are contacted.
"""
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

repo,tools=map(lambda x:Path(x).resolve(),sys.argv[1:3])
base=Path(tempfile.mkdtemp(prefix='nrg-audit-external-'))
env=dict(os.environ,PATH=str(tools)+':'+os.environ['PATH'])
nrg=repo/'target/debug/nrg'
results=[]
def record(name,**evidence):
    results.append(dict(name=name,**evidence)); print(json.dumps(results[-1]),flush=True)

# Real age streaming crypto: corrupt the final authenticated chunk after earlier chunks passed.
p=base/'age'; p.mkdir()
subprocess.run([nrg,'secrets','init'],cwd=p,env=env,capture_output=True,check=True)
recipient=(p/'.nrg-key.pub').read_text().strip()
plaintext=b'AUDIT_ONLY_PASSWORD='+b'x'*200000
encrypted=subprocess.run([tools/'age','-r',recipient],input=plaintext,capture_output=True,check=True).stdout
corrupt=bytearray(encrypted); corrupt[-1]^=1
(p/'secrets.env.enc').write_bytes(corrupt)
r=subprocess.run([nrg,'secrets','unseal','secrets.env.enc'],cwd=p,env=env,capture_output=True,umask=0o022)
output=p/'secrets.env'
assert r.returncode!=0 and not output.exists()
record('failed_unseal_publishes_no_plaintext',exit=r.returncode)
(p/'unseal.stderr').write_bytes(r.stderr)

# The wrapper writes all stdin before draining age's stdout; a larger value deadlocks.
start=time.monotonic()
control=subprocess.run([tools/'age','-r',recipient,'-a'],input=b'x'*1048576,capture_output=True,timeout=5,check=True)
control_elapsed=time.monotonic()-start
c=subprocess.Popen([nrg,'secrets','encrypt'],cwd=p,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
blocked=False
try: encrypted_token,_=c.communicate(b'x'*1048576,timeout=5)
except subprocess.TimeoutExpired:
    blocked=True; os.killpg(c.pid,signal.SIGKILL); c.communicate()
assert not blocked and c.returncode == 0
decrypted=subprocess.run([nrg,'secrets','decrypt'],cwd=p,env=env,input=encrypted_token,capture_output=True,timeout=5,check=True)
assert decrypted.stdout.rstrip(b'\n') == b'x'*1048576
record('large_age_value_completes',bytes=1048576,nrg_timeout_seconds=5,direct_age_elapsed_seconds=round(control_elapsed,3))

# Caddy route/listener shape from lib/caddy.rhai. Only ports, storage, admin, and certificate
# acquisition are changed for isolation. Disabling certificate acquisition does NOT disable
# Caddy's auto-HTTPS redirect logic. The reverse-proxy route remains exactly the stdlib shape.
def port():
    with socket.socket() as s: s.bind(('127.0.0.1',0)); return s.getsockname()[1]
class Backend(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.end_headers(); self.wfile.write(b'APPLICATION_PLAINTEXT')
    def log_message(self,*args): pass
backend=http.server.HTTPServer(('127.0.0.1',0),Backend)
threading.Thread(target=backend.serve_forever,daemon=True).start()
p=base/'caddy'; p.mkdir(); hp,sp,ap=port(),port(),port()
route={'@id':'app','match':[{'host':['audit.invalid']}], 'handle':[{'handler':'reverse_proxy','upstreams':[{'dial':f'127.0.0.1:{backend.server_port}'}]}]}
config={'admin':{'listen':f'127.0.0.1:{ap}'},'storage':{'module':'file_system','root':str(p/'storage')},
        'apps':{'http':{'http_port':hp,'https_port':sp,'servers':{'srv0':{
            'listen':[f':{hp}',f':{sp}'],'routes':[route],
            'automatic_https':{'disable_certificates':True}}}}}}
# Verify the listener shape against the actual embedded source, rather than a second hand-maintained config.
import re, ssl
source=(repo/'lib/caddy.rhai').read_text()
base_literal=re.search(r'let base = (".*");',source).group(1)
assert json.loads(json.loads(base_literal))['apps']['http']['servers']['srv0']['listen']==[':443']
subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-keyout',str(p/'key.pem'),'-out',str(p/'cert.pem'),'-days','1','-subj','/CN=audit.invalid'],capture_output=True,check=True)
config['apps']['tls']={'certificates':{'load_files':[{'certificate':str(p/'cert.pem'),'key':str(p/'key.pem')}]}}
config['apps']['http']['servers']['srv0']['tls_connection_policies']=[{}]
(p/'config.json').write_text(json.dumps(config))
cenv=dict(env,XDG_DATA_HOME=str(p/'data'),XDG_CONFIG_HOME=str(p/'config'))
with (p/'server.log').open('wb') as log:
    c=subprocess.Popen([tools/'caddy','run','--config',p/'config.json'],env=cenv,stdout=log,stderr=log)
    try:
        # Migrate a legacy listener through the same admin PATCH used by proxy_boot.
        for _ in range(100):
            try:
                admin=http.client.HTTPConnection('127.0.0.1',ap,timeout=.5)
                admin.request('PATCH','/config/apps/http/servers/srv0/listen',body=json.dumps([f':{sp}']),headers={'Content-Type':'application/json'})
                updated=admin.getresponse(); assert updated.status==200; updated.read(); admin.close(); break
            except OSError: time.sleep(.05)
        else: raise AssertionError((p/'server.log').read_text())
        response=None
        for _ in range(100):
            try:
                conn=http.client.HTTPConnection('127.0.0.1',hp,timeout=.5)
                conn.request('GET','/',headers={'Host':'audit.invalid'})
                response=conn.getresponse(); status=response.status; body=response.read(); conn.close(); break
            except OSError: time.sleep(.05)
        assert response is not None, (p/'server.log').read_text()
        assert status==308 and response.getheader('Location').startswith('https://audit.invalid') and body!=b'APPLICATION_PLAINTEXT'
        secure=http.client.HTTPConnection('127.0.0.1',sp,timeout=2)
        secure.sock=ssl._create_unverified_context().wrap_socket(socket.create_connection(('127.0.0.1',sp),timeout=2),server_hostname='audit.invalid')
        secure.request('GET','/',headers={'Host':'audit.invalid'})
        response_tls=secure.getresponse()
        assert response_tls.status==200 and response_tls.read()==b'APPLICATION_PLAINTEXT'
        secure.close()
        record('caddy_redirects_http_and_serves_https',status=status,body=body.decode(),expected='HTTP redirect to HTTPS')
    finally:
        c.terminate(); c.wait(timeout=5); backend.shutdown(); backend.server_close()
(base/'results.json').write_text(json.dumps(results,indent=2))
print('Evidence directory:',base)
