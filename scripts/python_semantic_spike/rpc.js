'use strict';
const { spawn } = require('child_process');

// Minimal LSP-style (Content-Length framed) JSON-RPC stdio client.
class Rpc {
  constructor(cmd, args, opts = {}) {
    this.proc = spawn(cmd, args, { stdio: ['pipe', 'pipe', 'pipe'], ...opts });
    this.nextId = 1;
    this.pending = new Map();
    this.notifications = [];
    this.handlers = new Map();
    this.stderr = '';
    this.exited = null;
    this.buf = Buffer.alloc(0);
    this.proc.stdout.on('data', (d) => this._onData(d));
    this.proc.stderr.on('data', (d) => { this.stderr += d.toString(); });
    this.proc.on('exit', (code, sig) => { this.exited = { code, sig }; });
  }
  _onData(d) {
    this.buf = Buffer.concat([this.buf, d]);
    for (;;) {
      const sep = this.buf.indexOf('\r\n\r\n');
      if (sep < 0) return;
      const header = this.buf.slice(0, sep).toString('ascii');
      const m = /Content-Length: (\d+)/i.exec(header);
      if (!m) { this.buf = this.buf.slice(sep + 4); continue; }
      const len = parseInt(m[1], 10);
      if (this.buf.length < sep + 4 + len) return;
      const body = this.buf.slice(sep + 4, sep + 4 + len).toString('utf8');
      this.buf = this.buf.slice(sep + 4 + len);
      let msg;
      try { msg = JSON.parse(body); } catch { continue; }
      this._dispatch(msg);
    }
  }
  _dispatch(msg) {
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
      const p = this.pending.get(msg.id);
      if (p) { this.pending.delete(msg.id); p(msg); }
      return;
    }
    if (msg.id !== undefined && msg.method) {
      // Server->client request: answer null so the server never blocks.
      this._send({ jsonrpc: '2.0', id: msg.id, result: null });
      return;
    }
    this.notifications.push(msg);
    const h = this.handlers.get(msg.method);
    if (h) h(msg.params);
  }
  _send(o) {
    const s = JSON.stringify(o);
    const b = Buffer.from(s, 'utf8');
    this.proc.stdin.write(`Content-Length: ${b.length}\r\n\r\n`);
    this.proc.stdin.write(b);
  }
  on(method, fn) { this.handlers.set(method, fn); }
  notify(method, params) { this._send({ jsonrpc: '2.0', method, params }); }
  request(method, params, timeoutMs = 60000) {
    const id = this.nextId++;
    const started = process.hrtime.bigint();
    return new Promise((resolve) => {
      const t = setTimeout(() => {
        this.pending.delete(id);
        resolve({ id, method, timedOut: true, ms: ms(started) });
      }, timeoutMs);
      this.pending.set(id, (msg) => {
        clearTimeout(t);
        resolve({ id, method, ms: ms(started), result: msg.result, error: msg.error });
      });
      this._send({ jsonrpc: '2.0', id, method, params });
    });
  }
  cancel(id) { this.notify('$/cancelRequest', { id }); }
  async kill() { this.proc.kill('SIGKILL'); }
}
function ms(startedHr) { return Number(process.hrtime.bigint() - startedHr) / 1e6; }
const sleep = (n) => new Promise((r) => setTimeout(r, n));
module.exports = { Rpc, sleep };
