import fs from 'fs';
import path from 'path';
import { defineConfig, PluginOption } from 'vite';
import dts from 'vite-plugin-dts';

const outDir = 'dist';

export default defineConfig({
  plugins: [
    wasm(),
    dts({
      beforeWriteFile(filePath, content) {
        // Public EventEmitter types also work when consumers exclude ambient types.
        if (/from ['"]events['"]/.test(content))
          return { filePath, content: `/// <reference types="node" />\n${content}` };
      }
    })
  ],
  worker: {
    format: 'es',
    plugins: () => [wasm()]
  },
  build: {
    outDir,
    lib: {
      entry: 'src/ts/index.ts',
      name: 'engine'
    }
  }
});

function wasm(): PluginOption {
  const packageJson = JSON.parse(fs.readFileSync('package.json', 'utf-8')) as {
    name: string;
    version: string;
  };
  const packageName = packageJson.name;
  const packageVersion = packageJson.version;
  const npmDistUrl = `https://cdn.jsdelivr.net/npm/${packageName}@${packageVersion}/dist`;

  return {
    name: 'fix-wasm-import',
    async closeBundle() {
      const pkgDir = 'pkg';
      if (fs.existsSync(pkgDir)) {
        const wasmFiles = fs.readdirSync(pkgDir).filter((file) => file.endsWith('.wasm'));
        fs.mkdirSync(outDir, { recursive: true });
        for (const wasmFile of wasmFiles) {
          fs.copyFileSync(path.join(pkgDir, wasmFile), path.join(outDir, wasmFile));
        }
      }

      const file = path.join(outDir, 'debugger-sh.js');
      if (!fs.existsSync(file)) return; // in worker builds this file doesn't exist yet
      let js = fs.readFileSync(file, { encoding: 'utf-8' });
      js = js.replaceAll(/(["'`])data:application\/wasm[^"'`]*\1/g, '""');
      fs.writeFileSync(file, js, { encoding: 'utf-8' });
    },

    /**
     * Set up `.wasm` import so that importing it returns a buffer.
     * This is taken from wasmer-js: https://github.com/wasmerio/wasmer-js/blob/main/rollup.config.mjs
     */
    async load(id) {
      if (!id.endsWith('.wasm')) return;
      const devPort = process.env.WASM_DEV_PORT;
      const isRelease = Boolean(process.env.WASM_RELEASE);
      const wasmFile = path.basename(id);

      // In dev mode, we serve compiled Rust .wasm files from a local dev server
      if (devPort) return `export default new URL("http://localhost:${devPort}/${wasmFile}");`;

      // Same-origin asset path for Cloudflare static deploys. Use a plain
      // string (not `new URL(..., import.meta.url)`) so Next/webpack does not
      // try to resolve `/engine_bg.wasm` at build time. Requires a real
      // http(s) worker script — not `?worker&inline` data: URLs.
      const sameOriginWasm = process.env.WASM_ASSET_PATH;
      if (isRelease && sameOriginWasm) {
        return `export default ${JSON.stringify(sameOriginWasm)};`;
      }

      // Otherwise serve .wasm files from the npm registry via CDN
      if (isRelease)
        return `export default new URL(${JSON.stringify(`${npmDistUrl}/${wasmFile}`)});`;

      // Otherwise, in normal local build, we b64 encode the .wasm files into a buffer
      // and bake into the bundled output, resulting in a very large bundle size
      const binary = fs.readFileSync(id);
      const base64 = binary.toString('base64');
      return `
          const src = ${JSON.stringify(base64)};
          const buf = Uint8Array.from(atob(src), c => c.charCodeAt(0));
          export default buf;
        `;
    }
  };
}
