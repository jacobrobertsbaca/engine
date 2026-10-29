import EventEmitter from 'events';

import { prefetch_urls, WorkerOut, WorkerStart } from '../../pkg/engine';
import init from '../../pkg/engine';
import wasmBinary from '../../pkg/engine_bg.wasm';
import type {
  CompilerModuleMessage,
  CompilerModuleStats,
  RetainedCompilerModule
} from './compiler-module-cache';
import { CppCacheLimits, CppObjectCache, ObjectCacheStats } from './cpp-object-cache';
import { Debugger } from './debugger';
import { createEngineWorker } from './engine-worker';
import { HostDeviceOpener, HostDeviceSession } from './host-device';
import { errorResult, Internals } from './util';

let initialization: ReturnType<typeof init> | undefined;
export type Lang = 'c' | 'python' | 'rust';
type StdoutMode = 1 | 2;

export interface CppArtifacts {
  sources?: string[] | null;
  archives?: string[] | null;
  precompiledHeader?: string | null;
}

/** Immutable ownership boundary. Create a new Engine when either identity changes. */
export interface EngineScope {
  projectId: string;
  accountId: string;
}
export interface CppPerformanceOptions {
  scope: EngineScope;
  objectCache?: boolean | Partial<CppCacheLimits>;
  retainWorker?: boolean;
  cacheCompilerModule?: boolean;
  /** Idle worker lifetime; defaults to 30 seconds, at most five minutes. */
  idleTimeoutMs?: number;
}
export interface EngineOptions {
  cpp?: CppPerformanceOptions;
}
export type { CppCacheLimits, ObjectCacheStats } from './cpp-object-cache';

export type Timing = { totalMs: number; buildMs: number; runMs: number };
export type CompletedResult = { type: 'completed'; exitCode: number; timing: Timing };
export type StoppedResult = { type: 'stopped' };
export type ErrorResult = { type: 'error'; error: { type: string; message: string } };
export type RunResult = CompletedResult | StoppedResult | ErrorResult;
export type FsNode = string | DirNode;
export type DirNode = { [name: string]: FsNode };

/** Bounded, source-free compilation measurements from one worker job. */
export interface CppTranslationUnitStatistics {
  hits: number;
  misses: number;
  bypasses: Array<{ source: string; reason: string }>;
  hit_sources: string[];
  invalid_sources: string[];
  hash_ms: number;
  lookup_ms: number;
  compile_ms: number;
  link_ms: number;
}
export interface CppJobStatistics {
  operation: 'compile' | 'run';
  workerReadyMs: number;
  workerReused: boolean;
  cache?: CppTranslationUnitStatistics;
  compilerModule?: CompilerModuleStats;
}
export interface CppCacheStatistics extends ObjectCacheStats {
  generation: number;
  workerRetained: boolean;
  workerRetirements: number;
  compilerModuleIdentityBytes: number;
  latest?: CppJobStatistics;
}

type InternalMessage = WorkerOut | CompilerModuleMessage;
interface Job {
  generation: number;
  settled: boolean;
  buildOnly: boolean;
  debug: boolean;
  start: WorkerStart;
  opener?: HostDeviceOpener;
  device?: HostDeviceSession;
  startedAt: number;
  stats: CppJobStatistics;
  staged: CppObjectCache;
  resolve: (result: RunResult) => void;
}
interface WorkerSlot {
  worker: Worker;
  ready: boolean;
  remove: () => void;
}

export class Engine {
  public readonly stdout = new Stdout(1);
  public readonly stderr = new Stdout(2);
  public readonly stdin = new Stdin();
  public readonly debugger: Debugger;
  public readonly lang: Lang;
  public readonly scope?: Readonly<EngineScope>;
  public fs: DirNode = {};
  public binaryFiles: Record<string, Uint8Array> = {};
  public cppArtifacts?: CppArtifacts;
  public hostDevice?: HostDeviceOpener;

  private readonly options?: Readonly<CppPerformanceOptions>;
  private readonly objects: CppObjectCache;
  private generation = 0;
  private disposed = false;
  private active?: Job;
  private promise?: Promise<RunResult>;
  private slot?: WorkerSlot;
  private idleTimer?: ReturnType<typeof setTimeout>;
  private compilerModule?: RetainedCompilerModule;
  private retirements = 0;
  private latest?: CppJobStatistics;

