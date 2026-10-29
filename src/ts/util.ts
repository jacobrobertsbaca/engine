import { type RunResult } from './index.js';

/**
 * An exported symbol that allows us to define properties
 * on objects that are only available by library code and not consumers
 * of the library.
 */
export const Internals: unique symbol = Symbol();

export function errorResult(err: unknown): RunResult {
  if (err instanceof Error) {
    return {
      type: 'error',
      error: { type: err.constructor.name, message: err.message }
    };
  }
  return {
    type: 'error',
    error: { type: typeof err, message: String(err) }
  };
}
