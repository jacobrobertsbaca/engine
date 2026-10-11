import assert from 'node:assert/strict';
import { readFile, rm } from 'node:fs/promises';
import { createServer } from 'node:http';
import { resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { buildFixtures } from './build-fixtures.mjs';

// Test a built package through its public API. Use --exceptions when the package
// also contains the optional C++ exception-handling change.
assert.ok(process.env.PLAYWRIGHT_MODULE, 'Set PLAYWRIGHT_MODULE to an installed Playwright module');
const { chromium } = await import(pathToFileURL(resolve(process.env.PLAYWRIGHT_MODULE)).href);
const modulePath =
  process.argv.slice(2).find((arg) => !arg.startsWith('--')) ??
  fileURLToPath(new URL('../../dist/debugger-sh.js', import.meta.url));
const engineModule = await readFile(modulePath);
const fixtures = await buildFixtures(process.argv.includes('--exceptions'));
const html = `<script type="module">
import { Engine } from '/engine.js';
window.probe = {
  async create() {
    this.engine = await Engine.create('c');
    const decoder = new TextDecoder();
    this.engine.stdout.on('data', bytes => this.state.stdout += decoder.decode(bytes));
    this.engine.stderr.on('data', bytes => this.state.stderr += decoder.decode(bytes));
    this.engine.debugger.on('event', event => this.state.events.push(event));
    this.pch = new Uint8Array(await (await fetch('/fixture.pch')).arrayBuffer());
    this.archive = new Uint8Array(await (await fetch('/libfixture.a')).arrayBuffer());
  },
  begin({ header, source, plain = false, debug = false, snapshot = false, invalid }) {
    this.state = { stdout: '', stderr: '', done: false, events: [] };
    this.seq = 1;
    this.engine.debugger.enabled = debug;
    this.engine.fs = plain ? { 'main.cpp': source } : {
      'main.cpp': source,
      'fixture.h': header,
      'excluded.cpp': '#error source selection must exclude this file'
    };
    this.engine.binaryFiles = plain ? {} : {
      '/fixture.pch': this.pch.slice(),
      '/libfixture.a': this.archive.slice()
    };
    this.engine.cppArtifacts = plain ? undefined : {
      sources: ['/main.cpp'], archives: ['/libfixture.a'], precompiledHeader: '/fixture.pch'
    };
    if (invalid === 'source') this.engine.cppArtifacts.sources = ['/missing.cpp'];
    if (invalid === 'empty sources') this.engine.cppArtifacts.sources = [];
    if (invalid === 'missing archive') this.engine.cppArtifacts.archives = ['/missing.a'];
    if (invalid === 'missing PCH') this.engine.cppArtifacts.precompiledHeader = '/missing.pch';
    if (invalid === 'binary path') this.engine.binaryFiles['/bad/../file'] = new Uint8Array([1]);
    if (invalid === 'corrupt archive') this.engine.binaryFiles['/libfixture.a'] = new Uint8Array([1, 2, 3]);
    this.engine.run().then(result => {
      this.state.result = result;
      this.state.done = true;
    }, error => {
      this.state.error = String(error);
      this.state.done = true;
    });
    if (snapshot) {
      this.engine.binaryFiles['/fixture.pch'].fill(0);
      this.engine.binaryFiles['/libfixture.a'].fill(0);
      this.engine.cppArtifacts.sources[0] = '/missing.cpp';
      this.engine.cppArtifacts.archives[0] = '/missing.a';
      this.engine.cppArtifacts.precompiledHeader = '/missing.pch';
    }
    if (debug) this.send('initialize');
  },
  send(command, args = {}) {
    return this.engine.debugger.send({ type: 'request', seq: this.seq++, command, arguments: args });
  }
};
</script>`;
const server = createServer((request, response) => {
  response.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
  response.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
  const resources = {
    '/': ['text/html', html],
    '/engine.js': ['text/javascript', engineModule],
    '/fixture.pch': ['application/octet-stream', fixtures.pch],
    '/libfixture.a': ['application/octet-stream', fixtures.archive]
  };
  const resource = resources[request.url];
  if (!resource) return response.writeHead(404).end();
  response.setHeader('Content-Type', resource[0]);
  response.end(resource[1]);
});
let browser;
let page;
const pageErrors = [];
const results = [];
const source =
  'int main() {\n  int value = PCH_VALUE + archive_value();\n  printf("ARTIFACT RESULT: %d\\n", value);\n  return value == 42 ? 0 : 1;\n}\n';

async function begin(options = {}) {
  await page.evaluate((args) => window.probe.begin(args), {
    header: fixtures.header,
    source,
    ...options
  });
}
async function complete(label, success = true) {
  await page.waitForFunction(() => window.probe.state.done, undefined, { timeout: 120_000 });
  const state = await page.evaluate(() => window.probe.state);
  if (success) {
    assert.equal(state.result?.type, 'completed', JSON.stringify(state));
    assert.equal(state.result.exitCode, 0, JSON.stringify(state));
    assert.equal(state.stdout, 'ARTIFACT RESULT: 42\n', JSON.stringify(state));
  } else {
    assert.ok(
      state.result?.type === 'error' ||
        (state.result?.type === 'completed' && state.result.exitCode !== 0),
      JSON.stringify(state)
    );
  }
  results.push({ label, result: state.result, stdout: state.stdout, stderr: state.stderr });
  return state;
}

try {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  browser = await chromium.launch({ headless: true });
  page = await browser.newPage();
  page.on('pageerror', (error) => pageErrors.push(String(error)));
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  await page.waitForFunction(() => window.probe !== undefined);
  assert.equal(await page.evaluate(() => crossOriginIsolated), true);
  await page.evaluate(() => window.probe.create());

  await begin({ snapshot: true });
  await complete('Run: PCH, archive, selected source, and binary/options snapshot');

  await begin({ debug: true });
  await page.waitForFunction(
    () => {
      const probe = window.probe;
      if (!probe.state.events.some((event) => event.event === 'initialized')) return false;
      const response = probe.send('setBreakpoints', {
        source: { path: '/main.cpp' },
        breakpoints: [{ line: 3 }]
      });
      if (!response?.body?.breakpoints?.some((breakpoint) => breakpoint.verified)) return false;
      return probe.send('configurationDone')?.success === true;
    },
    undefined,
    { timeout: 120_000 }
  );
  await page.waitForFunction(
    () => window.probe.state.events.some((event) => event.event === 'stopped'),
    undefined,
    { timeout: 120_000 }
  );
  const debug = await page.evaluate(() => {
    const probe = window.probe;
    const stopped = probe.state.events.find((event) => event.event === 'stopped');
    const stack = probe.send('stackTrace', { threadId: stopped.body.threadId });
    const response = probe.send('next', { threadId: stopped.body.threadId });
    return { stopped, stack, response };
  });
  assert.equal(debug.stopped.body.reason, 'breakpoint');
  assert.equal(debug.stack.success, true, JSON.stringify(debug));
  assert.equal(debug.stack.body.stackFrames[0].name, 'main');
  assert.equal(debug.stack.body.stackFrames[0].line, 3);
  assert.equal(debug.response.success, true, JSON.stringify(debug));
  await page.waitForFunction(
    () => window.probe.state.events.filter((event) => event.event === 'stopped').length >= 2,
    undefined,
    { timeout: 120_000 }
  );
  const stepped = await page.evaluate(() => {
    const probe = window.probe;
    const stopped = probe.state.events.filter((event) => event.event === 'stopped')[1];
    const stack = probe.send('stackTrace', { threadId: stopped.body.threadId });
    const response = probe.send('continue', { threadId: stopped.body.threadId });
    return { stopped, stack, response };
  });
  assert.equal(stepped.stopped.body.reason, 'step');
  assert.equal(stepped.stack.success, true, JSON.stringify(stepped));
  assert.equal(stepped.stack.body.stackFrames[0].name, 'main');
  assert.equal(stepped.stack.body.stackFrames[0].line, 4);
  assert.equal(stepped.response.success, true, JSON.stringify(stepped));
  await complete('Debug: PCH, archive, source breakpoint, Step Over and continue');

  for (const [invalid, message] of [
    ['source', 'C++ sources must select existing source files'],
    ['empty sources', 'C++ sources must select existing source files'],
    ['missing archive', 'Missing binary build input: /missing.a'],
    ['missing PCH', 'Missing binary build input: /missing.pch'],
    ['binary path', 'Invalid binary input path: /bad/../file']
  ]) {
    await begin({ invalid });
    const state = await complete(`Reject ${invalid}`, false);
    assert.equal(state.result.type, 'error', JSON.stringify(state));
    assert.equal(state.result.error.type, 'EngineError');
    assert.ok(state.result.error.message.includes(message), JSON.stringify(state));
  }
  await begin({ invalid: 'corrupt archive' });
  const corrupt = await complete('Reject corrupt archive', false);
  assert.match(corrupt.stdout + corrupt.stderr, /libfixture\.a/);
  await begin({
    plain: true,
    source: '#include <cstdio>\nint main() { printf("ARTIFACT RESULT: %d\\n", 42); }\n'
  });
  await complete('Plain source remains compatible after artifact errors');
  assert.deepEqual(pageErrors, []);
  console.log(
    JSON.stringify(
      { status: 'passed', exceptions: process.argv.includes('--exceptions'), results },
      null,
      2
    )
  );
} catch (error) {
  console.error(
    JSON.stringify(
      {
        status: 'failed',
        results,
        state: await page?.evaluate(() => window.probe?.state).catch(() => undefined),
        pageErrors
      },
      null,
      2
    )
  );
  throw error;
} finally {
  await browser?.close();
  if (server.listening) await new Promise((resolve) => server.close(resolve));
  await rm(fixtures.directory, { recursive: true, force: true });
}