  static async create(lang: Lang, options: EngineOptions = {}): Promise<Engine> {
    const capturedOptions = structuredClone(options);
    initialization ??= init({ module_or_path: wasmBinary }).catch((error: unknown) => {
      initialization = undefined;
      throw error;
    });
    await initialization;
    const engine = new Engine(lang, capturedOptions);
    if (typeof window !== 'undefined' && typeof fetch !== 'undefined')
      for (const url of prefetch_urls(lang))
        void fetch(url, { cache: 'force-cache' })
          .then((response) => response.arrayBuffer())
          .catch(() => {});
    return engine;
  }

  private constructor(lang: Lang, options: EngineOptions) {
    const cpp = options.cpp;
    if (cpp) {
      if (lang !== 'c') throw new TypeError('C++ performance options require C/C++');
      if (!cpp.scope?.projectId || !cpp.scope.accountId)
        throw new TypeError('C++ performance options require project and account identity');
      const idleTimeoutMs = cpp.idleTimeoutMs ?? 30_000;
      if (!Number.isSafeInteger(idleTimeoutMs) || idleTimeoutMs < 1 || idleTimeoutMs > 300_000)
        throw new RangeError('Compiler idle timeout must be between 1 and 300000 milliseconds');
      this.scope = Object.freeze({ ...cpp.scope });
      this.options = Object.freeze({ ...cpp, scope: this.scope, idleTimeoutMs });
    }
    this.objects = new CppObjectCache(typeof cpp?.objectCache === 'object' ? cpp.objectCache : {});
    this.debugger = new Debugger();
    this.lang = lang;
  }

  public get cacheStats(): CppCacheStatistics {
    return structuredClone({
      ...this.objects.stats(),
      generation: this.generation,
      workerRetained: !!this.slot && !this.active,
      workerRetirements: this.retirements,
      compilerModuleIdentityBytes: this.compilerModule?.bytes.byteLength ?? 0,
      latest: this.latest
    });
  }

  /** Clears all retained code/objects. An active job may finish but cannot repopulate them. */
  public clearCaches(): void {
    this.generation++;
    this.objects.clear();
    this.active?.staged.clear();
    this.compilerModule = undefined;
    if (!this.active) this.retireWorker();
  }

  /** Stops synchronously, drops retained resources, and resolves after job cleanup. */
  public stop(): Promise<void> {
    this.clearCaches();
    const drain = this.promise;
    this.retireWorker();
    if (this.active) this.finish(this.active, { type: 'stopped' });
    return drain ? drain.then(() => {}) : Promise.resolve();
  }

  /** Required on project/account replacement and teardown. This Engine cannot be reused. */
  public async dispose(): Promise<void> {
    this.disposed = true;
    await this.stop();
  }

  /** Every Run links current objects and starts a fresh student runtime. */
  public run(): Promise<RunResult> {
    return this.submit(false);
  }

  /** Compiles Run-profile C++ translation units only; never links, runs, or opens devices. */
  public compile(): Promise<RunResult> {
    return this.submit(true);
  }

