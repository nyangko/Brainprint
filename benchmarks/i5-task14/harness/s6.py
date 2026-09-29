import subprocess,os,json,sys,threading,time
sys.path.insert(0,os.environ['SP']+'/t14'); import mcpc
R=os.environ['R'];PID=open(os.environ['SP']+'/ab/daemon.pid').read().strip()
wss=[os.environ['SP']+'/t14/ws',os.environ['SP']+'/t14/ws2']
def tree():
    out=subprocess.run(['ps','-A','-o','pid=,ppid=,rss=,command='],capture_output=True,text=True).stdout.splitlines()
    rows=[l.split(None,3) for l in out]
    kids={PID}; changed=True
    while changed:
        changed=False
        for p,pp,rss,cmd in rows:
            if pp in kids and p not in kids: kids.add(p);changed=True
    desc=[r for r in rows if r[0] in kids and r[0]!=PID]
    daemons=[r for r in rows if r[3].split()[0].endswith('/brainprintd')]
    mcps=[r for r in rows if r[3].split()[0].endswith('/brainprint-mcp')]
    return dict(daemons=len(daemons),mcp_procs=len(mcps),daemon_rss_mb=round(int([r for r in rows if r[0]==PID][0][2])/1024,1),descendants=[(r[3].split('/')[-1].split()[0],round(int(r[2])/1024)) for r in desc])
print('before',tree())
sess=[];res=[]
def work(m,ws,i):
    r=m.rpc('tools/call',{'name':'brainprint.find','arguments':{'mode':'target','symbol_name':'load_workspace_config','workspace_path':ws}})
    r2=m.rpc('tools/call',{'name':'brainprint.relations','arguments':{'mode':'direct','symbol_name':'load_workspace_config','direction':'incoming','workspace_path':ws}})
    res.append((ws[-3:],i,'error' if r['result'].get('isError') or r2['result'].get('isError') else 'ok'))
ms=[]
for w in wss:
    for i in range(5):
        m,_=mcpc.start(R+'/brainprint-mcp',w); ms.append((m,w,i))
ths=[threading.Thread(target=work,args=x) for x in ms]
t=time.time();[x.start() for x in ths];[x.join() for x in ths]
print('10 concurrent sessions %.2fs'%(time.time()-t),sorted(res))
for w in wss:
    for f in ['crates/engine/src/config.rs']:
        subprocess.run([R+'/brainprint','relations','--resource-path',f,'--direction','outgoing','--workspace',w,'--json'],capture_output=True)
time.sleep(3)
print('after',tree())
