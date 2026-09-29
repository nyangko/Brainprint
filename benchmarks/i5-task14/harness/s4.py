import subprocess,os,json,time
R=os.environ['R'];ws=os.environ['SP']+'/t14/ws'
F=ws+'/crates/engine/src/config.rs'
def run(a):
    r=subprocess.run([R+'/brainprint']+a+['--workspace',ws,'--json'],capture_output=True,text=True)
    try:return json.loads(r.stdout)['outcome']
    except Exception:return {'raw':r.stdout[:200]+r.stderr[:200]}
def ins(extra=[],sym='load_workspace_config'):
    return run(['inspect','--symbol-name',sym,'--budget','compact','--budget-items','6','--retention','disabled']+extra)
def desc(o):
    if 'Ok' in o:
        i=o['Ok']['Inspect'];return 'Ok cur=%s items=%d more=%s'%(i['currentness'],len(i['page']['evidence']),i['more_available'])
    return json.dumps(o)[:200]
o=ins();import base64;tok=base64.urlsafe_b64encode(json.dumps(o['Ok']['Inspect']['continuation'],separators=(',',':')).encode()).decode().rstrip('=')
print('page1',desc(o))
o=ins(['--continuation',tok]);print('page2 same rev',desc(o))
orig=open(F).read()
t=time.time();open(F,'w').write(orig+'\n// t14 edit\npub fn t14_new_symbol() {}\n')
while True:
    o=ins(['--continuation',tok])
    if ('Err' in o) or ('raw' in o and 'invalid' not in o['raw']) or time.time()-t>10:break
    time.sleep(0.02)
print('after edit +%.0fms token ->'%((time.time()-t)*1000),desc(o))
t=time.time()
while True:
    n=run(['find','target','--symbol-name','t14_new_symbol','--budget','compact','--retention','disabled'])
    if 'Ok' in n and 'Current' in json.dumps(n)and 't14_new_symbol' in json.dumps(n).split('"name"')[1:2].__str__(): break
    if time.time()-t>10:break
    time.sleep(0.02)
print('new symbol visible +%.0fms'%((time.time()-t)*1000),desc({'Ok':{'Inspect':{'currentness':'?','page':{'evidence':[]},'more_available':'?'}}}) if False else json.dumps(n)[:120])
open(F,'w').write(orig)
t=time.time()
while True:
    o=ins(['--continuation',tok])
    if time.time()-t>1.0:break
    time.sleep(0.05)
print('after byte-identical revert, old token ->',desc(o))
t=time.time()
while True:
    n=run(['find','target','--symbol-name','t14_new_symbol','--budget','compact','--retention','disabled'])
    if 'Ok' in n and 'NotFound' in json.dumps(n): break
    if time.time()-t>10:break
    time.sleep(0.02)
print('deleted symbol +%.0fms'%((time.time()-t)*1000),json.dumps(n)[:200])
print('git status',subprocess.run(['git','-C',ws,'status','--porcelain'],capture_output=True,text=True).stdout.strip() or 'clean')
