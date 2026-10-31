// Not `&inline`: inlined data:/blob: workers cannot fetch same-origin Wasm paths
// like `/engine_bg.wasm` (Failed to parse URL / opaque origin under COEP).
import RustWorker from './worker?worker';

/** One explicit internal construction seam, also used by deterministic lifecycle tests. */
export function createEngineWorker(): Worker {
  return new RustWorker();
}
