import subprocess,os,time,json,sys
R=os.environ['R'];ws=os.environ['SP']+'/t14/ws';PID=open(os.environ['SP']+'/ab/daemon.pid').read().strip()
def ps():
    out=subprocess.run(['ps','-A','-o','pid=,ppid=,rss=,time=,command='],capture_output=True,text=True).stdout.splitlines()
    kids=[l.split(None,4) for l in out if l.split(None,4)[1]==PID]
    ra=[l.split(None,4) for l in out if 'rust-analyzer' in l.split(None,4)[4] and 'proc-macro' not in l]
    me=[l.split(None,4) for l in out if l.split(None,4)[0]==PID][0]
    return me[2],me[3],len(kids),len(ra),sum(int(k[2]) for k in ra)
def inc():
    r=subprocess.run(['python3',os.environ['SP']+'/t14/callers.py',ws],capture_output=True,text=True).stdout
    n=[l for l in r.splitlines() if l.startswith('confirmed')][0]
    sites=[l for l in r.splitlines() if l.startswith("('")]
    return n,len(sites),sites,r.splitlines()[0][:250]
print('t0',ps(),inc()[:2])
for f in ['crates/engine/src/config.rs','crates/engine/src/query_surface.rs','crates/daemon/src/query/lifecycle.rs']:
    t=time.time(); first=None
    for i in range(1,40):
        subprocess.run([R+'/brainprint','relations','--resource-path',f,'--direction','outgoing','--workspace',ws,'--json'],capture_output=True,text=True)
        n,ns,sites,head=inc()
        if first is None: first=(i,ns)
        if ns>=({'config':6,'query_surface':7,'lifecycle':8}[f.split('/')[-1].split('.')[0]]): break
        time.sleep(0.5)
    print(f,'asks',i,'elapsed %.2fs'%(time.time()-t),n,'sites',ns,'ps(rss_kb,cpu,children,ra_procs,ra_rss_kb)',ps())
n,ns,sites,head=inc()
print(head); print('\n'.join(sites))
# warm
t=time.time(); n2,ns2,_,_=inc(); print('warm incoming %.0fms sites %d'%((time.time()-t)*1000,ns2))
