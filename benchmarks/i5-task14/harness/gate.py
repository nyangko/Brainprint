"""I5 Task 14 final rerun: correctness/currentness product-path gate (product CLI/MCP/agent only).
usage (inside the isolated env.sh): python3 gate.py <workspace> <out.json>
Every check prints one line and lands in <out.json>. Exits 1 when any check fails."""
import json, os, subprocess, sys, time

R, SP = os.environ['R'], os.environ['SP']
W, OUT = sys.argv[1], sys.argv[2]
CFG = W + '/crates/engine/src/config.rs'
T1_SITES = {215, 430, 714, 551, 482, 498, 508, 561, 574, 586}
res = []


def cli(*a, inp=None, ws=True):
    t = time.time()
    r = subprocess.run([R + '/brainprint', *a] + (['--workspace', W] if ws else []) + ['--json'],
                       capture_output=True, text=True, input=inp)
    try:
        d = json.loads(r.stdout)
    except ValueError:
        d = {'raw': (r.stdout + r.stderr)[:300]}
    return d, round((time.time() - t) * 1000), len(r.stdout)


def check(name, ok, detail):
    res.append({'check': name, 'pass': bool(ok), 'detail': detail})
    print(('PASS ' if ok else 'FAIL ') + name + ' :: ' + json.dumps(detail)[:300])


def daemon(action):
    if action in ('stop', 'restart'):
        subprocess.run(['kill', open(SP + '/ab/daemon.pid').read().strip()])
        time.sleep(1.2)
    if action in ('start', 'restart'):
        subprocess.run(['python3', SP + '/ab/startd.py'], capture_output=True)


def ok(d):
    return d.get('outcome', {}).get('Ok')


def find_sym(name):
    d, ms, n = cli('find', 'target', '--symbol-name', name, '--budget', 'compact', '--retention', 'disabled')
    o = ok(d) or {}
    s = json.dumps(o)
    found = ('"name": "%s"' % name) in s
    cur = 'Current' if '"currentness": "Current"' in s else ('NotCurrent' if 'NotCurrent' in s else 'other')
    return found, cur, ms, d


def callers():
    d, ms, n = cli('relations', '--symbol-name', 'load_workspace_config', '--direction', 'incoming')
    r = ok(d)
    if not r:
        return None, None, ms, json.dumps(d)[:200]
    a = r['Relations']['answers'][0]
    lines = {e['span']['start']['line'] + 1 for c in a['confirmed'] for e in c['evidence']
             if e['occurrence_kind'] in ('CallSite', 'CallCandidateSite')}
    return r['Relations']['currentness'], lines, ms, a['coverage']


def inspect_page(tok=None):
    a = ['inspect', '--symbol-name', 'load_workspace_config', '--budget', 'compact', '--budget-items', '4', '--retention', 'disabled']
    if tok:
        a += ['--continuation', tok]
    d, ms, n = cli(*a)
    return d


def wait(pred, limit=15):
    t = time.time()
    while time.time() - t < limit:
        v = pred()
        if v:
            return round((time.time() - t) * 1000), v
        time.sleep(0.05)
    return None, None


# 1 fresh init / first query activates the runtime and the rust backend
st = subprocess.run([R + '/brainprint', 'status', W], capture_output=True, text=True).stdout
found, cur, ms, _ = find_sym('load_workspace_config')
check('fresh: find target Current', found and cur == 'Current', {'ms': ms})
cur, lines, ms, cov = callers()
check('fresh: T1 callers complete (4 prod + 6 test), no false call', cur == 'Current' and lines == T1_SITES,
      {'ms': ms, 'missing': sorted(T1_SITES - (lines or set())), 'extra': sorted((lines or set()) - T1_SITES), 'coverage': cov})
st2 = subprocess.run([R + '/brainprint', 'status', W], capture_output=True, text=True).stdout
check('backend asleep -> activated by query', 'rust' in st2,
      {'before': [l.strip() for l in st.splitlines() if 'rust ' in l or 'runtime' in l],
       'after': [l.strip() for l in st2.splitlines() if 'rust ' in l or 'runtime' in l]})

