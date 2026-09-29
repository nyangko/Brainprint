import time,subprocess,os
t=time.time()
p=subprocess.Popen([os.environ['R']+'/brainprintd'],stdout=open(os.environ['SP']+'/ab/daemon.log','a'),stderr=subprocess.STDOUT,start_new_session=True)
open(os.environ['SP']+'/ab/daemon.pid','w').write(str(p.pid))
while True:
    r=subprocess.run([os.environ['R']+'/brainprint','status'],capture_output=True,text=True)
    if r.returncode==0 and r.stdout.strip(): break
    time.sleep(0.02)
print('daemon status-ready s=%.2f pid=%d'%(time.time()-t,p.pid))
