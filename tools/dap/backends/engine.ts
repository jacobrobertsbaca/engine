import { $ } from 'bun';
import chalk from 'chalk';
import type { Artifact } from 'debugger-sh';
import { existsSync } from 'node:fs';
import { writeFile } from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';

import type { Backend, BackendOptions, Json } from '../run';

export async function createEngineBackend(opts: BackendOptions): Promise<Backend> {
  const { Engine } = await import('debugger-sh');
  const engine = await Engine.create(opts.lang);
  engine.debugger.enabled = true;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  engine.fs = opts.fsNode as unknown as any;

  const decoder = new TextDecoder();
  const onIo = (chunk: Uint8Array) => {
    process.stdout.write(chalk.gray(decoder.decode(chunk)));
  };
  engine.stdout.on('data', onIo);
  engine.stderr.on('data', onIo);

  const eventCbs: ((e: Json) => void)[] = [];
  const artifactTasks: Promise<void>[] = [];

  engine.debugger.on('event', (msg: unknown) => {
    for (const cb of eventCbs) cb(msg as Json);
  });
  engine.debugger.on('artifact', (artifact) => {
    const task = handleArtifactOutput(opts.testOutputDir, artifact).catch((err) => {
      console.log(`${chalk.cyan('info')} artifact output failed: ${String(err)}`);
    });
    artifactTasks.push(task);
    void task;
  });

  const runPromise = engine.run();
  // Test-only completion signal; production DAP does not carry the run result.
  void runPromise.then((result) => {
    for (const cb of eventCbs) cb({ type: 'event', event: 'runResult', body: result });
  });

  return {
    async send(req) {
      if (
        req &&
        typeof req === 'object' &&
        !Array.isArray(req) &&
        req.type === 'request' &&
        req.command === 'launch'
      ) {
        const request = req as { seq?: number; command?: string };
        return {
          type: 'response',
          seq: 0,
          request_seq: typeof request.seq === 'number' ? request.seq : 0,
          success: true,
          command: request.command ?? 'launch'
        };
      }
      return engine.debugger.send(req) as Json;
    },
    onEvent(cb) {
      eventCbs.push(cb);
    },
    async shutdown() {
      await Promise.race([runPromise, new Promise((resolve) => setTimeout(resolve, 1500))]).catch(
        () => {
          /* swallow; reported as test failure if needed */
        }
      );
      // Some scenarios intentionally finish while the guest is still paused.
      await engine.dispose();
      await Promise.all(artifactTasks);
    }
  };
}

function isObjectLike(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function toUint8Array(value: unknown): Uint8Array | null {
  if (value instanceof Uint8Array) return value;
  if (Array.isArray(value) && value.every((v) => typeof v === 'number'))
    return Uint8Array.from(value);
  return null;
}

async function writeDerivedArtifacts(
  testOutputDir: string,
  wasmPath: string,
  prefix: 'pre' | 'post'
) {
  const sh = (cmd: string) => $`sh -lc ${cmd}`.quiet().nothrow();

  if (prefix === 'pre')
    await sh(
      `which llvm-dwarfdump >/dev/null 2>&1 && llvm-dwarfdump "${wasmPath}" > "${path.join(testOutputDir, 'pre.dwarf')}"`
    );

  const watPath = path.join(testOutputDir, `${prefix}.wat`);
  await sh(`which wasm-tools >/dev/null 2>&1 && wasm-tools print "${wasmPath}" > "${watPath}"`);

  if (!existsSync(watPath))
    await sh(`which wasm2wat >/dev/null 2>&1 && wasm2wat "${wasmPath}" > "${watPath}"`);
}

async function handleArtifactOutput(testOutputDir: string, artifact: Artifact) {
  if (!isObjectLike(artifact)) return;
  if (typeof artifact.name !== 'string') return;
  if (artifact.name !== 'pre.wasm' && artifact.name !== 'post.wasm') return;
  const data = toUint8Array(artifact.data);
  if (!data) return;

  const wasmPath = path.join(testOutputDir, artifact.name);
  await writeFile(wasmPath, data);
  await writeDerivedArtifacts(
    testOutputDir,
    wasmPath,
    artifact.name === 'pre.wasm' ? 'pre' : 'post'
  );
}
