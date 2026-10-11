import EventEmitter from 'events';

import { prefetch_urls, WorkerOut, WorkerStart } from '../../pkg/engine';
import init from '../../pkg/engine';
import wasmBinary from '../../pkg/engine_bg.wasm';
import { Debugger } from './debugger';
import { HostDeviceOpener, HostDeviceSession } from './host-device';
import { errorResult, Internals } from './util';
import RustWorker from './worker?worker&inline';

// Concurrent Engine.create calls must share wasm-bindgen's initialization.
let initialization: ReturnType<typeof init> | undefined;

export type Lang = 'c' | 'python' | 'rust';
type StdoutMode = 1 | 2;

/** Optional inputs produced by the same C++ toolchain as this engine. */
export interface CppArtifacts {
  sources?: string[] | null;
  archives?: string[] | null;
  precompiledHeader?: string | null;
}

/** Wall-clock timing for a run, in milliseconds. */
export type Timing = {
  /** Host time including worker startup and teardown. */
  totalMs: number;
  /** Worker time fetching, compiling, and linking before execution. */
  buildMs: number;
  /** Worker time for the user program execution step only. */
  runMs: number;
};

/** The engine ran to completion with the provided `exitCode`. */
export type CompletedResult = { type: 'completed'; exitCode: number; timing: Timing };
/** The engine was stopped by calling `stop`. */
export type StoppedResult = { type: 'stopped' };
/** The engine had an error. This is an error with the engine itself, and not the user's code. */
export type ErrorResult = { type: 'error'; error: { type: string; message: string } };
/** The result of calling {@link Engine.run} */
export type RunResult = CompletedResult | StoppedResult | ErrorResult;

export type FsNode = string | DirNode;
export type DirNode = { [name: string]: FsNode };

export class Engine {
  public readonly stdout = new Stdout(1);
  public readonly stderr = new Stdout(2);
  public readonly stdin = new Stdin();
  public readonly debugger: Debugger;

  /** A function which, when called, rejects the ongoing execution */
  private rejector?: () => void;

  /** The ongoing `run` promise, if any */
  private promise?: Promise<RunResult>;
  private runActive = false;

  /**
   * The programming language of this engine.
   */
  public readonly lang: Lang;

  /**
   * The *initial* filesystem that the code sees.
   *
   * This is neither updated while the code is running, nor
   * will updating it have any effect on code that is already running.
   */
  public fs: DirNode = {};

  /** Optional binary build inputs, addressed by absolute virtual paths. */
  public binaryFiles: Record<string, Uint8Array> = {};
  public cppArtifacts?: CppArtifacts;

  /** Optional C/C++ and Python byte device. The opener is captured once at the start of each run. */
  public hostDevice?: HostDeviceOpener;

  static async create(lang: Lang): Promise<Engine> {
    initialization ??= init({ module_or_path: wasmBinary }).catch((error: unknown) => {
      initialization = undefined;
      throw error;
    });
    await initialization;
    if (typeof window !== 'undefined' && typeof fetch !== 'undefined')
      // Drain prefetches so an unread response cannot hold the browser cache entry open.
      for (const url of prefetch_urls(lang))
        void fetch(url, { cache: 'force-cache' })
          .then((response) => response.arrayBuffer())
          .catch(() => {});
    return new Engine(lang);
  }

  private constructor(lang: Lang) {
    this.debugger = new Debugger();
    this.lang = lang;
  }

  /**
   * Stops the currently running execution.
   */
  public stop(): void {
    this.rejector?.();
  }

  /**
   * Runs the program. {@link CompletedResult} carries build/run timing: `timing.runMs`
   * is the isolated user-program execution step, `timing.buildMs` covers toolchain fetch,
   * compile, and link, and `timing.totalMs` adds host-side worker startup and teardown.
   */
  public async run(): Promise<RunResult> {
    if (this.promise) return this.promise;
    if (this.runActive) throw new Error('Run setup is already active');
    this.runActive = true;
    this.promise = this.execute();
    try {
      return await this.promise;
    } finally {
      this.promise = undefined;
      this.runActive = false;
    }
  }