  private submit(buildOnly: boolean): Promise<RunResult> {
    if (this.disposed) return Promise.resolve(errorResult(new Error('Engine is disposed')));
    if (this.active)
      return Promise.resolve(errorResult(new Error('An engine job is already active')));
    if (buildOnly && this.lang !== 'c')
      return Promise.resolve(errorResult(new Error('Compile-only is supported for C/C++')));
    let snapshot: Pick<WorkerStart, 'fs' | 'binary_files' | 'cpp_artifacts'>;
    try {
      snapshot = {
        fs: structuredClone(this.fs),
        binary_files: Object.fromEntries(
          Object.entries(this.binaryFiles).map(([path, bytes]) => [path, bytes.slice()])
        ),
        cpp_artifacts: structuredClone(this.cppArtifacts) ?? null
      };
    } catch (error) {
      return Promise.resolve(errorResult(error));
    }
    const debug = !buildOnly && this.debugger.enabled;
    const opener = buildOnly ? undefined : this.hostDevice;
    if (
      opener !== undefined &&
      (typeof opener !== 'function' || !['c', 'python'].includes(this.lang))
    )
      return Promise.resolve(errorResult(new TypeError('Invalid host device configuration')));
    clearTimeout(this.idleTimer);
    if (debug) {
      this.retireWorker();
      this.compilerModule = undefined;
    }
    this.stdin[Internals].clear();
    const reused = !!this.slot;
    let resolve!: (value: RunResult) => void;
    const promise = new Promise<RunResult>((done) => {
      resolve = done;
    });
    this.promise = promise;
    const job: Job = {
      generation: this.generation,
      settled: false,
      buildOnly,
      debug,
      opener,
      startedAt: performance.now(),
      resolve,
      staged: new CppObjectCache(this.objects.limits),
      stats: { operation: buildOnly ? 'compile' : 'run', workerReadyMs: 0, workerReused: reused },
      start: {
        ...snapshot,
        lang: this.lang,
        is_debug: debug,
        build_only: buildOnly,
        stdin_buffer: this.stdin[Internals].buffer,
        cpp_cache:
          this.options?.objectCache && !debug
            ? { generation: this.generation, entries: this.objects.snapshot() }
            : null
      }
    };
    this.active = job;
    try {
      if (!this.slot) this.makeWorker();
      if (!this.current(job)) return promise;
      if (this.slot!.ready) this.dispatch(job);
    } catch (error) {
      this.finish(job, errorResult(error));
    }
    return promise;
  }

  private current(job: Job): boolean {
    return this.active === job && !job.settled;
  }

  private makeWorker(): void {
    const worker = createEngineWorker();
    const slot: WorkerSlot = { worker, ready: false, remove: () => {} };
    this.slot = slot;
    const onMessage = (event: MessageEvent<InternalMessage>) => {
      if (this.slot !== slot) return;
      const message = event.data;
      const job = this.active;
      if (message.type === 'ready') {
        if (slot.ready) return;
        slot.ready = true;
        if (job && this.current(job)) {
          try {
            this.dispatch(job);
          } catch (error) {
            this.finish(job, errorResult(error));
          }
        }
        return;
      }
      if (!job || !this.current(job)) return;
      try {
        if (message.type === 'stop') {
          this.finish(job, {
            type: 'completed',
            exitCode: message.exit_code,
            timing: {
              totalMs: performance.now() - job.startedAt,
              buildMs: message.build_ms,
              runMs: message.run_ms
            }
          });
        } else if (message.type === 'error') {
          this.finish(job, errorResult(new Error(message.message)));
        } else if (
          message.type === 'cpp_cache_object' &&
          message.generation === this.generation &&
          job.generation === this.generation
        ) {
          job.staged.put(message);
        } else if (message.type === 'cpp_cache_stats' && message.generation === job.generation) {
          job.stats.cache = message.stats;
        } else if (
          message.type === 'compiler_module' &&
          job.generation === this.generation &&
          this.options?.cacheCompilerModule &&
          !job.debug
        ) {
          this.compilerModule = message.value;
        } else if (message.type === 'compiler_module_stats') {
          job.stats.compilerModule = message;
        }
      } catch (error) {
        this.finish(job, errorResult(error));
      }
    };
    const onError = (event: ErrorEvent) => {
      if (this.slot !== slot) return;
      if (this.active)
        this.finish(this.active, errorResult(event.error ?? new Error(event.message)));
      else {
        this.retireWorker();
        this.compilerModule = undefined;
      }
    };
    const onMessageError = () =>
      onError({ message: 'Unreadable execution worker message' } as ErrorEvent);
    // DAP must observe completion before central cleanup retires a Debug worker.
    this.debugger[Internals].attach(worker);
    worker.addEventListener('message', onMessage);
    worker.addEventListener('error', onError);
    worker.addEventListener('messageerror', onMessageError);
    slot.remove = () => {
      worker.removeEventListener('message', onMessage);
      worker.removeEventListener('error', onError);
      worker.removeEventListener('messageerror', onMessageError);
      this.debugger[Internals].detach(worker);
    };
  }

