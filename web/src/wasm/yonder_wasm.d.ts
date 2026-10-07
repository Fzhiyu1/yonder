/* tslint:disable */
/* eslint-disable */

/**
 * Established encrypted channel.
 */
export class Channel {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Decrypt one Noise message. Returns the complete JSON message, or undefined while
     * more fragments are pending. Throws on authentication failure (drop the link).
     */
    decrypt(msg: Uint8Array): string | undefined;
    /**
     * Encrypt one JSON app message. Returns an array of Noise messages (Uint8Array[]).
     */
    encrypt(json: string): any;
    hostHello(): string;
}

/**
 * Handshake in progress (device side).
 */
export class Handshake {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Start a handshake with a host. `hello_json` is a `DeviceHello`.
     */
    constructor(keypair_json: string, host_pub_b64: string);
    /**
     * Consume message 2. Returns the channel; `hostHello()` on it gives the host JSON.
     */
    readResponse(msg: Uint8Array): Channel;
    /**
     * Handshake message 1 bytes.
     */
    writeHello(hello_json: string): Uint8Array;
}

export function fingerprint(public_b64: string): string;

/**
 * Returns a new device keypair as JSON `{"private": "...", "public": "..."}`.
 */
export function generateKeypair(): string;

/**
 * Public key (base64url) for a keypair JSON.
 */
export function publicKeyOf(keypair_json: string): string;

/**
 * Relay auth proof for a device. `nonce_b64` is the standard-base64 nonce from `challenge`.
 */
export function relayAuthProof(keypair_json: string, relay_pub_b64: string, nonce_b64: string): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_channel_free: (a: number, b: number) => void;
    readonly __wbg_handshake_free: (a: number, b: number) => void;
    readonly channel_decrypt: (a: number, b: number, c: number) => [number, number, number, number];
    readonly channel_encrypt: (a: number, b: number, c: number) => [number, number, number];
    readonly channel_hostHello: (a: number) => [number, number];
    readonly fingerprint: (a: number, b: number) => [number, number, number, number];
    readonly generateKeypair: () => [number, number, number, number];
    readonly handshake_new: (a: number, b: number, c: number, d: number) => [number, number, number];
    readonly handshake_readResponse: (a: number, b: number, c: number) => [number, number, number];
    readonly handshake_writeHello: (a: number, b: number, c: number) => [number, number, number, number];
    readonly publicKeyOf: (a: number, b: number) => [number, number, number, number];
    readonly relayAuthProof: (a: number, b: number, c: number, d: number, e: number, f: number) => [number, number, number, number];
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
