#!/usr/bin/env python3
"""Local audit probes. All SSH calls are intercepted; only loopback HTTP is used.
Usage: python3 reproduce.py /absolute/path/to/nrgize-rs
Assertions describe defects present at the audited revision, not desired behavior.
"""
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import base64
import threading
import time

REPO = Path(sys.argv[1]).resolve()
BIN = REPO / 'target/debug/nrg'
BASE = Path(tempfile.mkdtemp(prefix='nrg-audit-probes-'))
RESULTS = []

SHIM = r'''
import json, os, pathlib, shlex, subprocess, sys
p = pathlib.Path(os.environ['AUDIT_PROJECT'])
cmd = sys.argv[-1]
with (p/'ssh.jsonl').open('a') as f: f.write(json.dumps(cmd)+'\n')
mode = os.environ.get('AUDIT_MODE','')
if mode == 'write':
    sys.exit(subprocess.run(['sh','-c',cmd], input=sys.stdin.buffer.read()).returncode)
if mode == 'build-archive' and cmd.startswith('base64 -d'):
    (p/'archive.b64').write_bytes(sys.stdin.buffer.read())
if 'nrg-deploy-lock-' in cmd:
    cmd = cmd.replace('/tmp/nrg-deploy-lock-app', str(p/'remote-lock'))
    sys.exit(subprocess.run(['sh','-c',cmd]).returncode)
if cmd.startswith('mktemp -d '):
    sys.exit(subprocess.run(['sh','-c',cmd]).returncode)
if cmd.startswith('nc -z '): sys.exit(1)
if 'inspect -f' in cmd: print('true'); sys.exit(0)
if cmd.startswith('curl -s -o /dev/null'): print('200'); sys.exit(0)
if mode == 'pull-fail' and ' pull ' in cmd: print('simulated registry failure',file=sys.stderr); sys.exit(1)
if mode == 'promotion-missing' and " rename 'app-web-v3-" in cmd:
    print('No such container',file=sys.stderr); sys.exit(1)
if mode == 'cleanup-fail' and ' rename ' in cmd: print('simulated ssh loss',file=sys.stderr); sys.exit(255)
if mode == 'ambiguous-switch' and 'kamal-proxy deploy' in cmd:
    with (p/'switch-attempts').open('a') as f: f.write(cmd+'\n')
    # Model a committed forward switch whose acknowledgment is lost; restore also fails.
    print('simulated lost acknowledgement',file=sys.stderr); sys.exit(255)
if mode == 'interrupt' and ' pull ' in cmd:
    import signal
    os.kill(os.getppid(), signal.SIGTERM)
if mode == 'secret-error' and ' run -d ' in cmd:
    print('returned credential: AUDIT_ONLY_PASSWORD',file=sys.stderr); sys.exit(1)
sys.exit(0)
'''

def project(name, state=None):
    p = BASE/name
    p.mkdir()
    (p/'.energize').mkdir()
    (p/'lib').symlink_to(REPO/'lib')
    (p/'bin').mkdir()
    shim = p/'bin/ssh'
    shim.write_text('#!'+sys.executable+'\n'+SHIM)
    shim.chmod(0o700)
    if state is not None:
        (p/'.energize/state.json').write_text(json.dumps({'version':1,'data':state}))
    return p

def run(p, script='', args=None, mode='', cwd=None, extra=None):
    (p/'Energize.rhai').write_text(script)
    env = {k:v for k,v in os.environ.items() if not k.startswith('NRG_')}
    env.update(PATH=str(p/'bin')+':'+os.environ['PATH'], AUDIT_PROJECT=str(p), AUDIT_MODE=mode,
               NRG_SSH_CONTROL_PERSIST='off')
    env.update(extra or {})
    r = subprocess.run([str(BIN)]+(args or ['exec']),cwd=cwd or p,env=env,
                       capture_output=True,text=True,timeout=20)
    (p/'last.stdout').write_text(r.stdout)
    (p/'last.stderr').write_text(r.stderr)
    return r

def record(name, **evidence):
    RESULTS.append(dict(name=name, **evidence))
    print(json.dumps(RESULTS[-1]),flush=True)

