/* tslint:disable */
/* eslint-disable */
export const memory: WebAssembly.Memory;
export const __wbg_channel_free: (a: number, b: number) => void;
export const __wbg_handshake_free: (a: number, b: number) => void;
export const channel_decrypt: (a: number, b: number, c: number) => [number, number, number, number];
export const channel_encrypt: (a: number, b: number, c: number) => [number, number, number];
export const channel_hostHello: (a: number) => [number, number];
export const fingerprint: (a: number, b: number) => [number, number, number, number];
export const generateKeypair: () => [number, number, number, number];
export const handshake_new: (a: number, b: number, c: number, d: number) => [number, number, number];
export const handshake_readResponse: (a: number, b: number, c: number) => [number, number, number];
export const handshake_writeHello: (a: number, b: number, c: number) => [number, number, number, number];
export const publicKeyOf: (a: number, b: number) => [number, number, number, number];
export const relayAuthProof: (a: number, b: number, c: number, d: number, e: number, f: number) => [number, number, number, number];
export const __wbindgen_exn_store: (a: number) => void;
export const __externref_table_alloc: () => number;
export const __wbindgen_externrefs: WebAssembly.Table;
export const __wbindgen_free: (a: number, b: number, c: number) => void;
export const __wbindgen_malloc: (a: number, b: number) => number;
export const __externref_table_dealloc: (a: number) => void;
export const __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
export const __wbindgen_start: () => void;
