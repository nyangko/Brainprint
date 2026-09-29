import sqlite3,json,subprocess,os,uuid,sys
SP=os.environ['SP'];R=os.environ['R']
ws=SP+'/ab/wt/T2-B-r2'
db=sqlite3.connect(f'file:{ws}/.brainprint/data/index.db?mode=ro',uri=True)
def enclosing(path,start_byte):
    row=db.execute("select s.qualified_name,s.kind,s.start_line,s.end_line,s.start_byte,s.end_byte from symbol s join resource r on r.id=s.resource_id where r.path_key=? and s.start_byte<=? and s.end_byte>=? order by (s.end_byte-s.start_byte) limit 1",(path,start_byte,start_byte)).fetchone()
    return row
def find_text(pattern,ci):
    a=[R+'/brainprint','find','text','--literal',pattern,'--search-budget','standard','--workspace',ws,'--json']
    if ci: a.append('--case-insensitive')
    r=subprocess.run(a,capture_output=True,text=True)
    return r.stdout
for pattern,ci in [('TELEMETRY',False),('telemetry',True)]:
    out=find_text(pattern,ci)
    try: j=json.loads(out)
    except Exception: print('cli',out[:200]); continue
    ms=j['outcome']['Ok']['Find']['Text']['matches']
    base=len(json.dumps(j['outcome']['Ok']['Find']['Text']))
    enc=[];add_meta=0;bodies={}
    for m in ms:
        e=enclosing(m['path_rel'],m['span']['start_byte'])
        if e:
            q,k,sl,el,sb,eb=e; add_meta+=len(json.dumps({'enclosing':{'qualified_name':q,'kind':k,'span':[sl,el],'revision':'1'}}))
            enc.append((m['path_rel'].split('/')[-1],m['span']['start']['line']+1,q,eb-sb))
            bodies[(m['path_rel'],sb,eb)]=eb-sb
        else: enc.append((m['path_rel'].split('/')[-1],m['span']['start']['line']+1,None,0))
    small={k:v for k,v in bodies.items() if v<=1500}
    print(f"\nfind text {pattern!r} ci={ci}: {len(ms)} matches, response {base}B")
    for x in enc: print('   ',x)
    print(f"  + enclosing metadata: ~{add_meta}B (~{add_meta/1.9:.0f} tok)")
    print(f"  + enclosing declaration source, only bodies <=1500B, deduped: {sum(small.values())}B in {len(small)} decls (~{sum(small.values())/1.9:.0f} tok); all bodies {sum(bodies.values())}B in {len(bodies)} decls")
# inspect packets the agent would need instead of Reads
print()
for name,args in [('sink_path',['--symbol-name','sink_path']),('append',['--symbol-name','append','--in-resource','']),('TelemetryEvent',['--symbol-name','TelemetryEvent']),('TelemetryEvent::new',['--qualified','TelemetryEvent::new']),('emit_telemetry',['--symbol-name','emit_telemetry'])]:
    if '--in-resource' in args: continue
    r=subprocess.run([R+'/brainprint','inspect']+args+['--budget','standard','--retention','disabled','--workspace',ws,'--json'],capture_output=True,text=True)
    print(f"inspect {name:22} {len(r.stdout):>6}B (~{len(r.stdout)/1.9:.0f} tok)")
