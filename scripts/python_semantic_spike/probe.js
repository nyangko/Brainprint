'use strict';
// #19 task 5 -- Python semantic backend transport probe.
//
// Decides, empirically, which Pyright artifact/protocol Brainprint should
// launch for tasks 6-9. Runs the same capability matrix against both
// `pyright-typeserver` (TSP + LSP) and `pyright-langserver` (LSP only) and
// prints a redacted report; machine-local paths never reach stdout verbatim.
//
//   npm install && node probe.js            # both servers
//   node probe.js tsp                       # one server
//
// Nothing here is a Brainprint runtime dependency. The npm packages are
// installed into this directory only (node_modules is gitignored).

const path = require('path');
const fs = require('fs');
const { execSync } = require('child_process');
const { pathToFileURL } = require('url');
const { Rpc, sleep } = require('./rpc');

const ROOT = path.resolve(__dirname, '..', '..', 'fixtures', 'workspaces', 'python-semantic-spike');
const ROOT_URI = pathToFileURL(ROOT).href;
const MODULES = path.join(__dirname, 'node_modules');
const SERVERS = {
  tsp: path.join(MODULES, 'pyright-typeserver', 'pyright-typeserver.js'),
  lsp: path.join(MODULES, 'pyright', 'langserver.index.js'),
};

const F = (n) => path.join(ROOT, 'pkg', n);
const U = (n) => pathToFileURL(F(n)).href;