# 2 the ten product operations answer Current
ops = {
    'find files': ['find', 'files', '--path-prefix', 'crates/agent/src'],
    'find text': ['find', 'text', '--literal', 'BRAINPRINT_ADOPTION_TELEMETRY_PATH', '--search-budget', 'standard'],
    'inspect': ['inspect', '--symbol-name', 'load_workspace_config', '--budget', 'standard', '--retention', 'disabled'],
    'relations outgoing': ['relations', '--symbol-name', 'load_workspace_config', '--direction', 'outgoing'],
    'impact public-signature': ['impact', '--symbol-name', 'load_workspace_config', '--change', 'public-signature', '--budget', 'standard', '--retention', 'disabled'],
    'context change': ['context', 'change', '--symbol-name', 'load_workspace_config', '--budget', 'standard', '--retention', 'disabled'],
    'knowledge rules': ['knowledge', 'rules'],
    'structure': ['structure', '--group-directory', '--root', 'crates', '--depth', '2'],
}
for name, a in ops.items():
    d, ms, n = cli(*a)
    s = json.dumps(d)
    check('op: ' + name, ok(d) is not None and 'NotCurrent' not in s and '"Stale"' not in s,
          {'ms': ms, 'bytes': n, 'current': '"Current"' in s, 'err': None if ok(d) else s[:200]})

# 3 same revision continuation, source change invalidates it, new symbol visible, revert
o = ok(inspect_page())['Inspect']
import base64
tok = base64.urlsafe_b64encode(json.dumps(o['continuation'], separators=(',', ':')).encode()).decode().rstrip('=') if o.get('continuation') else None
d2 = inspect_page(tok) if tok else {}
check('same revision: continuation accepted', tok and ok(d2) is not None, {'more_available': o.get('more_available'), 'page2': json.dumps(d2)[:120]})
orig = open(CFG).read()
open(CFG, 'w').write(orig + '\npub fn t14_new_symbol() {}\n')
ms, _ = wait(lambda: find_sym('t14_new_symbol')[:2] == (True, 'Current'))
d3 = inspect_page(tok)
check('source change: new symbol Current', ms is not None, {'visible_after_ms': ms})
check('source change: old continuation rejected', ok(d3) is None, {'resp': json.dumps(d3)[:200]})
open(CFG, 'w').write(orig)
ms, _ = wait(lambda: find_sym('t14_new_symbol')[0] is False)
check('revert: deleted symbol gone', ms is not None, {'after_ms': ms})

# 4 daemon restart keeps truth
daemon('restart')
found, cur, ms, _ = find_sym('load_workspace_config')
c, lines, ms2, _ = callers()
check('daemon restart: Current, callers complete', found and cur == 'Current' and lines == T1_SITES, {'find_ms': ms, 'callers_ms': ms2})

# 5 offline edit: daemon stopped, file edited, daemon started: never the old truth as Current
daemon('stop')
open(CFG, 'w').write(orig + '\npub fn t14_offline_symbol() {}\n')
daemon('start')
found, cur, ms, d = find_sym('t14_offline_symbol')
check('offline edit: first answer sees edit or is not Current', (found and cur == 'Current') or cur != 'Current',
      {'found': found, 'currentness': cur, 'ms': ms})
open(CFG, 'w').write(orig)
ms, _ = wait(lambda: find_sym('t14_offline_symbol')[0] is False)
check('offline edit revert visible', ms is not None, {'after_ms': ms})

# 6 Working State: start -> partial handoff -> work-items -> resume
start = {'work_item': {'New': {'source_kind': 'UserRequest', 'source_ref': None, 'title': 'gate', 'goal': 'gate check'}},
         'head': None, 'git': 'Observe', 'owner_agent': None}
