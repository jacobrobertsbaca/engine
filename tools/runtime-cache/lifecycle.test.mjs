import { build } from 'esbuild';
import assert from 'node:assert/strict';
import { test } from 'node:test';

class FakeWorker {
  listeners = new Map();
  messages = [];
  terminated = false;
  addEventListener(type, callback) {
    const values = this.listeners.get(type) ?? new Set();
    values.add(callback);
    this.listeners.set(type, values);
  }
  removeEventListener(type, callback) {
    this.listeners.get(type)?.delete(callback);
  }
  postMessage(value) {
    this.messages.push(structuredClone(value));
  }
  terminate() {
    this.terminated = true;
  }
  emit(data) {
    for (const callback of [...(this.listeners.get('message') ?? [])])
      if (this.listeners.get('message').has(callback)) callback({ data });
  }
  count() {
    return [...this.listeners.values()].reduce((sum, values) => sum + values.size, 0);
  }
}
const workers = [];
globalThis.__engineTestWorker = () => {
  const worker = new FakeWorker();
  workers.push(worker);
  return worker;
};
const bundle = await build({
  entryPoints: ['src/ts/index.ts'],
  bundle: true,
  write: false,
  platform: 'node',
  format: 'esm',
  plugins: [
    {
      name: 'internal-test-construction',
      setup(build) {
        build.onResolve({ filter: /pkg\/engine(?:\.js)?$/ }, () => ({
          path: 'bindings',
          namespace: 'test'
        }));
        build.onResolve({ filter: /\.wasm$/ }, () => ({ path: 'binary', namespace: 'test' }));
        build.onResolve({ filter: /\.\/engine-worker$/ }, () => ({
          path: 'worker',
          namespace: 'test'
        }));
        build.onLoad({ filter: /.*/, namespace: 'test' }, ({ path }) => ({
          contents:
            path === 'worker'
              ? 'export const createEngineWorker = () => globalThis.__engineTestWorker();'
              : path === 'binary'
                ? 'export default new Uint8Array();'
                : `export default async function init() {};
         export const prefetch_urls = () => [];
         export class DapAdapter {
           attach(worker) { this.worker = worker; this.listener = e => {
             if (e.data.type === 'stop') this.callback?.({event:'terminated'});
           }; worker.addEventListener('message', this.listener); }
           detach() { this.worker?.removeEventListener('message', this.listener); }
           resetRun() {} on(callback) { this.callback = callback; }
           sendMessage() {} setFilterInternals() {}
         }`
        }));
      }
    }
  ]
});
const { Engine } = await import(
  `data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`
);
const options = (extra = {}) => ({
  cpp: {
    scope: { projectId: 'p', accountId: 'a' },
    objectCache: true,
    retainWorker: true,
    idleTimeoutMs: 1000,
    ...extra
  }
});
const complete = (worker) => worker.emit({ type: 'stop', exit_code: 0, build_ms: 1, run_ms: 1 });
const source = (key) => ({
  source: '/main.cpp',
  key: key.repeat(64),
  object_hash: 'a'.repeat(64),
  object: new Uint8Array([0, 97, 115, 109])
});
const record = (worker, engine) =>
  worker.emit({
    type: 'cpp_cache_object',
    generation: engine.cacheStats.generation,
    ...source('b')
  });

test('snapshot and mode captured before readiness; one job active', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  engine.fs = { 'main.cpp': 'first' };
  engine.binaryFiles = { '/x.a': new Uint8Array([1]) };
  const job = engine.run();
  const worker = workers.at(-1);
  engine.fs['main.cpp'] = 'later';
  engine.binaryFiles['/x.a'][0] = 2;
  engine.debugger.enabled = true;
  assert.equal((await engine.compile()).type, 'error');
  worker.emit({ type: 'ready' });
  const start = worker.messages.at(-1);
  assert.equal(start.fs['main.cpp'], 'first');
  assert.equal(start.binary_files['/x.a'][0], 1);
  assert.equal(start.is_debug, false);
  complete(worker);
  await job;
  await engine.dispose();
});

test('immediate Stop before Ready fences dispatch and late output', async () => {
  const engine = await Engine.create('c', options());
  let output = 0;
  engine.stdout.on('data', () => output++);
  const job = engine.run();
  const worker = workers.at(-1);
  await engine.stop();
  worker.emit({ type: 'ready' });
  worker.emit({ type: 'stdout', mode: 1, data: new Uint8Array([1]) });
  assert.equal((await job).type, 'stopped');
  assert.equal(worker.messages.length, 0);
  assert.equal(output, 0);
  assert.equal(worker.count(), 0);
  await engine.dispose();
});