// --- redaction: the report must not leak this machine's layout ----------
function redact(value) {
  return JSON.stringify(value === undefined ? null : value)
    .split(pathToFileURL(MODULES).href).join('<MODULES>')
    .split(ROOT_URI).join('<FIXTURE>')
    .split(MODULES).join('<MODULES>')
    .split(ROOT).join('<FIXTURE>')
    .replace(/file:\/\/\/[^"]*?\/site-packages/g, '<SITE-PACKAGES>')
    .replace(/file:\/\/\/[^"]*?\/(lib\/python[\d.]+)/g, '<PYTHON-LIB>')
    .replace(/file:\/\/\/[A-Za-z0-9_\-./]*/g, '<PATH>');
}

// --- source coordinates -------------------------------------------------
// Report the protocol coordinate (0-based line, UTF-16 code unit) next to
// the byte offset Brainprint's SourceSpan uses, so the two conventions can
// be compared on the same token.
function locate(file, needle, nth = 0) {
  const text = fs.readFileSync(file, 'utf8');
  let idx = -1;
  for (let i = 0; i <= nth; i++) {
    idx = text.indexOf(needle, idx + 1);
    if (idx < 0) throw new Error(`not found: ${needle} #${nth} in ${file}`);
  }
  const before = text.slice(0, idx);
  const line = before.split('\n').length - 1;
  const lineStart = before.lastIndexOf('\n') + 1;
  const character = idx - lineStart; // JS string index == UTF-16 code units
  const prefix = text.slice(lineStart, idx);
  return {
    uri: pathToFileURL(file).href,
    line,
    character,
    range: {
      start: { line, character },
      end: { line, character: character + needle.length },
    },
    utf16: character,
    codepoints: Array.from(prefix).length,
    utf8Bytes: Buffer.byteLength(prefix, 'utf8'),
  };
}

const rss = (pid) => {
  try {
    return parseInt(execSync(`ps -o rss= -p ${pid}`).toString().trim(), 10);
  } catch {
    return null;
  }
};

async function boot(which, capabilities) {
  const spawnedAt = Date.now();
  const rpc = new Rpc(process.execPath, [SERVERS[which], '--stdio'], { cwd: ROOT });
  rpc.snapshots = [];
  rpc.diagnostics = [];
  rpc.serverRequests = [];
  let lastSnapshotAt = Date.now();
  const dispatch = rpc._dispatch.bind(rpc);
  rpc._dispatch = (m) => {
    if (m.id !== undefined && m.method) rpc.serverRequests.push(m.method);
    dispatch(m);
  };
  rpc.on('typeServer/snapshotChanged', (p) => {
    rpc.snapshots.push(p);
    lastSnapshotAt = Date.now();
  });
  rpc.on('textDocument/publishDiagnostics', (p) =>
    rpc.diagnostics.push({ at: Date.now() - spawnedAt, count: (p.diagnostics || []).length }));

  const init = await rpc.request('initialize', {
    processId: process.pid,
    rootUri: ROOT_URI,
    workspaceFolders: [{ uri: ROOT_URI, name: 'python-semantic-spike' }],
    capabilities,
  }, 60000);
  rpc.notify('initialized', {});

  // "Ready" is when the snapshot stops moving (TSP) -- the language server
  // has no equivalent signal, so it gets a fixed settle window.
  if (which === 'tsp') {
    for (let i = 0; i < 150; i++) {
      await sleep(100);
      if (rpc.snapshots.length && Date.now() - lastSnapshotAt > 800) break;
    }
  } else {
    await sleep(2500);
  }
  return { rpc, init, initMs: init.ms, readyMs: Date.now() - spawnedAt };
}

const CLIENT_CAPABILITIES = {
  general: { positionEncodings: ['utf-16'] },
  workspace: {
    configuration: true,
    workspaceFolders: true,
    didChangeWatchedFiles: { dynamicRegistration: true },
  },
  textDocument: {
    synchronization: { dynamicRegistration: false },
    publishDiagnostics: {},
    definition: {}, declaration: {}, typeDefinition: {}, references: {},
    implementation: {}, callHierarchy: {}, typeHierarchy: {}, hover: {},
  },
  experimental: { typeServerMultiConnection: { supportedTransports: ['ipc'] } },
};

async function probe(which) {
  const report = { server: which, steps: [] };
  const rec = (name, v) => { report.steps.push({ name, ...v }); return v; };

  const { rpc, init, initMs, readyMs } = await boot(which, CLIENT_CAPABILITIES);
  report.initializeMs = Math.round(initMs);
  report.readyMs = readyMs;
  report.positionEncoding = init.result.capabilities.positionEncoding ?? null; // null => LSP default, UTF-16
  report.providers = Object.keys(init.result.capabilities).filter((k) => /Provider$/.test(k));

  // ---- TSP surface ----------------------------------------------------
  const snap = async () => (await rpc.request('typeServer/getSnapshot', undefined, 20000)).result;
  // Snapshots rotate while background analysis runs, so every TSP query
  // needs this retry: a stale snapshot is rejected, never answered.
  let staleRetries = 0;
  async function withSnapshot(method, mkParams, tries = 6) {
    let last;
    for (let i = 0; i < tries; i++) {
      const s = await snap();
      last = await rpc.request(method, mkParams(s), 30000);
      if (!last.error || last.error.code !== -32802) return last;
      staleRetries++;
      await sleep(150);
    }
    return last;
  }

  rec('typeServer/getSupportedProtocolVersion',
    await rpc.request('typeServer/getSupportedProtocolVersion', undefined, 20000));
  rec('typeServer/getSnapshot', await rpc.request('typeServer/getSnapshot', undefined, 20000));

  if (which === 'tsp') {
    rec('typeServer/getPythonSearchPaths', await withSnapshot(
      'typeServer/getPythonSearchPaths', (s) => ({ fromUri: ROOT_URI, snapshot: s })));

    for (const [label, moduleDescriptor] of [
      ['relative .base', { leadingDots: 1, nameParts: ['base'] }],
      ['absolute pkg.base', { leadingDots: 0, nameParts: ['pkg', 'base'] }],
      ['stdlib json', { leadingDots: 0, nameParts: ['json'] }],
      ['external requests', { leadingDots: 0, nameParts: ['requests'] }],
      ['missing module', { leadingDots: 0, nameParts: ['zzz_not_a_module'] }],
    ]) {
      rec(`typeServer/resolveImport (${label})`, await withSnapshot(
        'typeServer/resolveImport',
        (s) => ({ sourceUri: U('impl.py'), moduleDescriptor, snapshot: s })));
    }

    for (const [label, file, needle, nth, width] of [
      ['parameter x in def call', 'impl.py', 'x: Base', 0, 1],
      ['method run in x.run(1)', 'impl.py', 'run', 1, 3],
      ['literal argument 1', 'impl.py', 'value', 1, 5],
      ['class Impl', 'impl.py', 'Impl', 0, 4],
      ['non-ASCII class', 'unicode_case.py', '한글클래스', 0, 5],
    ]) {
      const loc = locate(F(file), needle, nth);
      const arg = {
        uri: loc.uri,
        range: { start: loc.range.start, end: { line: loc.line, character: loc.character + width } },
      };
      for (const m of ['getDeclaredType', 'getComputedType', 'getExpectedType']) {
        const r = await withSnapshot(`typeServer/${m}`, (s) => ({ arg, snapshot: s }));
        rec(`typeServer/${m} (${label})`, r);
      }
    }
    rec('typeServer/connection (multi-connection)',
      await rpc.request('typeServer/connection', { type: 'open', kind: 'ipc' }, 15000));
  }
  rec('unknown method', await rpc.request('typeServer/thisMethodDoesNotExist', {}, 10000));

  // ---- navigation surface (identical request set on both servers) -----
  for (const [label, method, params] of [
    ['definition at call site', 'textDocument/definition',
      { textDocument: { uri: U('impl.py') }, position: locate(F('impl.py'), 'run', 1).range.start }],
    ['declaration', 'textDocument/declaration',
      { textDocument: { uri: U('impl.py') }, position: locate(F('impl.py'), 'x.run(1)').range.start }],
    ['typeDefinition', 'textDocument/typeDefinition',
      { textDocument: { uri: U('impl.py') }, position: locate(F('impl.py'), 'x.run(1)').range.start }],
    ['references to Base', 'textDocument/references',
      { textDocument: { uri: U('base.py') }, position: locate(F('base.py'), 'Base').range.start, context: { includeDeclaration: true } }],
    ['references to Base.run', 'textDocument/references',
      { textDocument: { uri: U('base.py') }, position: locate(F('base.py'), 'run').range.start, context: { includeDeclaration: true } }],
    ['implementation of Base.run', 'textDocument/implementation',
      { textDocument: { uri: U('base.py') }, position: locate(F('base.py'), 'run').range.start }],
    ['type hierarchy of Impl', 'textDocument/prepareTypeHierarchy',
      { textDocument: { uri: U('impl.py') }, position: locate(F('impl.py'), 'Impl').range.start }],
  ]) {
    const r = await rpc.request(method, params, 25000);
    rec(`${method} (${label})`, { ms: r.ms, error: r.error, result: r.result });
  }

  const prepared = await rpc.request('textDocument/prepareCallHierarchy',
    { textDocument: { uri: U('base.py') }, position: locate(F('base.py'), 'run').range.start }, 25000);
  rec('textDocument/prepareCallHierarchy (Base.run)', prepared);
  if (Array.isArray(prepared.result) && prepared.result.length) {
    rec('callHierarchy/incomingCalls',
      await rpc.request('callHierarchy/incomingCalls', { item: prepared.result[0] }, 25000));
    rec('callHierarchy/outgoingCalls',
      await rpc.request('callHierarchy/outgoingCalls', { item: prepared.result[0] }, 25000));
  }

  // ---- source coordinates: UTF-16 vs codepoint vs byte -----------------
  // wide.py line 6 puts an identifier behind two surrogate-pair emoji, so
  // the three conventions disagree. A byte offset does not merely miss --
  // it lands on a *different real symbol* and answers confidently.
  const wide = locate(F('wide.py'), '파라미터.run(4)');
  report.coordinates = { line: wide.line, utf16: wide.utf16, codepoints: wide.codepoints, utf8Bytes: wide.utf8Bytes };
  for (const [label, character] of [
    ['UTF-16 code units (LSP convention)', wide.utf16],
    ['Unicode codepoints', wide.codepoints],
    ['UTF-8 bytes (Brainprint SourceSpan convention)', wide.utf8Bytes],
  ]) {
    const r = await rpc.request('textDocument/hover',
      { textDocument: { uri: U('wide.py') }, position: { line: wide.line, character } }, 20000);
    rec(`coordinate probe: ${label} (character=${character})`, r);
  }
  const hovered = await rpc.request('textDocument/hover',
    { textDocument: { uri: U('wide.py') }, position: { line: wide.line, character: wide.utf16 } }, 20000);
  if (hovered.result && hovered.result.range) {
    const lineText = fs.readFileSync(F('wide.py'), 'utf8').split('\n')[wide.line];
    const { start, end } = hovered.result.range;
    report.returnedRangeDecoding = {
      range: hovered.result.range,
      asUtf16: lineText.slice(start.character, end.character),
      asCodepoints: Array.from(lineText).slice(start.character, end.character).join(''),
      asBytes: Buffer.from(lineText, 'utf8').slice(start.character, end.character).toString('utf8'),
    };
  }

  // ---- filesystem truth vs open-document overlay ----------------------
  const implText = fs.readFileSync(F('impl.py'), 'utf8');
  rec('hover before any didOpen (filesystem truth)', await rpc.request('textDocument/hover',
    { textDocument: { uri: U('impl.py') }, position: locate(F('impl.py'), 'x.run(1)').range.start }, 20000));
  rpc.notify('textDocument/didOpen', {
    textDocument: {
      uri: U('impl.py'), languageId: 'python', version: 1,
      text: implText.replace('def call(x: Base):', 'def call(x: int):'),
    },
  });
  await sleep(1500);
  rec('hover under didOpen overlay', await rpc.request('textDocument/hover',
    { textDocument: { uri: U('impl.py') }, position: locate(F('impl.py'), 'x.run(1)').range.start }, 20000));
  rpc.notify('textDocument/didClose', { textDocument: { uri: U('impl.py') } });
  await sleep(1500);
  rec('hover after didClose (back to filesystem)', await rpc.request('textDocument/hover',
    { textDocument: { uri: U('impl.py') }, position: locate(F('impl.py'), 'x.run(1)').range.start }, 20000));

  // ---- snapshot currentness / stale rejection -------------------------
  if (which === 'tsp') {
    const before = await snap();
    rpc.notify('textDocument/didOpen', {
      textDocument: { uri: U('impl.py'), languageId: 'python', version: 1, text: `${implText}\nEXTRA = 1\n` },
    });
    await sleep(1500);
    const after = await snap();
    rec('snapshot moved on document change', { result: { before, after, changed: before !== after } });
    rec('stale snapshot: getComputedType', await rpc.request('typeServer/getComputedType',
      { arg: { uri: U('impl.py'), range: locate(F('impl.py'), 'x: Base').range }, snapshot: before }, 15000));
    rec('stale snapshot: resolveImport', await rpc.request('typeServer/resolveImport',
      { sourceUri: U('impl.py'), moduleDescriptor: { leadingDots: 1, nameParts: ['base'] }, snapshot: before }, 15000));
    rec('never-issued snapshot', await rpc.request('typeServer/resolveImport',
      { sourceUri: U('impl.py'), moduleDescriptor: { leadingDots: 1, nameParts: ['base'] }, snapshot: after + 1000 }, 15000));
    rec('current snapshot after stale rejections', await rpc.request('typeServer/resolveImport',
      { sourceUri: U('impl.py'), moduleDescriptor: { leadingDots: 1, nameParts: ['base'] }, snapshot: after }, 15000));
    rpc.notify('textDocument/didClose', { textDocument: { uri: U('impl.py') } });
    await sleep(800);
  }

  // ---- a file that only ever existed on disk ---------------------------
  const onDisk = F('_disk_only_case.py');
  fs.writeFileSync(onDisk, 'from .base import Base\n\n\ndef disk_case(x: Base) -> str:\n    return x.run(3)\n');
  rpc.notify('workspace/didChangeWatchedFiles', { changes: [{ uri: pathToFileURL(onDisk).href, type: 1 }] });
  await sleep(2500);
  rec('definition in a file never sent via didOpen', await rpc.request('textDocument/definition',
    { textDocument: { uri: pathToFileURL(onDisk).href }, position: locate(onDisk, 'run', 0).range.start }, 20000));
  fs.unlinkSync(onDisk);
  rpc.notify('workspace/didChangeWatchedFiles', { changes: [{ uri: pathToFileURL(onDisk).href, type: 3 }] });
  await sleep(800);

  // ---- warm latency ----------------------------------------------------
  const warm = [];
  for (let i = 0; i < 20; i++) {
    const r = await rpc.request('textDocument/definition',
      { textDocument: { uri: U('impl.py') }, position: locate(F('impl.py'), 'run', 1).range.start }, 20000);
    warm.push(Math.round(r.ms * 1000) / 1000);
  }
  report.warmDefinitionMs = { samples: warm, median: median(warm) };
  if (which === 'tsp') {
    const warmType = [];
    for (let i = 0; i < 20; i++) {
      const r = await withSnapshot('typeServer/getDeclaredType',
        (s) => ({ arg: { uri: U('impl.py'), range: locate(F('impl.py'), 'x: Base').range }, snapshot: s }));
      warmType.push(Math.round(r.ms * 1000) / 1000);
    }
    report.warmDeclaredTypeMs = { samples: warmType, median: median(warmType) };
  }

  // ---- cancellation ----------------------------------------------------
  const cancelId = rpc.nextId;
  const inFlight = rpc.request('textDocument/references',
    { textDocument: { uri: U('base.py') }, position: locate(F('base.py'), 'run').range.start, context: { includeDeclaration: true } }, 25000);
  rpc.cancel(cancelId);
  const cancelled = await inFlight;
  rec('$/cancelRequest on an in-flight references request', {
    ms: cancelled.ms,
    error: cancelled.error,
    result: Array.isArray(cancelled.result) ? { returnedLocations: cancelled.result.length } : cancelled.result,
  });
  rec('request after cancel', await rpc.request('textDocument/definition',
    { textDocument: { uri: U('impl.py') }, position: locate(F('impl.py'), 'run', 1).range.start }, 20000));

  // ---- malformed input -------------------------------------------------
  rpc.proc.stdin.write('Content-Length: 12\r\n\r\n{not json}\r\n');
  await sleep(600);
  rec('after a malformed frame', { result: { processAlive: !rpc.exited } });
  rec('request after a malformed frame', await rpc.request('textDocument/definition',
    { textDocument: { uri: U('impl.py') }, position: locate(F('impl.py'), 'run', 1).range.start }, 20000));

  report.configurationSections = Array.from(new Set(
    rpc.serverRequests.filter((m) => m === 'workspace/configuration')));
  report.serverToClientRequests = Array.from(new Set(rpc.serverRequests));
  report.snapshotChanges = rpc.snapshots.length;
  report.staleRetries = staleRetries;
  report.diagnosticsPublished = rpc.diagnostics.length;
  report.rssKb = rss(rpc.proc.pid);
  report.processCount = pyrightProcessCount();
  report.stderr = rpc.stderr.slice(0, 2000);

  rec('shutdown', await rpc.request('shutdown', undefined, 15000));
  rpc.notify('exit', undefined);
  await sleep(1500);
  report.cleanExit = rpc.exited;
  if (!rpc.exited) { rpc.proc.kill('SIGKILL'); report.cleanExit = 'did not exit; killed'; }
  return report;
}

function median(xs) {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.floor(s.length / 2)];
}
function pyrightProcessCount() {
  try {
    return execSync("pgrep -f 'pyright' || true").toString().trim().split('\n').filter(Boolean).length;
  } catch {
    return null;
  }
}

function printReport(r) {
  console.log(`\n${'='.repeat(72)}\n== ${r.server}  initialize ${r.initializeMs}ms  ready ${r.readyMs}ms  rss ${r.rssKb}kB  pyright processes ${r.processCount}`);
  console.log(`== positionEncoding ${redact(r.positionEncoding)} (null = LSP default, UTF-16)`);
  console.log(`== providers: ${r.providers.join(', ')}`);
  console.log(`== snapshot changes ${r.snapshotChanges}, stale retries ${r.staleRetries}, diagnostics published ${r.diagnosticsPublished}`);
  console.log(`== server->client: ${r.serverToClientRequests.join(', ') || '(none)'}`);
  console.log(`== coordinates on wide.py: ${redact(r.coordinates)}`);
  console.log(`== returned range decoded: ${redact(r.returnedRangeDecoding)}`);
  console.log(`== warm definition median ${r.warmDefinitionMs.median}ms${r.warmDeclaredTypeMs ? `, warm declaredType median ${r.warmDeclaredTypeMs.median}ms` : ''}`);
  console.log(`== clean exit: ${redact(r.cleanExit)}`);
  if (r.stderr) console.log(`== stderr: ${r.stderr.slice(0, 400)}`);
  for (const s of r.steps) {
    const err = s.error ? ` ERROR ${redact(s.error)}` : '';
    console.log(`\n  ${s.name}  [${s.ms === undefined ? '-' : Math.round(s.ms)}ms]${err}`);
    if (!s.error) console.log(`    -> ${redact(s.result).slice(0, 400)}`);
  }
}

(async () => {
  const wanted = process.argv.slice(2).filter((a) => SERVERS[a]);
  const targets = wanted.length ? wanted : ['tsp', 'lsp'];
  for (const t of targets) {
    if (!fs.existsSync(SERVERS[t])) {
      console.error(`missing ${t} server at ${SERVERS[t]} -- run \`npm install\` in ${__dirname}`);
      process.exit(1);
    }
  }
  for (const t of targets) printReport(await probe(t));
  process.exit(0);
})().catch((e) => { console.error('PROBE FAILED', e && e.stack); process.exit(1); });