  private async execute(): Promise<RunResult> {
    const totalStart = performance.now();
    const opener = this.hostDevice;
    const binaryFiles = structuredClone(this.binaryFiles);
    const cppArtifacts = structuredClone(this.cppArtifacts);
    let worker: Worker | undefined;
    let device: HostDeviceSession | undefined;
    let removeListeners: (() => void) | undefined;
    let cleanupError: Error | undefined;
    let result: RunResult;
    try {
      if (opener !== undefined) {
        if (this.lang !== 'c' && this.lang !== 'python')
          throw new TypeError('Host devices are supported only for C/C++ and Python execution');
        if (typeof opener !== 'function') throw new TypeError('hostDevice must be a function');
      }
      result = await new Promise<RunResult>((resolve, reject) => {
        this.rejector = () => reject('stopped');
        worker = new RustWorker();
        const currentWorker = worker;
        this.stdout[Internals].attach(worker);
        this.stderr[Internals].attach(worker);
        this.debugger[Internals].attach(worker);
        let ready = false;
        const onError = (event: ErrorEvent) =>
          reject(event.error ?? new Error(event.message || 'Execution worker failed'));
        const onMessageError = () =>
          reject(new Error('Execution worker emitted an unreadable message'));
        const onMessage = (event: MessageEvent<WorkerOut>) => {
          const message = event.data;
          try {
            if (message.type === 'ready' && !ready) {
              ready = true;
              const start: WorkerStart = {
                fs: this.fs,
                binary_files: binaryFiles,
                cpp_artifacts: cppArtifacts ?? null,
                lang: this.lang,
                stdin_buffer: this.stdin[Internals].buffer,
                is_debug: this.debugger.enabled,
                ...(device ? { host_device: device.workerStart } : {})
              };
              currentWorker.postMessage(start);
            } else if (message.type === 'stop') {
              resolve({
                type: 'completed',
                exitCode: message.exit_code,
                timing: {
                  totalMs: performance.now() - totalStart,
                  buildMs: message.build_ms,
                  runMs: message.run_ms
                }
              });
            } else if (message.type === 'error') {
              resolve({ type: 'error', error: { type: 'EngineError', message: message.message } });
            }
          } catch (error) {
            reject(error);
          }
        };
        worker.addEventListener('message', onMessage);
        worker.addEventListener('error', onError);
        worker.addEventListener('messageerror', onMessageError);
        removeListeners = () => {
          currentWorker.removeEventListener('message', onMessage);
          currentWorker.removeEventListener('error', onError);
          currentWorker.removeEventListener('messageerror', onMessageError);
        };
        if (opener !== undefined) {
          device = new HostDeviceSession(reject);
          device.attach(worker);
          device.open(opener);
        }
      });
    } catch (err: unknown) {
      result = err === 'stopped' ? { type: 'stopped' } : errorResult(err);
    } finally {
      this.rejector = undefined;
      removeListeners?.();
      cleanupError = device?.close();
      if (worker) {
        this.stdout[Internals].detach(worker);
        this.stderr[Internals].detach(worker);
        this.stdin[Internals].clear();
        worker.terminate();
      }
    }
    return cleanupError ? errorResult(cleanupError) : result;
  }
}

export class Stdout extends EventEmitter<{ data: [Uint8Array] }> {
  private readonly callback: (event: MessageEvent<WorkerOut>) => void;

  [Internals]: {
    attach(worker: Worker): void;
    detach(worker: Worker): void;
  };

  constructor(private readonly mode: StdoutMode) {
    super();
    this.callback = ((event: MessageEvent<WorkerOut>) => {
      const msg = event.data;
      if (msg.type !== 'stdout') return;
      if (msg.mode !== this.mode) return;
      const chunk = msg.data as Uint8Array<ArrayBuffer>;
      this.emit('data', chunk);
    }).bind(this);

    this[Internals] = {
      attach: this.attach.bind(this),
      detach: this.detach.bind(this)
    };
  }

  private attach(worker: Worker) {
    worker.addEventListener('message', this.callback);
  }

  private detach(worker: Worker) {
    worker.removeEventListener('message', this.callback);
  }
}

export class Stdin {
  /**
   * Ring buffer to store stdin data.
   *
   * - TypeScript controls write_index, Rust controls read_index
   * - One slot is always kept empty to distinguish full from empty
   */

  private static readonly BUFFER_SIZE = 2048;
  private static readonly HEADER_SIZE = 8; // 2 x i32
  private static readonly DATA_SIZE = Stdin.BUFFER_SIZE - Stdin.HEADER_SIZE;
  private static readonly READ_IDX = 0;
  private static readonly WRITE_IDX = 1;
  private static encoder = new TextEncoder();

  private readonly buffer = new SharedArrayBuffer(Stdin.BUFFER_SIZE);
  private readonly indices: Int32Array;
  private readonly data: Int8Array;

  [Internals]: {
    clear(): void;
    buffer: SharedArrayBuffer;
  };

  constructor() {
    this.indices = new Int32Array(this.buffer, 0, 2);
    this.data = new Int8Array(this.buffer, Stdin.HEADER_SIZE);
    this[Internals] = {
      clear: this.clear.bind(this),
      buffer: this.buffer
    };
  }

  private clear() {
    this.indices.fill(0);
  }

  public async write(value: Uint8Array | string): Promise<void> {
    const chunk = typeof value === 'string' ? Stdin.encoder.encode(value) : value;
    return this.writeBytes(chunk);
  }

  private async writeBytes(chunk: Uint8Array): Promise<void> {
    const { DATA_SIZE, READ_IDX, WRITE_IDX } = Stdin;
    let offset = 0;

    while (offset < chunk.length) {
      const readIdx = Atomics.load(this.indices, READ_IDX);
      let writeIdx = Atomics.load(this.indices, WRITE_IDX);

      if (writeIdx === DATA_SIZE - 1 && readIdx > 0) writeIdx = 0;
      const available = readIdx <= writeIdx ? DATA_SIZE - writeIdx - 1 : readIdx - writeIdx - 1;

      if (available === 0) {
        await Atomics.waitAsync(this.indices, READ_IDX, readIdx).value;
        continue;
      }

      const toWrite = Math.min(chunk.length - offset, available);
      this.data.set(chunk.subarray(offset, offset + toWrite), writeIdx);

      // Write index & notify reader
      Atomics.store(this.indices, WRITE_IDX, (writeIdx + toWrite) % DATA_SIZE);
      Atomics.notify(this.indices, WRITE_IDX);
      offset += toWrite;
    }
  }
}

export * from './debugger';
export { HOST_DEVICE_PATH } from './host-device';
export type { HostDevice, HostDeviceOpener } from './host-device';
