import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { diagnosticFixtures, expectedMarker, sixTranslationUnits, snapshotHash } from './fixtures.mjs';
import { buildFixtures } from '../cpp-artifacts/build-fixtures.mjs';

// Run only after npm run build. Like tools/stdin/run.mjs, this exercises a built
// release through public Engine APIs; it never starts a competing engine build.
assert.ok(process.env.PLAYWRIGHT_MODULE, 'Set PLAYWRIGHT_MODULE to an installed Playwright module');
const { chromium, webkit } = await import(pathToFileURL(resolve(process.env.PLAYWRIGHT_MODULE)).href);
const option = (name, fallback) => process.argv.find((arg) => arg.startsWith(`--${name}=`))?.slice(name.length + 3) ?? fallback;
const modulePath = resolve(option('module', fileURLToPath(new URL('../../dist/debugger-sh.js', import.meta.url))));
const evidenceRoot = resolve(option('output', join(dirname(dirname(dirname(modulePath))), 'cs106b-runtime-cache-evidence')));
const browserName = option('browser', 'chrome');
assert.ok(['chrome', 'chromium', 'webkit'].includes(browserName), 'Use chrome, chromium, or webkit; WebKit is not Safari');
const sampleCount = Number(option('samples', '3'));
assert.ok(Number.isInteger(sampleCount) && sampleCount >= 3, 'At least three counterbalanced samples are required');
const moduleBytes = await readFile(modulePath);
const digest = (bytes) => createHash('sha256').update(bytes).digest('hex');
const runDirectory = join(evidenceRoot, `${new Date().toISOString().replace(/[:.]/g, '-')}-${browserName}`);
await mkdir(runDirectory, { recursive: true });
const sourceRoot = resolve(fileURLToPath(new URL('../..', import.meta.url)));
const source = {
  commit: execFileSync('git', ['rev-parse', 'HEAD'], { cwd: sourceRoot, encoding: 'utf8' }).trim(),
  diffSha256: digest(execFileSync('git', ['diff', 'HEAD', '--', 'src', 'Cargo.toml', 'Cargo.lock'], { cwd: sourceRoot })),
  status: execFileSync('git', ['status', '--short'], { cwd: sourceRoot, encoding: 'utf8' }).trim(),
  modulePath,
  moduleSha256: digest(moduleBytes)
};
const sourceFiles = execFileSync('git', ['ls-files', '--cached', '--others', '--exclude-standard', 'src', 'Cargo.toml', 'Cargo.lock', 'package.json', 'package-lock.json', 'vite.config.ts'], { cwd: sourceRoot, encoding: 'utf8' }).trim().split('\n').filter(Boolean).sort();
const fileManifest = await Promise.all(sourceFiles.map(async (path) => ({ path, sha256: digest(await readFile(join(sourceRoot, path))) })));
source.inputManifestSha256 = digest(JSON.stringify(fileManifest));
await writeFile(join(runDirectory, 'source-inputs.json'), JSON.stringify(fileManifest, null, 2) + '\n');

