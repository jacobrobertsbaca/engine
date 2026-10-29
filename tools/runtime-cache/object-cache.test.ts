import assert from 'node:assert/strict';
import { test } from 'node:test';

import { CppObjectCache, DEFAULT_CPP_CACHE_LIMITS } from '../../src/ts/cpp-object-cache.ts';

const record = (source: string, size = 4, key = 'b') => ({
  source,
  key: key.repeat(64),
  object_hash: 'a'.repeat(64),
  object: new Uint8Array(size)
});

test('owned copies, replacement, and latest record per source', () => {
  const cache = new CppObjectCache();
  const value = record('/a.cpp');
  cache.put(value);
  value.object[0] = 99;
  const snapshot = cache.snapshot();
  assert.equal(snapshot[0].object[0], 0);
  snapshot[0].object[0] = 88;
  assert.equal(cache.snapshot()[0].object[0], 0);
  cache.put(record('/a.cpp', 8, 'c'));
  assert.equal(cache.stats().entries, 1);
  assert.equal(cache.stats().objectBytes, 8);
});
test('LRU evicts least recently hit source, with exact object accounting', () => {
  const cache = new CppObjectCache({ maxEntries: 2 });
  cache.put(record('/a'));
  cache.put(record('/b'));
  cache.touch('/a');
  cache.put(record('/c'));
  assert.deepEqual(
    cache.snapshot().map((r) => r.source),
    ['/a', '/c']
  );
  assert.equal(cache.stats().evictions, 1);
  assert.equal(cache.stats().objectBytes, 8);
});
test('object budget, oversize and zero limits never grow storage', () => {
  const cache = new CppObjectCache({ maxObjectBytes: 7 });
  cache.put(record('/a'));
  cache.put(record('/b'));
  assert.equal(cache.stats().entries, 1);
  assert.equal(cache.stats().evictions, 1);
  cache.put(record('/b', 8));
  assert.equal(cache.stats().entries, 0);
  assert.equal(cache.stats().oversized, 1);
  const none = new CppObjectCache({ maxEntries: 0 });
  none.put(record('/a'));
  assert.equal(none.stats().entries, 0);
});
test('overall and per-entry metadata budgets include UTF-8 source and digest fields', () => {
  const reference = new CppObjectCache();
  reference.put(record('/é'));
  const bytes = reference.stats().metadataBytes;
  const limited = new CppObjectCache({ maxMetadataBytes: bytes });
  limited.put(record('/é'));
  limited.put(record('/aa'));
  assert.equal(limited.stats().entries, 1);
  assert.equal(limited.stats().metadataBytes, bytes);
  const per = new CppObjectCache({ maxEntryMetadataBytes: bytes - 1 });
  per.put(record('/é'));
  assert.equal(per.stats().entries, 0);
});
test('malformed digests are misses; invalid limits fail early; clear releases both budgets', () => {
  const cache = new CppObjectCache();
  cache.put({ ...record('/a'), key: 'bad' });
  assert.equal(cache.stats().entries, 0);
  cache.put(record('/a'));
  cache.clear();
  assert.equal(cache.stats().entries, 0);
  assert.equal(cache.stats().metadataBytes, 0);
  assert.throws(() => new CppObjectCache({ maxEntries: NaN }));
  assert.throws(() => new CppObjectCache({ maxObjectBytes: -1 }));
  assert.equal(DEFAULT_CPP_CACHE_LIMITS.maxObjectBytes, 128 * 1024 * 1024);
});
