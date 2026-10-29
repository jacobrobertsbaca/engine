import assert from 'node:assert/strict';
import test from 'node:test';

import {
  CompilerModuleCache,
  type CompilerModuleMessage,
  type CompilerModuleStats,
  type RetainedCompilerModule
} from '../../src/ts/compiler-module-cache.ts';

// A real Wasm module exporting a fresh mutable global and a function reading it.
function binary(initial = 0): Uint8Array {
  return new Uint8Array([
    0,
    97,
    115,
    109,
    1,
    0,
    0,
    0,
    1,
    5,
    1,
    96,
    0,
    1,
    127,
    3,
    2,
    1,
    0,
    6,
    6,
    1,
    127,
    1,
    65,
    initial,
    11,
    7,
    12,
    2,
    1,
    103,
    3,
    0,
    4,
    114,
    101,
    97,
    100,
    0,
    0,
    10,
    6,
    1,
    4,
    0,
    35,
    0,
    11
  ]);
}

function harness() {
  const messages: CompilerModuleMessage[] = [];
  const cache = new CompilerModuleCache((message) => messages.push(message));
  const stats = (): CompilerModuleStats => {
    const latest = messages.filter((message) => message.type === 'compiler_module_stats').at(-1);
    assert.ok(latest);
    return latest;
  };
  const retained = (): RetainedCompilerModule => {
    const latest = messages.filter((message) => message.type === 'compiler_module').at(-1);
    assert.ok(latest);
    return latest.value;
  };
  return { cache, stats, retained };
}

const enabled = { type: 'configure_cpp', cacheCompilerModule: true } as const;

test('default off compiles distinct Modules and retains no identity bytes', async () => {
  const { cache, stats } = harness();
  assert.notStrictEqual(await cache.construct(binary()), await cache.construct(binary()));
  cache.instantiated(2);
  assert.equal(stats().hits, 0);
  assert.equal(stats().misses, 0);
  assert.equal(stats().identityBytes, 0);
  assert.equal(stats().instantiateMs, 2);
});

test('a hit returns the compiled Module and each instance still gets fresh mutable globals', async () => {
  const { cache, stats } = harness();
  cache.configure(enabled);
  const module = await cache.construct(binary());
  assert.strictEqual(await cache.construct(binary()), module);
  assert.equal(stats().hits, 1);
  assert.equal(stats().misses, 1);
  assert.equal(stats().identityBytes, binary().length);
  assert.match(stats().sha256 ?? '', /^[a-f0-9]{64}$/);
  const first = new WebAssembly.Instance(module);
  const firstGlobal = first.exports.g as WebAssembly.Global;
  firstGlobal.value = 42;
  const second = new WebAssembly.Instance(await cache.construct(binary()));
  assert.equal(firstGlobal.value, 42);
  assert.equal((second.exports.g as WebAssembly.Global).value, 0);
});

test('same-length compiler byte changes replace the single retained Module', async () => {
  const { cache, stats } = harness();
  cache.configure(enabled);
  const original = await cache.construct(binary());
  assert.notStrictEqual(await cache.construct(binary(1)), original);
  assert.equal(stats().hits, 0);
  assert.equal(stats().misses, 2);
  assert.equal(stats().identityBytes, binary().length);
  assert.notStrictEqual(await cache.construct(binary()), original);
  assert.equal(stats().misses, 3);
});

test('parent-worker structured clone retains immutable code with verified byte identity', async () => {
  const original = harness();
  original.cache.configure(enabled);
  await original.cache.construct(binary());
  const seed = structuredClone(original.retained());
  const replacement = harness();
  replacement.cache.configure({ ...enabled, compilerModule: seed });
  assert.strictEqual(await replacement.cache.construct(binary()), seed.module);
  assert.equal(replacement.stats().hits, 1);
  assert.equal(replacement.stats().misses, 0);
});

test('a claimed matching SHA does not bypass the full-byte identity check', async () => {
  const original = harness();
  original.cache.configure(enabled);
  await original.cache.construct(binary());
  const seed = { ...original.retained(), bytes: binary(1) };
  const replacement = harness();
  replacement.cache.configure({ ...enabled, compilerModule: seed });
  assert.notStrictEqual(await replacement.cache.construct(binary()), seed.module);
  assert.equal(replacement.stats().hits, 0);
  assert.equal(replacement.stats().misses, 1);
});

test('identity is taken from owned bytes before asynchronous hashing', async () => {
  const { cache, stats } = harness();
  cache.configure(enabled);
  const input = binary();
  const pending = cache.construct(input);
  input.fill(255);
  const module = await pending;
  assert.equal((new WebAssembly.Instance(module).exports.g as WebAssembly.Global).value, 0);
  assert.strictEqual(await cache.construct(binary()), module);
  assert.equal(stats().hits, 1);
});

test('disable and clear release retained identity; invalid Wasm never populates it', async () => {
  const { cache, stats } = harness();
  cache.configure(enabled);
  const original = await cache.construct(binary());
  cache.configure({ type: 'configure_cpp', cacheCompilerModule: false });
  cache.instantiated(0);
  assert.equal(stats().identityBytes, 0);
  cache.configure(enabled);
  await assert.rejects(cache.construct(new Uint8Array([0, 1, 2])), WebAssembly.CompileError);
  cache.instantiated(0);
  assert.equal(stats().identityBytes, 0);
  assert.notStrictEqual(await cache.construct(binary()), original);
});

test('unsupported Module cloning is an optional-cache bypass', async () => {
  const cache = new CompilerModuleCache(() => {
    throw new DOMException('Cannot clone', 'DataCloneError');
  });
  cache.configure(enabled);
  const module = await cache.construct(binary());
  assert.ok(module instanceof WebAssembly.Module);
  assert.strictEqual(await cache.construct(binary()), module);
});

test('clear during hashing cannot repopulate the retained Module', async () => {
  const { cache, stats } = harness();
  cache.configure(enabled);
  const pending = cache.construct(binary());
  cache.configure({ type: 'configure_cpp', cacheCompilerModule: false });
  await pending;
  cache.instantiated(0);
  assert.equal(stats().identityBytes, 0);
  assert.equal(stats().hits, 0);
  assert.equal(stats().misses, 0);
});
