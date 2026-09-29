import subprocess,json,sys,os,time
class M:
  def __init__(s,cmd,cwd):
    s.p=subprocess.Popen(cmd,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,cwd=cwd,text=True,bufsize=1);s.i=0
  def rpc(s,method,params=None,notify=False):
    msg={"jsonrpc":"2.0","method":method}
    if params is not None: msg["params"]=params
    if not notify: s.i+=1;msg["id"]=s.i
    s.p.stdin.write(json.dumps(msg)+"\n");s.p.stdin.flush()
    if notify: return None
    while True:
      l=s.p.stdout.readline()
      if not l: raise SystemExit("closed: "+s.p.stderr.read()[:500])
      d=json.loads(l)
      if d.get("id")==s.i: return d
def start(binp,cwd):
  m=M([binp],cwd)
  r=m.rpc("initialize",{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t14","version":"0"}})
  m.rpc("notifications/initialized",{},notify=True)
  return m,r
if __name__=="__main__":
  m,r=start(sys.argv[1],sys.argv[2])
  print("server:",r["result"].get("serverInfo"),"instructions_bytes=",len(r["result"].get("instructions") or ""))
  tl=m.rpc("tools/list",{})["result"]["tools"]
  print("tools:",[t["name"] for t in tl],"tools_list_json_bytes=",len(json.dumps(tl)))
  json.dump(tl,open(sys.argv[3],"w"),indent=1)
