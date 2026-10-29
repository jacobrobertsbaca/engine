# Fork release

This repository is a fork of [debugger-sh/engine](https://github.com/debugger-sh/engine) packaged for
the WebIDE. Fork builds keep the upstream package name `debugger-sh` and add a
`-webide.<webide-version>.<n>` prerelease suffix, so the numeric part names the upstream base
(`0.3.15-webide.0.4.0.1` is upstream 0.3.15, first fork build for WebIDE 0.4.0). They are distributed
only as a release tarball from this fork; nothing here is published to the npm registry, where the
name belongs upstream.

## Build and pack

Requires the toolchain in [Contributing](../README.md#contributing--building-from-source).

```bash
npm ci                      # locked install
npm run build               # wasm-pack build --target web && vite build
npm pack --ignore-scripts   # -> debugger-sh-<version>.tgz
```

Use `npm run build`, **not** `npm run build:release`. `build:release` sets `WASM_RELEASE=1`, which
makes `dist/debugger-sh.js` load `engine_bg.wasm` from
`https://cdn.jsdelivr.net/npm/debugger-sh@<version>/dist/` — a URL that does not exist for a fork
version, and which would serve upstream bytes if it did. The normal build base64-embeds the engine
WASM into the bundle, so the packed artifact needs neither the network nor a local path for its own
WASM. Compiler and sysroot URLs fetched at run time are unchanged from upstream.

`--ignore-scripts` packs the `dist/` you just built without running any lifecycle script. Do not run
`npm publish` in this fork (not even `--dry-run`): publish runs `prepublishOnly`, which is
`build:release`. `npm pack` on its own runs `prepack`/`prepare` but not `prepublishOnly` under
npm 11; `--ignore-scripts` removes the dependence on that ordering.

## Release

Attach the tarball to a release tagged `debugger-sh-v<version>`, giving the download URL consumers
install from:

```
https://github.com/justinvassantachart/engine/releases/download/debugger-sh-v<version>/debugger-sh-<version>.tgz
```

Record with each release: Node/npm/Rust/wasm-pack versions, the source commit and tree, the
tarball's size and SHA-256/SHA-512, and the SHA-256 of both the standalone `dist/engine_bg.wasm` and
the WASM embedded in `dist/debugger-sh.js` (they must match `pkg/engine_bg.wasm`).

The stdin regression uses an existing Playwright installation and its Chromium browser:

```bash
PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs node tools/stdin/run.mjs
```

An optional first argument selects the public `dist/debugger-sh.js` from an unpacked tarball.
The test covers empty reads without input, sequential Python input, C++ input and stopping a
blocked read before rerunning. It starts an isolated loopback server and closes its browser.

## Upstream coverage

The cumulative source includes the five changes proposed upstream as of 2026-10-10:

- [#18](https://github.com/debugger-sh/engine/pull/18): stale DWARF file indices.
- [#19](https://github.com/debugger-sh/engine/pull/19): C/C++ and Python host byte device.
- [#20](https://github.com/debugger-sh/engine/pull/20): C++ source exceptions and debugger unwinding.
- [#21](https://github.com/debugger-sh/engine/pull/21): zero-length stdin reads.
- [#22](https://github.com/debugger-sh/engine/pull/22): precompiled C++ headers, archives, and source selection.

These PRs are based on upstream `cc250508fabb5b091075e073ceb2e14899fd8423`.
Their combined engine source, preserving both sides of the overlapping host-device
and compiler-input changes, matches the fork's `src/` tree at `3f01766`. Fork package versions,
repository/download URLs, and release instructions remain specific to this fork.
Acceptance, merge order, and any later upstream edits still require comparison
against the actual merged source before switching a consumer to upstream.

The `0.3.15-webide.0.8.0.1` candidate also includes
[#23](https://github.com/debugger-sh/engine/pull/23): bounded native container reads,
truthful unavailable values, and paged libc++ deque children. Its two source commits
apply unchanged to the same upstream base. Applying the earlier cumulative source
and runtime-asset changes to that upstream feature branch reproduces this fork's
`src/` and `assets/` trees exactly. Candidate preparation does not publish a release.

This synchronization does not replace the already-published
`0.3.15-webide.0.7.3.1` release or its immutable source tag (`cf7fcdd`). A future
fork artifact needs a new version and its own build, test, and integrity evidence.

## Notices

`assets/cpp-exceptions/runtime.tar.gz` ships verbatim and carries the upstream LLVM
libc++abi/libunwind license texts, `NOTICE.txt` and `provenance.json` under
`share/licenses/cpp-exceptions/`; it is also embedded in the engine WASM by `src/worker/mod.rs`.
A fork build must leave it byte-for-byte identical. `LICENSE` (MIT, upstream copyright) is packed by
npm regardless of `files` and must stay.
