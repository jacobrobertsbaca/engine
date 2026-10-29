import RustWorker from './worker?worker&inline';

/** One explicit internal construction seam, also used by deterministic lifecycle tests. */
export function createEngineWorker(): Worker {
  return new RustWorker();
}
