import { createStore, del, get, set } from 'idb-keyval';
import { loadWasm } from './net/wasm';
import type { AgentKind } from './proto/generated/AgentKind';
import type { ApprovalMode } from './proto/generated/ApprovalMode';

export interface StoredHost {
  /** Host static public key (base64url); the host identity. */
  host: string;
  host_name: string;
  relay: string;
  paired_at: number;
  /** Local display label (overrides host_name). */
  label?: string;
  os?: string;
}

export type ThemePref = 'system' | 'light' | 'dark';

export interface Settings {
  theme: ThemePref;
  termFontSize: number;
  /** Approval mode last picked per agent for new chats (unset: follow the host's configuration). */
  approvalByAgent: Partial<Record<AgentKind, ApprovalMode>>;
}

export const DEFAULT_SETTINGS: Settings = { theme: 'system', termFontSize: 13, approvalByAgent: {} };

const store = typeof indexedDB !== 'undefined' ? createStore('yonder', 'kv') : undefined;
const memory = new Map<string, unknown>();

async function read<T>(key: string): Promise<T | undefined> {
  if (!store) return memory.get(key) as T | undefined;
  try {
    return await get<T>(key, store);
  } catch {
    return memory.get(key) as T | undefined;
  }
}

async function write<T>(key: string, value: T): Promise<void> {
  memory.set(key, value);
  if (!store) return;
  try {
    await set(key, value, store);
  } catch {
    /* private mode etc.: keep in memory */
  }
}

async function remove(key: string): Promise<void> {
  memory.delete(key);
  if (store) await del(key, store).catch(() => undefined);
}

let keypairPromise: Promise<string> | null = null;

/** Device X25519 keypair JSON `{private, public}`; generated on first run. */
export function getDeviceKeypair(): Promise<string> {
  if (!keypairPromise) {
    keypairPromise = (async () => {
      const existing = await read<string>('device_keypair');
      if (existing) return existing;
      const w = await loadWasm();
      const kp = w.generateKeypair();
      await write('device_keypair', kp);
      return kp;
    })();
    keypairPromise.catch(() => {
      keypairPromise = null;
    });
  }
  return keypairPromise;
}

export async function getDevicePublic(): Promise<string> {
  const w = await loadWasm();
  return w.publicKeyOf(await getDeviceKeypair());
}

export function defaultDeviceName(ua = typeof navigator !== 'undefined' ? navigator.userAgent : ''): string {
  const touchMac = typeof navigator !== 'undefined' && /Macintosh/.test(ua) && navigator.maxTouchPoints > 1;
  if (/iPad/.test(ua) || touchMac) return 'iPad';
  if (/iPhone|iPod/.test(ua)) return 'iPhone';
  if (/Android/.test(ua)) return 'Android';
  if (/Macintosh|Mac OS X/.test(ua)) return 'Mac';
  if (/Windows/.test(ua)) return 'Windows';
  if (/Linux|X11/.test(ua)) return 'Linux';
  return '浏览器';
}

export async function getDeviceName(): Promise<string> {
  return (await read<string>('device_name')) || defaultDeviceName();
}

export function setDeviceName(name: string): Promise<void> {
  return write('device_name', name.trim());
}

export async function loadHosts(): Promise<StoredHost[]> {
  return (await read<StoredHost[]>('hosts')) ?? [];
}

export function saveHosts(hosts: StoredHost[]): Promise<void> {
  return write('hosts', hosts);
}

export async function loadSettings(): Promise<Settings> {
  return { ...DEFAULT_SETTINGS, ...((await read<Partial<Settings>>('settings')) ?? {}) };
}

export function saveSettings(s: Settings): Promise<void> {
  return write('settings', s);
}

/** Device VAPID key pair (ECDSA P-256, JWK) shared by all hosts. */
export interface VapidKeys {
  publicJwk: JsonWebKey;
  privateJwk: JsonWebKey;
}

export function loadVapid(): Promise<VapidKeys | undefined> {
  return read<VapidKeys>('vapid');
}

export function saveVapid(k: VapidKeys): Promise<void> {
  return write('vapid', k);
}

export async function loadPushEnabled(): Promise<boolean> {
  return (await read<boolean>('push_enabled')) ?? false;
}

export function savePushEnabled(v: boolean): Promise<void> {
  return v ? write('push_enabled', true) : remove('push_enabled');
}
