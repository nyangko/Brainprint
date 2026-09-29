import json,sys,subprocess,os,time,sqlite3,uuid
R=os.environ['R']; ws=sys.argv[1]; sym=sys.argv[2] if len(sys.argv)>2 else 'load_workspace_config'
t=time.time()
r=subprocess.run([R+'/brainprint','relations','--symbol-name',sym,'--direction','incoming','--workspace',ws,'--json'],capture_output=True,text=True)
ms=(time.time()-t)*1000
d=json.loads(r.stdout); o=d['outcome']['Ok']['Relations']
db=sqlite3.connect('file:%s/.brainprint/data/index.db?mode=ro'%ws,uri=True)
path={str(uuid.UUID(bytes=bytes(u))):p for u,p in db.execute('select uid,path_rel from resource')}
sym_name={}
try:
    for u,n,q in db.execute('select uid,name,qualified_name from symbol'): sym_name[str(uuid.UUID(bytes=bytes(u)))]=q or n
except Exception as e: pass
a=o['answers'][0]
print('ms=%.0f bytes=%d currentness=%s coverage=%s'%(ms,len(r.stdout),o['currentness'],json.dumps(a['coverage'])))
sites=[]
for c in a['confirmed']:
    for e in c['evidence']:
        sites.append((path.get(e['resource'],e['resource']),e['span']['start']['line']+1,sym_name.get(c['source'].get('Symbol'),c['source']),e['occurrence_kind'],c['resolution'],c['support']))
for s in sorted(sites): print(s)
print('confirmed',len(a['confirmed']),'gaps',len(a['gaps']))
for g in a['gaps']: print('GAP',json.dumps(g)[:300])