d, ms, n = cli('work', 'start', inp=json.dumps(start))
wi = d.get('Started', {}).get('working_state', {}).get('work_item')
check('work start', wi is not None, {'resp': json.dumps(d)[:200]})
d, ms, n = cli('work', 'result', inp=json.dumps({'work_item': wi, 'outcome': 'Partial', 'summary': 'gate handoff', 'commit_id': None,
                                                  'verification_summary': None, 'verification': None, 'git': 'Observe', 'change_set': None}))
check('work result partial', 'Recorded' in json.dumps(d)[:50] or 'Recorded' in d, {'resp': json.dumps(d)[:200]})
d, ms, n = cli('context', 'resume', '--work-item', wi, '--budget', 'standard', '--retention', 'disabled')
check('context resume returns handoff', ok(d) is not None and 'gate handoff' in json.dumps(d), {'ms': ms, 'bytes': n})
d, ms, n = cli('work', 'result', inp=json.dumps({'work_item': wi, 'outcome': 'Abandon', 'summary': 'gate end', 'commit_id': None,
                                                  'verification_summary': None, 'verification': None, 'git': 'Observe', 'change_set': None}))

# 7 MCP
sys.path.insert(0, SP + '/ab')
import mcpc
m, r = mcpc.start(R + '/brainprint-mcp', W)
tl = m.rpc('tools/list', {})['result']['tools']
c = m.rpc('tools/call', {'name': tl[0]['name'] if False else 'brainprint.find',
                         'arguments': {'mode': 'target', 'symbol_name': 'load_workspace_config', 'workspace_path': W}})['result']
check('MCP: 4 tools, find answers Current', len(tl) == 4 and not c.get('isError') and 'Current' in json.dumps(c),
      {'tools': [t['name'] for t in tl], 'tools_list_bytes': len(json.dumps(tl)), 'instructions_bytes': len(r['result'].get('instructions') or '')})

# 8 Agent integration: probe + a PreToolUse bridge event in prefer mode
p = subprocess.run([R + '/brainprint-agent', 'probe', '--client', 'claude-code'], capture_output=True, text=True)
hook = {'session_id': 'gate', 'hook_event_name': 'PreToolUse', 'cwd': W, 'tool_name': 'Grep',
        'tool_input': {'pattern': 'load_workspace_config', 'path': W}}
b = subprocess.run([R + '/brainprint-agent', 'bridge', '--client', 'claude-code', '--event', 'PreToolUse', '--mode', 'prefer'],
                   capture_output=True, text=True, input=json.dumps(hook), cwd=W)
check('agent: bridge PreToolUse answers (exit 0)', b.returncode == 0, {'probe_exit': p.returncode, 'probe': (p.stdout + p.stderr)[:200], 'bridge': (b.stdout + b.stderr)[:300]})

# 9 unsupported/degraded: rust backend locator removed -> callers must not claim complete
gc = os.environ['HOME'] + '/.brainprint/config.toml'
g = open(gc).read()
open(gc, 'w').write('format_version = 1\n')
daemon('restart')
c, lines, ms, cov = callers()
degraded_honest = c != 'Current' or lines == T1_SITES or (isinstance(cov, dict) and (cov.get('unconfirmed_owners') or cov.get('requires_semantics') or cov.get('gaps') or cov.get('semantic', {}).get('not_current')))
check('degraded (no rust backend): no false complete', degraded_honest,
      {'currentness': c, 'sites': len(lines or ()), 'missing': sorted(T1_SITES - (lines or set())), 'coverage': cov})
open(gc, 'w').write(g)
daemon('restart')
c, lines, ms, _ = callers()
check('backend restored: callers complete again', lines == T1_SITES, {'ms': ms})

check('workspace clean after gate', subprocess.run(['git', '-C', W, 'status', '--porcelain', '--untracked-files=no'], capture_output=True, text=True).stdout.strip() == '', {})
json.dump(res, open(OUT, 'w'), indent=1)
sys.exit(0 if all(r['pass'] for r in res) else 1)
