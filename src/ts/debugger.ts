/// <reference types="node" preserve="true" />
import EventEmitter from 'events';

/* eslint-disable-next-line @typescript-eslint/no-unused-vars */
import { type Engine } from '.';
import { DapAdapter, WorkerOut } from '../../pkg/engine';
import { Internals } from './util';

type DebuggerEventMap = {
  event: [unknown];
  artifact: [Artifact];
};

export class Artifact {
  public readonly name: string;
  public readonly data: Uint8Array;

  constructor(name: string, data: Uint8Array) {
    this.name = name;
    this.data = data;
  }

  public download() {
    const bytes = new Uint8Array(this.data);
    const blob = new Blob([bytes]);
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = this.name;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    URL.revokeObjectURL(url);
  }
}

export class Debugger extends EventEmitter<DebuggerEventMap> {
  /**
   * Access to internal properties of the debugger.
   * These are put under a special symbol so that they cannot be accessed by
   * clients of the library.
   */
  [Internals]: {
    attach(worker: Worker): void;
    detach(worker: Worker): void;
    beginRun(): void;
  };

  private readonly dap: DapAdapter;

  /**
   * Whether or not debugging is enabled.
   * Changes will takes effect during the next call to {@link Engine.run}.
   */
  public enabled: boolean = true;

  /**
   * When true, internal frames (bdb, bridge) are omitted from DAP `stackTrace`
   * responses. Internal frames are always tagged with `presentationHint: "subtle"`
   * regardless, so raw clients can filter themselves.
   * Must be set before calling {@link Engine.run}.
   */
  public set filterInternals(value: boolean) {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (this.dap as any).setFilterInternals(value);
  }

  constructor() {
    super();
    this[Internals] = {
      attach: this.attach.bind(this),
      detach: this.detach.bind(this),
      beginRun: () => this.dap.resetRun()
    };

    this.onMessage = this.onMessage.bind(this);
    this.dap = new DapAdapter();
    this.dap.on(this.onMessage);
  }

  public send(message: unknown): unknown {
    // return the response from the DAP adapter. This is sync. DAP events are async and emitted through the on('event') listener.
    return this.dap.sendMessage(message);
  }

  private readonly artifactListener = (event: MessageEvent<WorkerOut>) => {
    if (event.data.type !== 'artifact') return;
    const artifact = new Artifact(event.data.name, new Uint8Array(event.data.data));
    this.emit('artifact', artifact);
  };

  private attach(worker: Worker) {
    worker.addEventListener('message', this.artifactListener);
    this.dap.attach(worker);
  }

  private detach(worker: Worker) {
    worker.removeEventListener('message', this.artifactListener);
    this.dap.detach();
  }

  private onMessage(message: unknown) {
    this.emit('event', message);
  }
}
