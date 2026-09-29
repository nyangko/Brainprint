import subprocess,time,json,sys,os
R,W,PID=sys.argv[1],sys.argv[2],sys.argv[3]
def cpu():
  t=subprocess.run(['ps','-o','time=,rss=','-p',PID],capture_output=True,text=True).stdout.split()
  m,s=t[0].split(':'); return float(m)*60+float(s), int(t[1])
def q(name,args,retry=False):
  t=time.time();r=subprocess.run([R+'/brainprint']+args+['--workspace',W,'--json'],capture_output=True,text=True);ms=(time.time()-t)*1000
  out=r.stdout
  try:
    d=json.loads(out); o=d['outcome']; kind=list(o.keys())[0]; inner=o[kind]
    op=list(inner.keys())[0] if isinstance(inner,dict) else ''
    s=json.dumps(inner)
    cur=[c for c in ('"Current"','"NotCurrent"','"Stale"') if c in s]
    extra=[k for k in ('MORE_AVAILABLE','MoreAvailable','Reused','reuse','continuation','Complete') if k in s]
  except Exception as e:
    d=None;kind='PARSE_ERR';op=r.stderr[:200];cur=[];extra=[]
  print(f"{name:34} {ms:7.0f}ms exit={r.returncode} {kind}/{op} bytes={len(out)} cur={cur} {extra[:4]}")
  return d,out
c0,rss0=cpu()
S=['--symbol-name','load_workspace_config']
B=['--budget','standard','--retention','disabled']
q('find target',['find','target']+S+B)
q('find files crates/agent/src',['find','files','--path-prefix','crates/agent/src'])
q('find text literal (explicit)',['find','text','--literal','BRAINPRINT_ADOPTION_TELEMETRY_PATH','--search-budget','standard'])
q('inspect',['inspect']+S+B)
q('relations incoming',['relations']+S+['--direction','incoming'])
q('relations outgoing',['relations']+S+['--direction','outgoing'])
q('impact public-signature',['impact']+S+['--change','public-signature']+B)
q('context change',['context','change']+S+B)
q('knowledge rules',['knowledge','rules'])
q('structure group-directory',['structure','--group-directory','--root','crates','--depth','2'])
c1,rss1=cpu()
print(f"daemon cpu_s delta={c1-c0:.2f} rss_kb before={rss0} after={rss1}")