const html = `<!doctype html><meta charset="utf-8"><title>Runtime cache validation</title>
<style>body{font:15px system-ui;margin:28px;color:#16324f;background:#f4f7fb}h1{font-size:24px}pre{white-space:pre-wrap;background:white;padding:18px;border:1px solid #cbd5e1;border-radius:8px}canvas{background:white;border:1px solid #cbd5e1}</style>
<h1>Runtime cache validation</h1><p id="label">Loading engine…</p><canvas width="360" height="140"></canvas><pre id="receipt"></pre>
<script type="module">
import { Engine } from '/engine.js';
const canvas = document.querySelector('canvas');
const context = canvas.getContext('2d');
const memory = () => performance.memory ? { jsHeapUsedBytes: performance.memory.usedJSHeapSize, jsHeapTotalBytes: performance.memory.totalJSHeapSize, jsHeapLimitBytes: performance.memory.jsHeapSizeLimit } : null;
window.probe = {
  async measureMemory() {
    if (typeof performance.measureUserAgentSpecificMemory !== 'function') return { status: 'unavailable' };
    let timeout;
    try {
      return await Promise.race([
        performance.measureUserAgentSpecificMemory().then(value => ({ status: 'measured', bytes: value.bytes, breakdown: value.breakdown })),
        new Promise(resolve => { timeout = setTimeout(() => resolve({ status: 'timeout', timeoutMs: 15000 }), 15000); })
      ]);
    } catch (error) { return { status: 'unavailable', error: String(error) }; }
    finally { clearTimeout(timeout); }
  },
  async create(cpp) {
    if (this.engine) await this.close();
    const start = performance.now();
    this.engine = await Engine.create('c', { cpp });
    this.creationMs = performance.now() - start;
    this.engine.debugger.enabled = false;
    this.state = {};
    const decoder = new TextDecoder();
    this.engine.stdout.on('data', bytes => {
      if (!this.state.active) { this.lateOutput = (this.lateOutput ?? 0) + bytes.length; return; }
      this.state.stdout += decoder.decode(bytes);
      if (this.state.marker && this.state.firstMarkerMs === null && this.state.stdout.includes(this.state.marker)) {
        this.state.firstMarkerMs = performance.now() - this.state.apiStarted;
        this.state.editToMarkerMs = performance.now() - this.state.started;
      }
    });
    this.engine.stderr.on('data', bytes => {
      if (!this.state.active) { this.lateOutput = (this.lateOutput ?? 0) + bytes.length; return; }
      this.state.stderr += decoder.decode(bytes);
    });
    this.engine.hostDevice = device => {
      const jobState = this.state;
      this.state.deviceOpens++;
      device.onData(bytes => {
        if (jobState !== this.state || !jobState.active) { this.lateGraphics = (this.lateGraphics ?? 0) + bytes.length; return; }
        this.state.deviceBytes += bytes.length;
        if (this.state.firstGraphicsBytesMs === null) this.state.firstGraphicsBytesMs = performance.now() - this.state.apiStarted;
        const command = decoder.decode(bytes).trim().split(' ');
        if (command[0] === 'RECT') {
          context.fillStyle = command[5]; context.fillRect(...command.slice(1, 5).map(Number));
          requestAnimationFrame(() => { if (jobState === this.state && this.state.drawFrameCallbackMs === null) this.state.drawFrameCallbackMs = performance.now() - this.state.apiStarted; });
        }
      });
      return () => { this.state.deviceCleanups++; };
    };
    return { creationMs: this.creationMs, memory: memory(), stats: this.engine.cacheStats };
  },
  begin({ label, files, marker = '', compileOnly = false, binaryFiles = {}, artifacts }) {
    this.state = { label, marker, active: true, done: false, stdout: '', stderr: '', started: performance.now(), firstMarkerMs: null, firstGraphicsBytesMs: null, drawFrameCallbackMs: null, deviceOpens: 0, deviceCleanups: 0, deviceBytes: 0, beforeMemory: memory() };
    context.clearRect(0, 0, canvas.width, canvas.height);
    this.engine.fs = structuredClone(files);
    this.engine.binaryFiles = Object.fromEntries(Object.entries(binaryFiles).map(([path, bytes]) => [path, new Uint8Array(bytes)]));
    this.engine.cppArtifacts = structuredClone(artifacts);
    document.querySelector('#label').textContent = label;
    this.state.preparationMs = performance.now() - this.state.started;
    this.state.apiStarted = performance.now();
    const job = compileOnly ? this.engine.compile() : this.engine.run();
    this.job = job;
    job.then(result => { this.state.result = result; }, error => { this.state.error = String(error); }).finally(() => {
      this.state.apiMs = performance.now() - this.state.apiStarted;
      this.state.totalRequestMs = performance.now() - this.state.started;
      this.state.stats = this.engine.cacheStats;
      this.state.afterMemory = memory();
      this.state.active = false;
      this.state.done = true;
      document.querySelector('#receipt').textContent = JSON.stringify(this.state, null, 2);
    });
  },
  async close() {
    if (!this.engine) return null;
    await this.engine.stop();
    await this.job;
    if (typeof this.engine.dispose === 'function') await this.engine.dispose();
    const released = { stats: this.engine.cacheStats, memory: memory(), lateOutputBytes: this.lateOutput ?? 0, lateGraphicsBytes: this.lateGraphics ?? 0 };
    this.engine = null;
    return released;
  },
  userAgent() { return { userAgent: navigator.userAgent, crossOriginIsolated, creationMs: this.creationMs, memory: memory() }; }
};
</script>`;
const server = createServer((request, response) => {
  response.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
  response.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
  if (request.url === '/engine.js') {
    response.setHeader('Content-Type', 'text/javascript'); response.end(moduleBytes);
  } else if (request.url === '/') {
    response.setHeader('Content-Type', 'text/html'); response.end(html);
  } else response.writeHead(404).end();
});
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
const baseUrl = `http://127.0.0.1:${server.address().port}/`;
const results = [];
const releases = [];
const pageErrors = [];
let browser;
let page;
let activeContext;
let browserIdentity;
let activeReceipt;
const armOptions = {
  off: { objectCache: false, retainWorker: false, cacheCompilerModule: false },
  reset: { objectCache: true, retainWorker: false, cacheCompilerModule: false },
  retained: { objectCache: true, retainWorker: true, cacheCompilerModule: true }
};
const variants = [ ['cold', 'base'], ['unchanged', 'base'], ['edit', 'edit'], ['header', 'header'] ];
const snapshots = Object.fromEntries(['base', 'edit', 'header'].map((variant) => [variant, sixTranslationUnits(variant)]));
assert.equal(Buffer.byteLength(snapshots.base['part1.cpp']), Buffer.byteLength(snapshots.edit['part1.cpp']), 'Student edit must have equal byte length');
assert.equal(Buffer.byteLength(snapshots.edit['common.h']), Buffer.byteLength(snapshots.header['common.h']), 'Header edit must have equal byte length');

