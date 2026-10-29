import init, * as wasm from '../../pkg/engine.js';
import wasmBinary from '../../pkg/engine_bg.wasm';
import { CompilerModuleCache, ConfigureCompilerModule } from './compiler-module-cache';

const compilerModules = new CompilerModuleCache((message) => postMessage(message));

// Register before Rust's message handler so these internal envelopes never become jobs.
addEventListener('message', (event: MessageEvent<ConfigureCompilerModule>) => {
  if (event.data?.type !== 'configure_cpp') return;
  event.stopImmediatePropagation();
  compilerModules.configure(event.data);
});

try {
  await init({ module_or_path: wasmBinary });
  wasm.set_compiler_module_constructor((bytes: Uint8Array) => compilerModules.construct(bytes));
  wasm.set_compiler_instance_observer((milliseconds: number) =>
    compilerModules.instantiated(milliseconds)
  );
  wasm.main();
} catch (err) {
  postMessage({
    type: 'error',
    message: err instanceof Error ? err.message : String(err)
  });
}
