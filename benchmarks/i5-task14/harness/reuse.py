import subprocess,os,json,sys,time
sys.path.insert(0,os.environ['SP']+'/t14'); import mcpc
R=os.environ['R'];ws=os.environ['SP']+'/t14/ws'
def cli(ret,sess):
    r=subprocess.run([R+'/brainprint','inspect','--symbol-name','load_workspace_config','--budget','standard','--retention',ret,'--client-id','t14','--session-id',sess,'--workspace',ws,'--json'],capture_output=True,text=True)
    d=json.loads(r.stdout);pg=d['outcome']['Ok']['Inspect']['page']
    return len(r.stdout),len(pg['evidence']),len(pg.get('references') or [])
for ret in ['disabled','fresh','retained']:
    print(ret,[cli(ret,'s-'+ret) for _ in range(3)],'(wire_bytes, evidence_items, references)')
m,_=mcpc.start(R+'/brainprint-mcp',ws)
tl=m.rpc('tools/list',{})
out=[]
for i in range(3):
    r=m.rpc('tools/call',{'name':'brainprint.inspect','arguments':{'symbol_name':'load_workspace_config'}})
    out.append(len(json.dumps(r['result'])))
print('mcp inspect x3 result_json_bytes',out)
print(json.dumps(r['result'])[-600:])