const save = async (status, error) => {
  const median = (values) => { const sorted = values.filter(Number.isFinite).sort((a, b) => a - b); return sorted.length ? sorted[Math.floor(sorted.length / 2)] : null; };
  const comparisons = Object.fromEntries(['cold', 'unchanged', 'edit', 'header'].map((input) => [input, Object.fromEntries(['off', 'reset', 'retained'].map((arm) => {
    const rows = results.filter((row) => row.arm === arm && row.input === input);
    return [arm, { n: rows.length, apiMedianMs: median(rows.map((row) => row.apiMs)), workerBuildMedianMs: median(rows.map((row) => row.result?.timing?.buildMs)), firstMarkerMedianMs: median(rows.map((row) => row.firstMarkerMs)), firstGraphicsBytesMedianMs: median(rows.map((row) => row.firstGraphicsBytesMs)), jsHeapMedianBytes: median(rows.map((row) => row.afterMemory?.jsHeapUsedBytes)) }];
  }))]));
  const receipts = {
    schemaVersion: 1, status, ...(error ? { error: String(error) } : {}), source, browser: browserIdentity,
    timingBoundaries: {
      cold: 'Fresh Engine and browser context; no object, worker or retained compiler Module. Network/cache behavior is recorded, not normalized.',
      firstMarkerMs: 'Public Run call to complete expected stdout marker in host listener; not human click latency.',
      editToMarkerMs: 'Harness submission including owned snapshot installation to complete expected marker; excludes human editing and host admission.',
      firstGraphicsBytesMs: 'Public Run call to first bytes of the synthetic RECT byte-device fixture; not Stanford GUI or painted-frame latency.',
      drawFrameCallbackMs: 'Animation-frame callback following canvas draw; not proof of actual screen paint.',
      actualGuiPaintMs: null, warmup: 'Compile-only measured separately from ordinary foreground Run.',
      memory: 'Per-run performance.memory readings are JS heap only. Optional measureUserAgentSpecificMemory readings include the API attribution breakdown and remain distinct from total browser process RSS.'
    },
    historicalEvidence: 'Named marble-experiments and overnight-report inputs were unavailable locally; no exploratory numbers are acceptance guarantees.',
    inputHashes: Object.fromEntries(Object.entries(snapshots).map(([name, files]) => [name, snapshotHash(files)])),
    comparisons, results, releases, pageErrors, activeReceipt
  };
  await writeFile(join(runDirectory, 'receipts.json'), JSON.stringify(receipts, null, 2) + '\n');
  return receipts;
};

