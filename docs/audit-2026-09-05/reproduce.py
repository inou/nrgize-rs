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
    assert cmd.startswith('umask 077; cat > ')
    target = shlex.split(cmd)[-1]
    assert pathlib.Path(target).parent == p
    sys.exit(subprocess.run(['sh','-c',cmd], input=sys.stdin.buffer.read()).returncode)
if mode == 'build-archive' and cmd.startswith('base64 -d'):
    (p/'archive.b64').write_bytes(sys.stdin.buffer.read())
if cmd.startswith('mkdir '): (p/'remote-lock').write_text('held')
if cmd.startswith('rm -rf '): (p/'remote-lock').unlink(missing_ok=True)
if cmd.startswith('nc -z '): sys.exit(1)
if 'inspect -f' in cmd: print('true'); sys.exit(0)
if cmd.startswith('curl -s -o /dev/null'): print('200'); sys.exit(0)
if mode == 'pull-fail' and ' pull ' in cmd: print('simulated registry failure',file=sys.stderr); sys.exit(1)
if mode == 'cleanup-fail' and ' container prune ' in cmd: print('simulated ssh loss',file=sys.stderr); sys.exit(255)
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

# 1. A normal Secret is leaked by the native compensation-error sink.
p = project('rollback-secret')
r = run(p, 'let s=reveal(secret("TEST")); transaction(|| { on_rollback(|| { throw s; }); throw "original error"; });',
        extra={'NRG_SECRET_TEST':'AUDIT_ONLY_PASSWORD'})
assert r.returncode != 0 and 'AUDIT_ONLY_PASSWORD' in r.stderr
record('rollback_error_unredacted', exit=r.returncode, leaked=True)

# 2. write_remote preserves unsafe existing modes and follows symlinks.
p = project('remote-write')
victim = p/'victim'; victim.write_text('old'); victim.chmod(0o644)
link = p/'remote-env'; link.symlink_to(victim)
r = run(p, f'let r=write_remote("fake", "AUDIT_ONLY_PASSWORD", {json.dumps(str(link))}); if !r.ok {{throw r.stderr;}}',mode='write')
assert r.returncode == 0 and victim.read_text()=='AUDIT_ONLY_PASSWORD' and victim.stat().st_mode & 0o777 == 0o644
record('write_remote_symlink_and_permissions', exit=0, symlink_followed=True, mode='0644')

# 3. A failed rollback overwrites the automatic rollback image before the pull.
p = project('rollback-prev', INITIAL)
r = run(p,'import "std/deploy" as d; d::rollback(["web1"],"app");',mode='pull-fail')
assert r.returncode != 0 and state(p)['app.prev']=='repo:v2'
record('failed_rollback_destroys_prev', before='repo:v1',after=state(p)['app.prev'])

# 4. Cleanup SSH failure after cutover leaves state pointing at the retired port.
p = project('cleanup-state', INITIAL)
r = run(p,DEPLOY,mode='cleanup-fail')
calls = (p/'ssh.jsonl').read_text()
assert r.returncode==0 and "--target 'localhost:13000'" in calls and state(p)['app.target.web1']=='localhost:13001'
record('cleanup_failure_stale_target',exit=0, switched_to='localhost:13000',saved=state(p)['app.target.web1'],saved_image=state(p)['app.image'])

# 5. A switch that takes effect but loses its reply is treated as never switched.
p = project('ambiguous-switch', INITIAL)
r = run(p,DEPLOY,mode='ambiguous-switch')
calls=(p/'ssh.jsonl').read_text()
assert r.returncode!=0 and "rm -f 'app-web-v3-13000'" in calls and len((p/'switch-attempts').read_text().splitlines())==2
record('ambiguous_switch_removes_new',exit=r.returncode,restore_failed=True,new_container_removal_requested=True)

# 6. Engine abort skips Rhai try/catch responsible for the distributed lock.
p = project('interrupt-lock', INITIAL)
r = run(p,DEPLOY,mode='interrupt')
assert r.returncode!=0 and (p/'remote-lock').exists()
record('sigterm_leaks_distributed_lock',exit=r.returncode,lock_retained=True)

# 7. Native rollback uses Docker even if the last deployment used Podman.
p = project('rollback-runtime',dict(INITIAL,**{'nrg.runtime.cmd':'podman','nrg.runtime.name':'podman'}))
r = run(p,args=['rollback','app','--dry-run'])
assert r.returncode==0 and "docker pull 'repo:v1'" in r.stdout and 'podman pull' not in r.stdout
record('cli_rollback_ignores_runtime',saved='podman',planned='docker')

# 8. Dry-run drops root used for file-based secrets when invoked in a subdirectory.
p = project('dryrun-secret-root'); (p/'sub').mkdir()
(p/'.energize/secrets').write_text('TEST=AUDIT_ONLY_PASSWORD\n')
script='let s=secret("TEST"); print("found");'
live=run(p,script,cwd=p/'sub'); dry=run(p,script,args=['exec','--dry-run'],cwd=p/'sub')
assert live.returncode==0 and dry.returncode!=0 and 'not found' in dry.stderr
record('dryrun_wrong_secret_root',live=live.returncode,dry_run=dry.returncode)

# 9. In-memory registration is absent when persisted credentials are replayed.
saved=dict(INITIAL); saved['app.config']=json.dumps({'envs':{'PASSWORD':'AUDIT_ONLY_PASSWORD'}})
p=project('rollback-replayed-secret',saved)
r=run(p,args=['rollback','app'],mode='secret-error')
assert r.returncode!=0
assert 'AUDIT_ONLY_PASSWORD' in r.stderr
record('state_replay_has_no_secret_registration',leaked=True)

# 10. Bunny is absent from the embedded-module / vendor catalog.
p=project('bunny-embedded')
r=run(p,'import "std/bunny" as b;',args=['exec','--dry-run'])
assert r.returncode!=0 and 'Module not found' in r.stderr
record('bunny_missing_from_embedded_catalog',exit=r.returncode)

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
assert r.returncode==0 and captured and {k.lower():v for k,v in captured[0].items()}['accesskey']=='AUDIT_ONLY_PASSWORD'
record('cross_origin_redirect_leaks_accesskey',leaked=True)

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
assert 'status=200,body=' in r.stderr and 'bad' not in r.stderr
record('truncated_http_body_becomes_success',status=200,body='')

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
    assert r.returncode==0 and changed
    record('remote_build_local_archive_symlink',exit=0,victim_overwritten=True,mode=oct(victim.stat().st_mode&0o777))
finally:
    archive.unlink(missing_ok=True)

# 14. remove --purge-state leaves .port and .config keys and ignores held locks.
p=project('remove-purge',INITIAL)
r=run(p,args=['remove','app','--yes','--purge-state'])
remaining=state(p)
assert r.returncode==0 and 'app.port.web1' in remaining and 'app.config' in remaining
record('remove_purge_retains_state',remaining_keys=sorted(remaining),exit=0)

# 15. A failed redeploy replaces the last known-good predecessor with the current image.
p=project('failed-deploy-prev',INITIAL)
r=run(p,DEPLOY,mode='ambiguous-switch')
assert r.returncode!=0 and state(p)['app.prev']=='repo:v2'
record('failed_deploy_destroys_prev',before='repo:v1',after=state(p)['app.prev'])

(BASE/'results.json').write_text(json.dumps(RESULTS,indent=2))
print('Evidence directory:',BASE)
