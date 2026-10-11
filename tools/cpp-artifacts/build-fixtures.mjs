import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { WASI } from 'node:wasi';

// Pin the compiler and sysroot used by src/worker/mod.rs. PCH files must be
// generated with the same language and exception settings as the tested engine.
export async function buildFixtures(exceptions = false) {
  const directory = await mkdtemp(join(tmpdir(), 'debugger-sh-cpp-artifacts-'));
  try {
    const assets = [
      ['llvm.core.wasm', '24fbed474c7b5b4968fd73fc4827440b93fb351c1b6264516130300eff3e7bf5'],
      ['llvm-resources.tar.gz', '8745c6af23fd841fa507d0c73f6b6b8e8da1b4ec216a6339739a28b38ff7227b']
    ];
    for (const [name, digest] of assets) {
      const response = await fetch(`https://fabioibanez.github.io/website/${name}`, {
        signal: AbortSignal.timeout(120_000)
      });
      assert.ok(response.ok, `Cannot fetch ${name}: ${response.status}`);
      const bytes = Buffer.from(await response.arrayBuffer());
      assert.equal(createHash('sha256').update(bytes).digest('hex'), digest, name);
      await writeFile(join(directory, name), bytes);
    }
    const sysroot = join(directory, 'sysroot');
    await mkdir(sysroot);
    execFileSync('tar', ['-xzf', join(directory, 'llvm-resources.tar.gz'), '-C', sysroot]);
    const compiler = await WebAssembly.compile(await readFile(join(directory, 'llvm.core.wasm')));
    function newWasi(args) {
      return new WASI({
        version: 'preview1',
        args,
        env: {},
        preopens: { '/': sysroot },
        returnOnExit: true
      });
    }
    async function invoke(args) {
      const wasi = newWasi(args);
      const instance = await WebAssembly.instantiate(compiler, wasi.getImportObject());
      assert.equal(wasi.start(instance), 0, `Fixture compilation failed: ${args.join(' ')}`);
    }
    const flags = [
      '-cc1',
      '-triple',
      'wasm32-wasip1',
      '-Werror',
      '-disable-free',
      '-isysroot',
      '/',
      '-internal-isystem',
      '/include/c++/v1',
      '-internal-isystem',
      '/include',
      '-internal-isystem',
      '/include/wasm32-wasip1',
      '-std=c++23'
    ];
    if (exceptions) {
      flags.push(
        '-fexceptions',
        '-fcxx-exceptions',
        '-exception-model=wasm',
        '-target-feature',
        '+exception-handling',
        '-mllvm',
        '-wasm-enable-eh',
        '-mllvm',
        '-wasm-use-legacy-eh=true'
      );
    }
    const header = '#include <cstdio>\n#define PCH_VALUE 40\nint archive_value();\n';
    await writeFile(join(sysroot, 'fixture.h'), header);
    await writeFile(join(sysroot, 'archive.cpp'), 'int archive_value() { return 2; }\n');
    await invoke([
      'clang',
      ...flags,
      '-emit-pch',
      '-fno-pch-timestamp',
      '-fvalidate-ast-input-files-content',
      '-x',
      'c++-header',
      '-o',
      '/fixture.pch',
      '/fixture.h'
    ]);
    await invoke(['clang', ...flags, '-emit-obj', '-x', 'c++', '-o', '/archive.o', '/archive.cpp']);
    await invoke(['llvm-ar', 'rcs', '/libfixture.a', '/archive.o']);
    return {
      directory,
      header,
      pch: await readFile(join(sysroot, 'fixture.pch')),
      archive: await readFile(join(sysroot, 'libfixture.a'))
    };
  } catch (error) {
    await rm(directory, { recursive: true, force: true });
    throw error;
  }
}