  private dispatch(job: Job): void {
    if (!this.current(job) || !this.slot) return;
    const worker = this.slot.worker;
    job.stats.workerReadyMs = performance.now() - job.startedAt;
    this.stdout[Internals].attach(worker);
    this.stderr[Internals].attach(worker);
    this.debugger[Internals].beginRun();
    if (job.opener) {
      job.device = new HostDeviceSession((error) => this.finish(job, errorResult(error)));
      job.device.attach(worker);
      job.device.open(job.opener);
      // Device openers are observable user callbacks and may synchronously Stop/dispose.
      if (!this.current(job)) return;
      job.start.host_device = job.device.workerStart;
    }
    const cacheCompilerModule = !!this.options?.cacheCompilerModule && !job.debug;
    worker.postMessage({
      type: 'configure_cpp',
      cacheCompilerModule,
      compilerModule: cacheCompilerModule ? this.compilerModule : undefined
    });
    if (this.current(job)) worker.postMessage(job.start);
  }

  private finish(job: Job, result: RunResult): void {
    if (!this.current(job)) return;
    // Fence before cleanup or any observable callback can dispatch queued-ready work.
    job.settled = true;
    const worker = this.slot?.worker;
    if (worker) {
      this.stdout[Internals].detach(worker);
      this.stderr[Internals].detach(worker);
    }
    const cleanupError = job.device?.close();
    if (cleanupError) result = errorResult(cleanupError);
    const healthy =
      result.type === 'completed' &&
      result.exitCode === 0 &&
      !job.debug &&
      job.generation === this.generation &&
      !this.disposed;
    if (job.generation === this.generation)
      for (const source of job.stats.cache?.invalid_sources ?? []) this.objects.delete(source);
    if (healthy) {
      for (const source of job.stats.cache?.hit_sources ?? []) this.objects.touch(source);
      for (const entry of job.staged.snapshot()) this.objects.put(entry);
    }
    this.latest = job.stats;
    this.stdin[Internals].clear();
    if (!healthy || !this.options?.retainWorker || this.lang !== 'c') this.retireWorker();
    if (healthy && (this.options?.retainWorker || this.options?.cacheCompilerModule))
      this.idleTimer = setTimeout(() => {
        this.retireWorker();
        this.compilerModule = undefined;
      }, this.options.idleTimeoutMs);
    if (!healthy) this.compilerModule = undefined;
    this.active = undefined;
    this.promise = undefined;
    job.resolve(result);
  }

  private retireWorker(): void {
    clearTimeout(this.idleTimer);
    const slot = this.slot;
    if (!slot) return;
    this.slot = undefined;
    slot.remove();
    this.stdout[Internals].detach(slot.worker);
    this.stderr[Internals].detach(slot.worker);
    slot.worker.terminate();
    this.retirements++;
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

  private buffer = new SharedArrayBuffer(Stdin.BUFFER_SIZE);
  private generation = 0;
  private indices: Int32Array;
  private data: Int8Array;

  [Internals]: {
    clear(): void;
    buffer: SharedArrayBuffer;
  };

  constructor() {
    this.indices = new Int32Array(this.buffer, 0, 2);
    this.data = new Int8Array(this.buffer, Stdin.HEADER_SIZE);
    const getBuffer = () => this.buffer;
    this[Internals] = {
      clear: this.clear.bind(this),
      get buffer() {
        return getBuffer();
      }
    };
  }

  private clear() {
    this.generation++;
    Atomics.notify(this.indices, Stdin.READ_IDX);
    this.buffer = new SharedArrayBuffer(Stdin.BUFFER_SIZE);
    this.indices = new Int32Array(this.buffer, 0, 2);
    this.data = new Int8Array(this.buffer, Stdin.HEADER_SIZE);
  }

  public async write(value: Uint8Array | string): Promise<void> {
    const chunk = typeof value === 'string' ? Stdin.encoder.encode(value) : value;
    return this.writeBytes(chunk);
  }

  private async writeBytes(chunk: Uint8Array): Promise<void> {
    const { DATA_SIZE, READ_IDX, WRITE_IDX } = Stdin;
    let offset = 0;
    const generation = this.generation;

    while (offset < chunk.length && generation === this.generation) {
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
