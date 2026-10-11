# debugger-sh

A browser-based execution engine powered by WebAssembly. Compile and run C/C++ and Python programs entirely in the browser with a built-in debugger.

## Installation

This is the WebIDE fork build `0.3.15-webide.0.7.3.1`, not an upstream npm release: `npm install debugger-sh` resolves to the upstream package. Install the fork from the release tarball published by this repository (see [docs/fork-release.md](./docs/fork-release.md)):

```bash
npm install https://github.com/justinvassantachart/engine/releases/download/debugger-sh-v0.3.15-webide.0.7.3.1/debugger-sh-0.3.15-webide.0.7.3.1.tgz
```

> **Requires** these response headers (the engine uses `SharedArrayBuffer` for stdin):
>
> ```
> Cross-Origin-Embedder-Policy: require-corp
> Cross-Origin-Opener-Policy: same-origin
> ```

---

## Quick start

```ts
import { Engine } from 'debugger-sh';

const engine = await Engine.create('c');

// Virtual filesystem — the program sees these as real files
engine.fs = {
  'main.cpp': `
    #include <iostream>
    int main() {
      std::cout << "Hello, world!" << std::endl;
      return 0;
    }
  `
};

// stdout / stderr chunks arrive as Uint8Array (UTF-8)
const decoder = new TextDecoder();
const print = (chunk: Uint8Array) => process.stdout.write(decoder.decode(chunk));
engine.stdout.on('data', print);
engine.stderr.on('data', print);

await engine.run();
```

> Programs compile in **debug mode** by default, so a [DAP](https://microsoft.github.io/debug-adapter-protocol/) handshake is required before `run()` will proceed — see the [integration guide](./docs/integration.md#debugger-dap). To skip it, set `engine.debugger.enabled = false`.

C++ source `throw`/`catch` and RAII work in Run and Debug. Prebuilt libc++ error
helpers can still abort instead of throwing. The worker embeds a small LLVM
exception supplement; compiler/sysroot URLs stay unchanged. Exact upstream
notices and payload provenance are in `share/licenses/cpp-exceptions/` inside
[runtime.tar.gz](./assets/cpp-exceptions/runtime.tar.gz).

---

## Full API

```ts
const engine = await Engine.create('c'); // 'c' | 'python'

engine.fs; // DirNode  — virtual filesystem, set before run()
engine.binaryFiles; // Record<string, Uint8Array> — optional C/C++ binary files
engine.cppArtifacts; // Optional selected sources, archives, and precompiled header
engine.stdout; // Stdout   — .on('data', (chunk: Uint8Array) => …) / .off(...)
engine.stderr; // Stdout
engine.stdin; // Stdin    — .write(string | Uint8Array): Promise<void>
engine.hostDevice; // Optional C/C++ and Python duplex byte device — see below
engine.debugger; // Debugger — DAP interface; set .enabled = false to skip the handshake
engine.lang; // Lang

engine.run(); // Promise<RunResult> — { type: 'completed', exitCode } | { type: 'stopped' } | { type: 'error', error }
engine.stop(); // void               — kills the worker; run() resolves with { type: 'stopped' }

engine.debugger.send(message); // DAP request, returns response synchronously
engine.debugger.on('event', fn); // async DAP events
engine.debugger.on('artifact', fn); // download artifacts emitted by the engine

// Python only — hide bridge/bdb frames from stackTrace (set before run())
engine.debugger.filterInternals = true;
```

See [Presentation filtering (Python)](./docs/integration.md#presentation-filtering-python) in the integration guide for stack `presentationHint`, frame naming (`__main__` vs `main`), and dunder local filtering.

---

## Precompiled C++ inputs

Set `binaryFiles` to bytes keyed by absolute virtual paths. Set `cppArtifacts`
to select source files, link archives, or use a precompiled header:

```ts
engine.binaryFiles = { '/support.a': archiveBytes, '/headers.pch': headerBytes };
engine.cppArtifacts = {
  sources: ['/main.cpp'],
  archives: ['/support.a'],
  precompiledHeader: '/headers.pch'
};
```

All fields are optional. Without `sources`, the engine compiles every source in
`engine.fs`. Selected sources must exist and cannot be empty; archive and PCH
paths must have matching bytes in `binaryFiles`. Binary paths must be absolute,
cannot contain empty, `.` or `..` components, and cannot replace existing files.
The engine snapshots the binary bytes and artifact options when a run starts.
Clear them before reusing an engine for an ordinary source-only run.

Build artifacts with the same compiler, sysroot, and flags as the engine. A PCH
also needs its original header files at their original virtual paths. These
inputs work in Run and Debug for C/C++ only; Python and Rust reject them.

After building, run the browser regression with an existing Playwright installation:

```bash
PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs node tools/cpp-artifacts/run.mjs --exceptions
```

---

## Wiring stdin

`engine.stdin.write()` accepts a UTF-8 string or `Uint8Array`. Programs read via `cin`, `scanf`, `read()`, etc.

```ts
await engine.stdin.write('42\n');
await engine.stdin.write(new TextEncoder().encode('42\n'));
```

---

For C/C++ or Python host I/O, set `engine.hostDevice` to a synchronous opener receiving
`{ signal, onData, write }`; return an optional synchronous cleanup function. The guest opens
`HOST_DEVICE_PATH` with `O_RDWR` for blocking reads/writes; polling is unsupported.
`write` snapshots bytes; await each write. Stop/exit aborts the per-run signal,
rejects pending writes and calls cleanup once. Terminal I/O stays separate.

Python uses the same device through `os.open("/dev/debugger-sh-host", os.O_RDWR)`,
`os.read(fd, size)`, and `os.write(fd, data)`. Reads and writes may be partial;
loop until a complete application-defined message has transferred. The device
is a byte stream: framing, request/response matching, and graphics or AI behavior
belong to the application. It is absent when no opener is supplied.

Test after building: `npx -y bun tools/host-device/run.ts`.

## Example project

For a full reference integration, see the [debugger.sh IDE](https://github.com/debugger-sh/debugger.sh) — a Next.js + MUI app wiring up CodeMirror 6, xterm.js, and `debugger-sh` into a working in-browser IDE.

---

## Contributing / building from source

> Requires [Cargo 1.91+](https://crates.io/), [wasm-pack](https://rustwasm.github.io/wasm-pack/), and Node v22+.

```bash
cargo install wasm-pack
npm install
npm run build   # wasm-pack build --target web && vite build
npm run dev     # local development
```

After building, run the stdin browser regression checks with an installed Playwright
module and Chromium browser:

```bash
PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs node tools/stdin/run.mjs
```

The checks cover an empty read without input, sequential Python input across reruns,
stopping blocked input, and C++ input. Pass a built or unpacked release's
`dist/debugger-sh.js` as the first argument to test that artifact.

To test against the reference IDE, clone [debugger-sh/debugger.sh](https://github.com/debugger-sh/debugger.sh) alongside this repo and link:

```bash
npm link                  # in this repo
cd ../debugger.sh
npm link debugger-sh
npm run dev
```
