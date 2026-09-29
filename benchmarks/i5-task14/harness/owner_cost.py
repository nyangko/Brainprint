import subprocess,os,time,json,threading,sys
R=os.environ['R'];SP=os.environ['SP'];PID=open(SP+'/ab/daemon.pid').read().strip()
def tree():
    rows=[l.split(None,4) for l in subprocess.run(['/bin/ps','-A','-o','pid=,ppid=,rss=,time=,command='],capture_output=True,text=True).stdout.splitlines()]
    k={PID};ch=True
    while ch:
        ch=False
        for r in rows:
            if r[1] in k and r[0] not in k:k.add(r[0]);ch=True
    d=[r for r in rows if r[0] in k]
    return sum(int(r[2]) for r in d)/1024,len(d)-1
def ctx(ws,args):
    t=time.time()
    r=subprocess.run([R+'/brainprint','context','change','--symbol-name','load_workspace_config','--language','rust','--change','public-signature','--budget','standard','--retention','disabled','--workspace',ws,'--json']+args,capture_output=True,text=True)
    return (time.time()-t)*1000,len(r.stdout),r.stdout
def sample(stop,out):
    while not stop.is_set(): out.append(tree()[0]);time.sleep(0.2)
for ws in sys.argv[1:]:
    ws=SP+'/ab/wt/'+ws
    stop=threading.Event();peak=[];th=threading.Thread(target=sample,args=(stop,peak));th.start()
    ms1,b1,o1=ctx(ws,[])
    ms2,b2,o2=ctx(ws,[])
    stop.set();th.join()
    j=json.loads(o1)['outcome']['Ok']['Context']
    gaps=[g for g in json.dumps(j).split('UnconfirmedCallerOwners')[1:2]]
    print(f"{os.path.basename(ws)}: 1st call {ms1:.0f}ms ({b1}B) | 2nd call (same, warm, owners current) {ms2:.0f}ms | peak tree RSS {max(peak):.0f}MB | tree now {tree()}")
