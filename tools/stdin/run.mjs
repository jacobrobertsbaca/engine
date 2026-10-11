import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

// Test a built or unpacked release through its public Engine API in Chromium.
assert.ok(process.env.PLAYWRIGHT_MODULE, 'Set PLAYWRIGHT_MODULE to an installed Playwright module');
const { chromium } = await import(pathToFileURL(resolve(process.env.PLAYWRIGHT_MODULE)).href);
const modulePath =
  process.argv[2] ?? fileURLToPath(new URL('../../dist/debugger-sh.js', import.meta.url));
const engineModule = await readFile(modulePath);
const html = `<script type="module">
import { Engine } from '/engine.js';
window.probe = {
  async create(language) {
    this.engine?.stop();
    this.engine = await Engine.create(language);
    this.engine.debugger.enabled = false;
    const decoder = new TextDecoder();
    this.engine.stdout.on('data', bytes => this.state.stdout += decoder.decode(bytes));
    this.engine.stderr.on('data', bytes => this.state.stderr += decoder.decode(bytes));
  },
  begin(files) {
    this.state = { stdout: '', stderr: '', done: false };
    this.engine.fs = files;
    this.engine.run().then(result => {
      this.state.result = result;
      this.state.done = true;
    }, error => {
      this.state.error = String(error);
      this.state.done = true;
    });
  }
};
</script>`;
const server = createServer((request, response) => {
  response.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
  response.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
  if (request.url === '/engine.js') {
    response.setHeader('Content-Type', 'text/javascript');
    response.end(engineModule);
  } else if (request.url === '/') {
    response.setHeader('Content-Type', 'text/html');
    response.end(html);
  } else {
    response.writeHead(404).end();
  }
});
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
let browser;
let page;
const pageErrors = [];
const results = [];

async function begin(language, source) {
  await page.evaluate(
    ({ language, source }) => {
      window.probe.begin({ [language === 'python' ? 'main.py' : 'main.cpp']: source });
    },
    { language, source }
  );
}
async function output(fragment) {
  await page.waitForFunction(
    (text) => {
      const state = window.probe.state;
      return (state.stdout + state.stderr).includes(text) || state.done;
    },
    fragment,
    { timeout: 60_000 }
  );
  const state = await page.evaluate(() => window.probe.state);
  assert.ok((state.stdout + state.stderr).includes(fragment), JSON.stringify(state));
}
async function completed(label) {
  await page.waitForFunction(() => window.probe.state.done, undefined, { timeout: 15_000 });
  const state = await page.evaluate(() => window.probe.state);
  assert.equal(state.result?.type, 'completed', JSON.stringify(state));
  assert.equal(state.result.exitCode, 0, JSON.stringify(state));
  results.push({
    label,
    stdout: state.stdout,
    stderr: state.stderr,
    exitCode: state.result.exitCode
  });
  return state;
}
const write = (text) => page.evaluate((value) => window.probe.engine.stdin.write(value), text);

try {
  browser = await chromium.launch({ headless: true });
  page = await browser.newPage();
  page.on('pageerror', (error) => pageErrors.push(String(error)));
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  await page.waitForFunction(() => window.probe !== undefined);
  assert.equal(await page.evaluate(() => crossOriginIsolated), true);
  await page.evaluate(() => window.probe.create('python'));
  await begin(
    'python',
    'import os\nprint("BEFORE", flush=True)\nprint("EMPTY", repr(os.read(0, 0)), flush=True)'
  );
  await output('BEFORE');
  assert.match((await completed('empty read without input')).stdout, /EMPTY b''/);

  const python =
    'import sys\nname = input("Name? ")\nnumber = int(input("Number? "))\nprint(f"RESULT: {name} | {number * 2}", flush=True)\nprint("STDERR: complete", file=sys.stderr, flush=True)';
  for (const [name, number, doubled] of [
    ['Ada Lovelace', '21', '42'],
    ['Grace Hopper', '7', '14']
  ]) {
    await begin('python', python);
    await output('Name?');
    await write(`${name}\n`);
    await output('Number?');
    await write(`${number}\n`);
    const state = await completed(`Python sequential input: ${name}`);
    assert.ok(state.stdout.includes(`RESULT: ${name} | ${doubled}`));
    assert.ok(state.stderr.includes('STDERR: complete'));
  }

  await begin('python', 'print("WAITING", flush=True)\ninput()');
  await output('WAITING');
  await page.evaluate(() => window.probe.engine.stop());
  await page.waitForFunction(() => window.probe.state.done, undefined, { timeout: 15_000 });
  assert.equal((await page.evaluate(() => window.probe.state)).result.type, 'stopped');
  await begin('python', 'print("RERUN", flush=True)');
  await output('RERUN');
  assert.match((await completed('stop blocked input and rerun')).stdout, /RERUN/);

  await page.evaluate(() => window.probe.create('c'));
  await begin(
    'c',
    '#include <iostream>\n#include <string>\nint main() { std::string name; int number; std::cout << "Name? " << std::flush; std::getline(std::cin, name); std::cout << "Number? " << std::flush; std::cin >> number; std::cout << "RESULT: " << name << " | " << number * 2 << std::endl; }'
  );
  await output('Name?');
  await write('Ada Lovelace\n');
  await output('Number?');
  await write('21\n');
  assert.match((await completed('C++ input control')).stdout, /RESULT: Ada Lovelace \| 42/);
  assert.deepEqual(pageErrors, []);
  console.log(JSON.stringify({ status: 'passed', results }, null, 2));
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
  await new Promise((resolve) => server.close(resolve));
}
