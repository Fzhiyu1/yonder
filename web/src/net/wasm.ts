import init, * as wasm from '../wasm/yonder_wasm.js';
import wasmUrl from '../wasm/yonder_wasm_bg.wasm?url';

export type Wasm = typeof wasm;

let ready: Promise<Wasm> | null = null;

/** Lazily initializes the wasm module once and returns its exports. */
export function loadWasm(): Promise<Wasm> {
  if (!ready) {
    ready = init({ module_or_path: wasmUrl }).then(() => wasm);
    ready.catch(() => {
      ready = null;
    });
  }
  return ready;
}