test('clear during compile rejects all late object records and retires worker', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  const job = engine.compile();
  const worker = workers.at(-1);
  worker.emit({ type: 'ready' });
  const generation = engine.cacheStats.generation;
  engine.clearCaches();
  worker.emit({ type: 'cpp_cache_object', generation, ...source('b') });
  complete(worker);
  await job;
  assert.equal(engine.cacheStats.entries, 0);
  assert.equal(worker.terminated, true);
  await engine.dispose();
});

test('successful repeated jobs reuse worker with stable listeners and fresh stdin', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  const first = engine.run();
  const worker = workers.at(-1);
  worker.emit({ type: 'ready' });
  const firstBuffer = worker.messages.at(-1).stdin_buffer;
  const listeners = worker.count();
  record(worker, engine);
  complete(worker);
  await first;
  assert.equal(engine.cacheStats.entries, 1);
  const second = engine.run();
  assert.equal(workers.at(-1), worker);
  assert.equal(worker.count(), listeners);
  assert.notEqual(worker.messages.at(-1).stdin_buffer, firstBuffer);
  assert.equal(worker.messages.at(-1).cpp_cache.entries.length, 1);
  complete(worker);
  await second;
  assert.equal(engine.cacheStats.latest.workerReused, true);
  await engine.dispose();
  assert.equal(worker.count(), 0);
  assert.equal(engine.cacheStats.entries, 0);
});

test('nonzero outcomes never populate; errors recover; Debug retires retained worker', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  let job = engine.run();
  let worker = workers.at(-1);
  worker.emit({ type: 'ready' });
  record(worker, engine);
  worker.emit({ type: 'stop', exit_code: 1, build_ms: 1, run_ms: 0 });
  await job;
  assert.equal(engine.cacheStats.entries, 0);
  assert.ok(worker.terminated);
  job = engine.run();
  worker = workers.at(-1);
  worker.emit({ type: 'error', message: 'failure' });
  assert.equal((await job).type, 'error');
  job = engine.run();
  worker = workers.at(-1);
  worker.emit({ type: 'ready' });
  complete(worker);
  await job;
  engine.debugger.enabled = true;
  job = engine.run();
  assert.ok(worker.terminated);
  worker = workers.at(-1);
  worker.emit({ type: 'ready' });
  assert.equal(worker.messages.at(-1).cpp_cache, null);
  complete(worker);
  await job;
  assert.ok(worker.terminated);
  await engine.dispose();
});

test('compile never opens devices; synchronous opener Stop prevents start', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  let opens = 0;
  engine.hostDevice = () => {
    opens++;
    void engine.stop();
  };
  let job = engine.compile();
  let worker = workers.at(-1);
  worker.emit({ type: 'ready' });
  assert.equal(worker.messages.at(-1).build_only, true);
  complete(worker);
  await job;
  assert.equal(opens, 0);
  job = engine.run();
  worker = workers.at(-1);
  assert.equal((await job).type, 'stopped');
  assert.equal(opens, 1);
  assert.equal(worker.messages.filter((m) => m.lang).length, 1);
  await engine.dispose();
});

test('scope fixed and isolated; disposed engines reject; idle timeout releases worker', async () => {
  const engine = await Engine.create('c', options({ idleTimeoutMs: 5 }));
  engine.debugger.enabled = false;
  const job = engine.run();
  const worker = workers.at(-1);
  worker.emit({ type: 'ready' });
  record(worker, engine);
  complete(worker);
  await job;
  await new Promise((resolve) => setTimeout(resolve, 15));
  assert.ok(worker.terminated);
  const other = await Engine.create(
    'c',
    options({ scope: { projectId: 'other', accountId: 'b' } })
  );
  assert.equal(other.cacheStats.entries, 0);
  assert.ok(Object.isFrozen(engine.scope));
  await engine.dispose();
  assert.equal((await engine.run()).type, 'error');
  await other.dispose();
});

test('Debug completion still emits DAP terminated before listener retirement', async () => {
  const engine = await Engine.create('c');
  let terminated = 0;
  engine.debugger.on('event', (event) => {
    if (event.event === 'terminated') terminated++;
  });
  const job = engine.run();
  const worker = workers.at(-1);
  worker.emit({ type: 'ready' });
  complete(worker);
  assert.equal((await job).type, 'completed');
  assert.equal(terminated, 1);
  assert.equal(worker.count(), 0);
  await engine.dispose();
});
