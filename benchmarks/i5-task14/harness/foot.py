import subprocess,os,time,threading,json
R=os.environ['R'];PID=open(os.environ['SP']+'/ab/daemon.pid').read().strip();ws=os.environ['SP']+'/t14/ws'
def tree():
    rows=[l.split(None,4) for l in subprocess.run(['/bin/ps','-A','-o','pid=,ppid=,rss=,time=,command='],capture_output=True,text=True).stdout.splitlines()]
    k={PID};ch=True
    while ch:
        ch=False
        for r in rows:
            if r[1] in k and r[0] not in k:k.add(r[0]);ch=True
    d=[r for r in rows if r[0] in k]
    return sum(int(r[2]) for r in d),len(d)-1,{r[4].split('/')[-1].split()[0]:int(r[2]) for r in d}
samples=[];stop=False
def samp():
    while not stop:
        samples.append((time.time(),)+tree()[:2]);time.sleep(0.2)
th=threading.Thread(target=samp);th.start()
t0=time.time()
for f in ['crates/engine/src/config.rs','crates/engine/src/query_surface.rs','crates/daemon/src/query/lifecycle.rs']:
    subprocess.run([R+'/brainprint','relations','--resource-path',f,'--direction','outgoing','--workspace',ws,'--json'],capture_output=True)
    print(f.split('/')[-1],'done +%.2fs'%(time.time()-t0))
time.sleep(2);stop=True;th.join()
print('peak tree RSS MB %.0f (children max %d)'%(max(s[1] for s in samples)/1024,max(s[2] for s in samples)))
print('final tree',tree())
print('cpu/ threads/ fds:',subprocess.run(['/bin/ps','-M','-p',PID],capture_output=True,text=True).stdout.count('\n')-1,'threads;',len(subprocess.run(['/usr/sbin/lsof','-p',PID],capture_output=True,text=True).stdout.splitlines())-1,'fds')