def state(p):
    return json.loads((p/'.energize/state.json').read_text())['data']

INITIAL = {'app.version':'v2','app.image':'repo:v2','app.prev':'repo:v1',
           'app.port.web1':'13001','app.target.web1':'localhost:13001',
           'app.config':json.dumps({'health_attempts':1,'health_interval':0})}
DEPLOY = 'import "std/deploy" as d; d::deploy(["web1"],"repo:v3","app",#{skip_build:true,skip_push:true,health_attempts:1,health_interval:0});'


def check(name, condition, detail=""):
    assert condition, f"{name}: {detail} (fixtures: {BASE})"
    print(f"PASS {name}", flush=True)
p=project('redaction')
r=run(p,'let s=reveal(secret("TEST")); transaction(|| { on_rollback(|| { throw s; }); throw "original"; });',extra={'NRG_SECRET_TEST':'AUDIT_ONLY_PASSWORD'})
check('compensation redaction', r.returncode != 0 and 'AUDIT_ONLY_PASSWORD' not in r.stderr, r.stderr)
p=project('write'); victim=p/'victim'; victim.write_text('original'); victim.chmod(0o644); link=p/'env'; link.symlink_to(victim)
r=run(p,f'let r=write_remote("fake","SECRET",{json.dumps(str(link))}); if !r.ok {{throw "write failed";}}',mode='write')
check('symlink rejected',r.returncode != 0 and victim.read_text()=='original',r.stderr)
link.unlink(); link.write_text('old'); link.chmod(0o644)
r=run(p,f'let r=write_remote("fake","SECRET",{json.dumps(str(link))}); if !r.ok {{throw "write failed";}}',mode='write')
check('existing file replaced privately',r.returncode==0 and link.read_text()=='SECRET' and link.stat().st_mode&0o777==0o600,r.stderr)
for name, script, mode in [('rollback-history','import "std/deploy" as d; d::rollback(["web1"],"app");','pull-fail'),('deploy-history',DEPLOY,'ambiguous-switch')]:
    p=project(name,INITIAL); r=run(p,script,mode=mode)
    check(name,r.returncode!=0 and state(p)['app.prev']=='repo:v1',r.stderr)
    if mode=='ambiguous-switch':
        calls=(p/'ssh.jsonl').read_text()
        check('ambiguous cutover preserves backend',"rm -f 'app-web-v3-13000'" not in calls and len((p/'switch-attempts').read_text().splitlines())==2,calls)
        check('ambiguous cutover retains journal','app.transition.web1' in state(p))
p=project('cleanup',INITIAL);r=run(p,DEPLOY,mode='cleanup-fail')
check('cutover state survives cleanup failure',r.returncode==0 and state(p)['app.target.web1']=='localhost:13000' and state(p)['app.image']=='repo:v3',r.stderr)
check('no host-wide pruning','container prune' not in (p/'ssh.jsonl').read_text())
p=project('interrupt',INITIAL);r=run(p,DEPLOY,mode='interrupt')
check('interrupt releases owned lock',r.returncode!=0 and not (p/'remote-lock').exists(),r.stderr)
p=project('runtime',dict(INITIAL,**{'app.runtime.cmd':'podman','app.runtime.name':'podman'}));r=run(p,args=['rollback','app','--dry-run'])
check('rollback runtime',r.returncode==0 and 'podman pull' in r.stdout and 'docker pull' not in r.stdout,r.stderr)
p=project('root');(p/'sub').mkdir();(p/'.energize/secrets').write_text('TEST=AUDIT_ONLY_PASSWORD\n')
r=run(p,'let s=secret("TEST");',args=['exec','--dry-run'],cwd=p/'sub')
check('overlay project root',r.returncode==0,r.stderr)
saved=dict(INITIAL);saved['app.config']=json.dumps({'envs':{'PASSWORD':'AUDIT_ONLY_PASSWORD'}})
p=project('replay',saved);r=run(p,args=['rollback','app'],mode='secret-error')
check('replayed secrets redacted',r.returncode!=0 and 'AUDIT_ONLY_PASSWORD' not in r.stderr,r.stderr)
p=project('bunny');r=run(p,'import "std/bunny" as b;')
check('embedded Bunny',r.returncode==0,r.stderr)
p=project('purge',INITIAL);r=run(p,args=['remove','app','--yes','--purge-state'])
check('complete state purge',r.returncode==0 and not any(k.startswith('app.') for k in state(p)),r.stderr)
backup=json.loads((p/'.energize/state.json.bak').read_text())['data']
check('purge sanitizes automatic backup',not any(k.startswith('app.') for k in backup))
p=project('dest',{'production/'+k:v for k,v in INITIAL.items()});r=run(p,args=['status','--dest','production'])
check('day-2 destination',r.returncode==0 and 'app' in r.stdout,r.stderr)
p=project('timeout');r=run(p,'let r=local_exec("sleep 30"); if r.ok {throw "unexpected success";} print(r.stderr);',extra={'NRG_COMMAND_TIMEOUT_SECS':'1'})
check('command deadline',r.returncode==0 and 'deadline exceeded' in r.stderr,r.stderr)
print(f"All checks passed; fixtures: {BASE}")

