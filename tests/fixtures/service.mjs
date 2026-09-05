// An isolated service for exercising controller workflows without touching real workloads.
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import dgram from 'node:dgram';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const [verb, directory, argument] = process.argv.slice(2);
if (!directory || !path.isAbsolute(directory)) throw new Error('Provide an absolute fixture directory.');
fs.mkdirSync(directory, { recursive: true });
const file = (name) => path.join(directory, name);
const read = (name, fallback = '') => { try { return fs.readFileSync(file(name), 'utf8').trim(); } catch { return fallback; } };
const write = (name, value) => fs.writeFileSync(file(name), String(value));
const metadata = () => { try { return JSON.parse(read('server.json')); } catch { return null; } };
async function healthy() {
  const data = metadata();
  if (!data) return false;
  try {
    const response = await fetch(`http://127.0.0.1:${data.port}/identity`, { signal: AbortSignal.timeout(1000) });
    const identity = await response.json();
    return identity.instance === data.instance && identity.pid === data.pid;
  } catch { return false; }
}
switch (verb) {
  case 'serve': {
    const instance = crypto.randomUUID();
    const udp = dgram.createSocket('udp4');
    udp.on('message', packet => {
      write('wol-count', Number(read('wol-count', '0')) + 1);
      write('wol-last', JSON.stringify({ size: packet.length, sha256: crypto.createHash('sha256').update(packet).digest('hex') }));
    });
    udp.bind(Number(read('udp-port', '0')), '127.0.0.1', () => write('udp-port', udp.address().port));
    const server = http.createServer((request, response) => {
      response.setHeader('Content-Type', 'application/json');
      response.end(JSON.stringify(request.url === '/identity'
        ? { instance, pid: process.pid }
        : { ok: true, busy: read('busy') === 'on', version: read('version', '1.0.0') }));
    });
    server.listen(Number(argument || 0), '127.0.0.1', () => {
      write('server.json', JSON.stringify({ instance, pid: process.pid, port: server.address().port }));
    });
    for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => { udp.close(); server.close(() => process.exit(0)); });
    break;
  }
  case 'start': {
    if (await healthy()) break;
    const output = fs.openSync(file('service.log'), 'a');
    const child = spawn(process.execPath, [fileURLToPath(import.meta.url), 'serve', directory, argument || '0'], {
      detached: true, stdio: ['ignore', output, output], windowsHide: true,
    });
    child.unref();
    fs.closeSync(output);
    for (let attempt = 0; attempt < 50; attempt++) {
      if (await healthy()) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    if (!await healthy()) throw new Error('Fixture did not start.');
    break;
  }
  case 'stop':
    if (await healthy()) process.kill(metadata().pid, 'SIGTERM');
    break;
  case 'running': case 'health':
    process.exitCode = await healthy() ? 0 : 1;
    break;
  case 'slow-busy':
    await new Promise(resolve => setTimeout(resolve, Number(argument || 16000)));
  case 'busy':
    console.log(JSON.stringify({ busy: read('busy') === 'on', reason: read('busy') === 'on' ? 'fixture work is active' : 'idle' }));
    break;
  case 'set-busy':
    if (!['on', 'off'].includes(argument)) throw new Error('Use on or off.');
    write('busy', argument);
    break;
  case 'installed': console.log(read('version', '1.0.0')); break;
  case 'latest': console.log(read('latest', '2.0.0')); break;
  case 'slow-update':
    await new Promise(resolve => setTimeout(resolve, Number(argument || 3000)));
  case 'update':
    write('previous', read('version', '1.0.0'));
    write('version', read('latest', '2.0.0'));
    break;
  case 'noop-update': break;
  case 'fail-update': throw new Error('Requested fixture update failure.');
  case 'rollback': write('version', read('previous', '1.0.0')); break;
  case 'slow-action':
    await new Promise(resolve => setTimeout(resolve, Number(argument || 3000)));
  case 'action':
    write('actions', Number(read('actions', '0')) + 1);
    console.log(`Fixture action completed ${read('actions')} time(s).`);
    break;
  case 'info': console.log(JSON.stringify({ ...metadata(), version: read('version', '1.0.0'), busy: read('busy') === 'on', actions: Number(read('actions', '0')), udpPort: Number(read('udp-port', '0')), wolPackets: Number(read('wol-count', '0')), lastWol: JSON.parse(read('wol-last', 'null')) })); break;
  default: throw new Error('Unknown fixture verb.');
}