async function create(cpp, suffix) {
  if (activeContext) await activeContext.close();
  activeContext = await browser.newContext();
  page = await activeContext.newPage();
  page.on('pageerror', (error) => pageErrors.push({ suffix, error: String(error) }));
  const navigationStart = performance.now();
  await page.goto(baseUrl);
  await page.waitForFunction(() => window.probe);
  const pageReadyMs = performance.now() - navigationStart;
  assert.equal(await page.evaluate(() => crossOriginIsolated), true);
  const creation = await page.evaluate((options) => window.probe.create(options), {
    ...cpp, scope: { projectId: `cache-validation-${suffix}`, accountId: 'synthetic-test-account' }, idleTimeoutMs: 30_000
  });
  return { ...creation, pageReadyMs };
}

function count(stats, key) { const value = stats?.latest?.cache?.[key]; return typeof value === 'number' ? value : undefined; }

async function complete(label, files, options = {}) {
  activeReceipt = { label, inputSha256: snapshotHash(files), ...options };
  await page.evaluate((input) => window.probe.begin(input), { label, files, ...options });
  await page.waitForFunction(() => window.probe.state.done, undefined, { timeout: 180_000 });
  // Wait one display callback so the screenshot records completed fixture drawing.
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(resolve)));
  const state = await page.evaluate(() => ({ ...window.probe.state, started: undefined, apiStarted: undefined }));
  const screenshot = `${String(results.length + 1).padStart(3, '0')}-${label.replace(/[^a-zA-Z0-9-]/g, '-')}.png`;
  await page.screenshot({ path: join(runDirectory, screenshot), fullPage: true });
  const resourceTimings = await page.evaluate(() => performance.getEntriesByType('resource').map(({ name, duration, transferSize, encodedBodySize }) => ({ name, duration, transferSize, encodedBodySize })));
  const result = { ...activeReceipt, ...state, screenshot, resourceTimings };
  results.push(result);
  await save('running');
  assert.equal(state.error, undefined, JSON.stringify(state));
  activeReceipt = undefined;
  console.log(JSON.stringify({ label, result: state.result, apiMs: state.apiMs, stats: state.stats }));
  return result;
}

function successful(state, marker) {
  assert.equal(state.result?.type, 'completed', JSON.stringify(state));
  assert.equal(state.result.exitCode, 0, JSON.stringify(state));
  if (marker) {
    assert.equal(state.stdout.trim(), marker, JSON.stringify(state));
    assert.equal(state.deviceOpens, 1, JSON.stringify(state));
    assert.equal(state.deviceCleanups, 1, JSON.stringify(state));
    assert.ok(state.deviceBytes > 0, JSON.stringify(state));
    assert.notEqual(state.firstMarkerMs, null, JSON.stringify(state));
  }
}

async function close(label) {
  const released = { label, ...(await page.evaluate(() => window.probe.close())) };
  releases.push(released);
  await save('running');
  assert.equal(released.stats.entries, 0, 'Stop/disposal must clear object entries');
  assert.equal(released.stats.compilerModuleIdentityBytes, 0, 'Stop/disposal must release the Module identity');
  assert.equal(released.stats.workerRetained, false, 'Stop/disposal must release the worker');
  assert.equal(released.lateOutputBytes, 0, 'No stdout/stderr after settlement');
  assert.equal(released.lateGraphicsBytes, 0, 'No graphics after settlement');
}