# 11. Hostile redirect receives the custom API-key header at a different origin.
captured=[]
class Receiver(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        captured.append(dict(self.headers)); self.send_response(200); self.end_headers(); self.wfile.write(b'{}')
    def log_message(self,*args): pass
sink=http.server.HTTPServer(('127.0.0.1',0),Receiver)
class Redirect(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(302); self.send_header('Location',f'http://localhost:{sink.server_port}/stolen'); self.end_headers()
    def log_message(self,*args): pass
redirect=http.server.HTTPServer(('127.0.0.1',0),Redirect)
for server in [sink,redirect]: threading.Thread(target=server.serve_forever,daemon=True).start()
p=project('http-redirect')
r=run(p,f'let s=secret("TEST"); let r=http_get("http://127.0.0.1:{redirect.server_port}/",#{{AccessKey:reveal(s)}}); print(r.status);',extra={'NRG_SECRET_TEST':'AUDIT_ONLY_PASSWORD'})
for server in [sink,redirect]: server.shutdown(); server.server_close()
assert r.returncode==0 and not captured
record('cross_origin_redirect_leaks_accesskey',leaked=False)

# 12. Read errors after a successful HTTP status are silently converted to empty success.
class Truncated(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.send_header('Content-Length','100'); self.end_headers(); self.wfile.write(b'bad'); self.close_connection=True
    def log_message(self,*args): pass
server=http.server.HTTPServer(('127.0.0.1',0),Truncated)
threading.Thread(target=server.serve_forever,daemon=True).start()
p=project('http-truncated')
r=run(p,f'let r=http_get("http://127.0.0.1:{server.server_port}/"); print("status="+r.status+",body="+r.body);')
server.shutdown(); server.server_close()
assert 'status=0,body=response body failed' in r.stderr
record('truncated_http_body_becomes_success',status=0,body='')

# 13. The local archive for a remote build has a predictable /tmp filename.
p=project('local-build-archive')
(p/'Dockerfile').write_text('FROM scratch\n')
victim=p/'victim'; victim.write_text('ORIGINAL AUDIT FIXTURE'); victim.chmod(0o644)
tag='audit-'+BASE.name+':v1'
archive=Path('/tmp/.nrg-build-ctx-'+tag.replace(':','_')+'.local.tgz')
assert not archive.exists() and not archive.is_symlink()
archive.symlink_to(victim)
try:
    r=run(p,f'import "std/docker" as d; d::docker_build({json.dumps(tag)},#{{build_host:"fake"}});',mode='build-archive')
    changed=victim.read_bytes()!=b'ORIGINAL AUDIT FIXTURE'
    assert r.returncode==0 and not changed
    record('remote_build_local_archive_symlink',exit=0,victim_overwritten=False,mode=oct(victim.stat().st_mode&0o777))
finally:
    archive.unlink(missing_ok=True)


p=project('promotion-missing',INITIAL);r=run(p,DEPLOY,mode='promotion-missing')
calls=(p/'ssh.jsonl').read_text()
check('missing promotion preserves old backend',r.returncode==0 and "stop -t '30' 'app-web-old'" not in calls and 'app.transition.web1' in state(p),r.stderr)
