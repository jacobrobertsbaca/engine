// After npm run build: npx -y bun tools/host-device/run.ts
import assert from 'node:assert/strict';

import { Engine, HOST_DEVICE_PATH, type HostDevice } from '../../dist/debugger-sh.js';

const size = 2 * 64 * 1024 + 37;
const expected = Uint8Array.from({ length: size }, (_, i) => i % 251);
const programs = {
  c: {
    file: 'main.cpp',
    exchange: `#include <fcntl.h>
#include <unistd.h>
int main() {
  int fd = open("${HOST_DEVICE_PATH}", O_RDWR);
  if (fd < 0) return 1;
  unsigned char bytes[${size}];
  if (read(fd, bytes, 0) != 0 || write(fd, bytes, 0) != 0) return 5;
  for (int offset = 0; offset < ${size};) {
    int count = ${size} - offset;
    if (count > 997) count = 997;
    int n = read(fd, bytes + offset, count);
    if (n <= 0) return 2;
    offset += n;
  }
  for (int i = 0; i < ${size}; ++i)
    if (bytes[i] != i % 251) return 3;
  for (int offset = 0; offset < ${size};) {
    int n = write(fd, bytes + offset, ${size} - offset);
    if (n <= 0) return 4;
    offset += n;
  }
  return 0; // No final acknowledgement: output must arrive before cleanup.
}`,
    blocked: `#include <fcntl.h>
#include <unistd.h>
int main() {
  int fd = open("${HOST_DEVICE_PATH}", O_RDWR);
  char byte = 'r';
  if (fd < 0 || write(fd, &byte, 1) != 1) return 1;
  return read(fd, &byte, 1) < 0;
}`,
    empty: 'int main() { return 0; }'
  },
  python: {
    file: 'main.py',
    exchange: `import os
fd = os.open("${HOST_DEVICE_PATH}", os.O_RDWR)
assert os.read(fd, 0) == b''
assert os.write(fd, b'') == 0
data = bytearray()
while len(data) < ${size}:
    chunk = os.read(fd, min(997, ${size} - len(data)))
    assert chunk
    data.extend(chunk)
assert data == bytes(i % 251 for i in range(${size}))
offset = 0
while offset < len(data):
    count = os.write(fd, data[offset:])
    assert count > 0
    offset += count
# No final acknowledgement: output must arrive before cleanup.
`,
    blocked: `import os
fd = os.open("${HOST_DEVICE_PATH}", os.O_RDWR)
assert os.write(fd, b'r') == 1
os.read(fd, 1)
`,
    empty: 'pass'
  }
};

async function testLanguage(lang: keyof typeof programs) {
  const program = programs[lang];
  const engine = await Engine.create(lang);
  let handle!: HostDevice;
  let cleanups = 0;
  const unhandled: unknown[] = [];
  const onUnhandled = (reason: unknown) => {
    unhandled.push(reason);
  };
  process.on('unhandledRejection', onUnhandled);
  const timer = setTimeout(() => {
    engine.stop();
    throw new Error('Host device test timed out');
  }, 90_000);

  async function exchange(debug: boolean) {
    engine.debugger.enabled = debug;
    engine.fs = { [program.file]: program.exchange };
    const chunks: Uint8Array[] = [];
    let writing!: Promise<void>,
      overlap!: Promise<void>,
      bytesAtCleanup = 0;
    engine.hostDevice = (device) => {
      handle = device;
      device.onData((bytes) => {
        chunks.push(bytes);
      });
      const input = new Uint8Array(expected);
      writing = device.write(input);
      input.fill(0); // write() must snapshot before its first wait for space.
      overlap = assert.rejects(device.write(new Uint8Array([1])), /previous host device write/);
      return () => {
        cleanups++;
        bytesAtCleanup = chunks.reduce((total, chunk) => total + chunk.length, 0);
      };
    };
    const result = await engine.run();
    await Promise.all([writing, overlap]);
    assert.equal(result.type, 'completed', JSON.stringify(result));
    if (result.type === 'completed') assert.equal(result.exitCode, 0);
    assert.equal(bytesAtCleanup, size);
    assert.deepEqual(Buffer.concat(chunks), Buffer.from(expected));
    assert.equal(handle.signal.aborted, true);
    await assert.rejects(handle.write(new Uint8Array([1])), /closed/);
  }

  try {
    await exchange(false);
    assert.equal(cleanups, 1);

    // Stop an actual blocked guest read, then reuse the same Engine in Debug.
    engine.fs = { [program.file]: program.blocked };
    let ready!: () => void;
    const waiting = new Promise<void>((resolve) => {
      ready = resolve;
    });
    engine.hostDevice = (device) => {
      handle = device;
      device.onData(() => ready());
      return () => {
        cleanups++;
      };
    };
    const running = engine.run();
    await waiting;
    engine.stop();
    assert.equal((await running).type, 'stopped');
    assert.equal(cleanups, 2);
    await assert.rejects(handle.write(new Uint8Array([1])), /closed/);

    let sequence = 1;
    engine.debugger.on('event', (event: { event?: string }) => {
      if (event.event === 'initialized')
        queueMicrotask(() => {
          engine.debugger.send({ seq: sequence++, type: 'request', command: 'configurationDone' });
        });
    });
    engine.debugger.send({
      seq: sequence++,
      type: 'request',
      command: 'initialize',
      arguments: { adapterID: 'host-device' }
    });
    await exchange(true);
    assert.equal(cleanups, 3);
    engine.debugger.enabled = false;
    engine.fs = { [program.file]: program.empty };
    engine.hostDevice = () => async () => {
      cleanups++;
      throw new Error('Async cleanup is not supported');
    };
    const failed = await engine.run();
    assert.equal(failed.type, 'error');
    if (failed.type === 'error') {
      assert.equal(failed.error.type, 'TypeError');
      assert.match(failed.error.message, /cleanup must be synchronous/);
    }
    await new Promise<void>((resolve) => setImmediate(resolve));
    assert.equal(cleanups, 4);
    assert.deepEqual(unhandled, []);
    if (lang === 'python') {
      engine.hostDevice = undefined;
      engine.fs = {
        'main.py': `import os
assert not os.path.exists("${HOST_DEVICE_PATH}")
`
      };
      const absent = await engine.run();
      assert.equal(absent.type, 'completed', JSON.stringify(absent));
      if (absent.type === 'completed') assert.equal(absent.exitCode, 0);
      assert.equal(cleanups, 4);
    }
    console.log(
      `PASS ${lang}: Run/Debug byte exchange, partial transfers, backpressure, final delivery, stop/rerun and cleanup`
    );
  } finally {
    process.off('unhandledRejection', onUnhandled);
    clearTimeout(timer);
    engine.stop();
  }
}

await testLanguage('c');
await testLanguage('python');

const rust = await Engine.create('rust');
rust.hostDevice = () => assert.fail('Unsupported language must not open a host device');
const unsupported = await rust.run();
assert.equal(unsupported.type, 'error');
if (unsupported.type === 'error') {
  assert.equal(unsupported.error.type, 'TypeError');
  assert.match(unsupported.error.message, /supported only for C\/C\+\+ and Python/);
}
console.log('PASS: Python device is opt-in; Rust remains unsupported');