try {
  const browserLaunchStart = performance.now();
  browser = browserName === 'webkit'
    ? await webkit.launch({ headless: true })
    : await chromium.launch({ headless: true, ...(browserName === 'chrome' ? { channel: 'chrome' } : {}) });
  browserIdentity = { requestedBrowser: browserName, version: browser.version(), launchMs: performance.now() - browserLaunchStart, ...(browserName === 'chrome' ? { product: 'Installed Google Chrome', expectedExecutable: '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' } : {}), actualSafari: false };

  if (!process.argv.includes('--matrix-only')) {
    await create(armOptions.retained, 'compile-only');
    const prepared = await complete('compile-only-six-tu', snapshots.base, { compileOnly: true });
    successful(prepared);
    assert.equal(prepared.stdout, '');
    assert.equal(prepared.deviceOpens, 0, 'Compile-only must not open a host device');
    assert.equal(prepared.deviceBytes, 0);
    assert.equal(prepared.stats.latest.cache.link_ms, 0, 'Compile-only must return before linking');
    assert.equal(prepared.result.timing.runMs, 0, 'Compile-only must return before guest execution');
    const afterCompile = await complete('ordinary-run-after-compile-only', snapshots.base, { marker: expectedMarker() });
    successful(afterCompile, expectedMarker());
    assert.equal(count(afterCompile.stats, 'hits'), 6, JSON.stringify(afterCompile.stats));
    await close('compile-only');

    for (const [name, files] of Object.entries(diagnosticFixtures).filter(([name]) => name !== 'nonce')) {
      const outputs = [];
      for (const arm of ['off', 'retained']) {
        await create(armOptions[arm], `${name}-${arm}`);
        for (let repeat = 0; repeat < 2; repeat++) {
          const state = await complete(`diagnostic-${name}-${arm}-${repeat + 1}`, files);
          if (name === 'message') successful(state);
          else assert.ok(state.result?.type === 'completed' && state.result.exitCode !== 0 || state.result?.type === 'error', JSON.stringify(state));
          assert.equal(count(state.stats, 'hits') ?? 0, 0, JSON.stringify(state.stats));
          assert.equal(state.stats.entries, 0, 'Diagnostic or failed compilations must not populate the object cache');
          outputs.push({ arm, repeat, stdout: state.stdout, stderr: state.stderr, result: state.result?.type, exitCode: state.result?.exitCode });
        }
        await close(`${name}-${arm}`);
      }
      assert.deepEqual(outputs.slice(0, 2).map(({ arm, ...state }) => state), outputs.slice(2).map(({ arm, ...state }) => state), `${name}: caching must preserve diagnostics and outcome`);
    }
    await create(armOptions.retained, 'test-nonce');
    for (const nonce of [101, 102]) {
      const state = await complete(`tests-nonce-${nonce}`, diagnosticFixtures.nonce(nonce));
      successful(state);
      assert.equal(count(state.stats, 'hits') ?? 0, 0, JSON.stringify(state.stats));
      assert.equal(state.stats.entries, 0, 'Diagnostic nonce headers must remain on the compiler path');
    }
    await close('test-nonce');

    // Fixture generation runs sequentially after browser jobs have drained. It
    // compiles only this tiny PCH/archive using the existing pinned toolchain.
    const fixtures = await buildFixtures(true);
    try {
      await writeFile(join(runDirectory, 'fixture.pch'), fixtures.pch);
      await writeFile(join(runDirectory, 'libfixture.a'), fixtures.archive);
      await writeFile(join(runDirectory, 'artifact-identities.json'), JSON.stringify({
        pchSha256: digest(fixtures.pch), archiveSha256: digest(fixtures.archive), headerSha256: digest(fixtures.header),
        compilerSha256: '24fbed474c7b5b4968fd73fc4827440b93fb351c1b6264516130300eff3e7bf5',
        sysrootSha256: '8745c6af23fd841fa507d0c73f6b6b8e8da1b4ec216a6339739a28b38ff7227b', exceptions: true
      }, null, 2) + '\n');
      await create(armOptions.retained, 'pch-on');
      const files = { 'main.cpp': 'int main() { printf("PCH RESULT: %d\\n", PCH_VALUE + archive_value()); return 0; }\n', 'fixture.h': fixtures.header };
      for (let repeat = 0; repeat < 2; repeat++) {
        const state = await complete(`pch-on-archive-${repeat + 1}`, files, {
          binaryFiles: { '/fixture.pch': [...fixtures.pch], '/libfixture.a': [...fixtures.archive] },
          artifacts: { sources: ['/main.cpp'], archives: ['/libfixture.a'], precompiledHeader: '/fixture.pch' }
        });
        successful(state);
        assert.equal(state.stdout, 'PCH RESULT: 42\n');
      }
      await close('pch-on');
    } finally {
      await rm(fixtures.directory, { recursive: true, force: true });
    }
  }
  if (!process.argv.includes('--correctness-only')) {
    for (let sample = 0; sample < sampleCount; sample++) {
      const arms = ['off', 'reset', 'retained'];
      const order = arms.slice(sample % arms.length).concat(arms.slice(0, sample % arms.length));
      for (const arm of order) {
        const creation = await create(armOptions[arm], `${sample + 1}-${arm}`);
        browserIdentity.userAgent = await page.evaluate(() => window.probe.userAgent());
        for (const [input, variant] of variants) {
          const state = await complete(`sample-${sample + 1}-${arm}-${input}`, snapshots[variant], { marker: expectedMarker(variant) });
          state.sample = sample + 1; state.arm = arm; state.input = input; state.creation = creation;
          successful(state, expectedMarker(variant));
          // Statistics count a hit only where the worker actually skipped clang.
          if (arm !== 'off' && input === 'unchanged') assert.equal(count(state.stats, 'hits'), 6, JSON.stringify(state.stats));
          if (arm !== 'off' && input === 'edit') {
            assert.equal(count(state.stats, 'hits'), 5, JSON.stringify(state.stats));
            assert.equal(count(state.stats, 'misses'), 1, JSON.stringify(state.stats));
          }
          if (arm !== 'off' && input === 'header') assert.equal(count(state.stats, 'hits'), 0, JSON.stringify(state.stats));
          assert.equal(state.stats.latest.workerReused, arm === 'retained' && input !== 'cold', JSON.stringify(state.stats));
          if (arm === 'retained' && input !== 'cold') {
            assert.ok(state.stats.latest.compilerModule.hits > 0, 'Retained Module use must be independently observed');
            assert.equal(state.stats.latest.compilerModule.misses, 0, JSON.stringify(state.stats));
          }
        }
        if (process.argv.includes('--measure-memory')) {
          releases.push({ label: `sample-${sample + 1}-${arm}-before-release`, userAgentSpecificMemory: await page.evaluate(() => window.probe.measureMemory()) });
        }
        await close(`sample-${sample + 1}-${arm}`);
        if (process.argv.includes('--measure-memory')) {
          releases.push({ label: `sample-${sample + 1}-${arm}-after-release`, userAgentSpecificMemory: await page.evaluate(() => window.probe.measureMemory()) });
        }
      }
    }
  }
  assert.deepEqual(pageErrors, []);
  for (const { path, sha256 } of fileManifest) {
    assert.equal(digest(await readFile(join(sourceRoot, path))), sha256, `Production input changed during validation: ${path}`);
  }
  assert.equal(digest(await readFile(modulePath)), source.moduleSha256, 'Built bundle changed during validation');
  await save('passed');
  console.log(JSON.stringify({ status: 'passed', evidence: runDirectory, samples: sampleCount, measuredRuns: results.filter(({ sample }) => sample).length }));
} catch (error) {
  const failureScreenshot = 'failure.png';
  const captured = await page?.screenshot({ path: join(runDirectory, failureScreenshot), fullPage: true }).then(() => true, () => false);
  activeReceipt = { ...activeReceipt, ...(captured ? { screenshot: failureScreenshot } : {}), browserState: await page?.evaluate(() => window.probe?.state).catch(() => undefined) };
  await save('failed', error);
  console.error(JSON.stringify({ status: 'failed', evidence: runDirectory, error: String(error) }));
  throw error;
} finally {
  await page?.evaluate(() => window.probe?.close()).catch(() => {});
  await browser?.close();
  await new Promise((resolve) => server.close(resolve));
}
