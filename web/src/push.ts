import { bytesToB64url, b64urlToBytes } from './lib/base64';
import type { HostConnection } from './net/types';
import { loadPushEnabled, loadVapid, saveVapid, savePushEnabled, type VapidKeys } from './storage';

export type PushSupport = 'supported' | 'unsupported' | 'ios_needs_install' | 'insecure';

export function isIos(): boolean {
  const ua = navigator.userAgent;
  return /iPhone|iPad|iPod/.test(ua) || (/Macintosh/.test(ua) && navigator.maxTouchPoints > 1);
}

export function isStandalone(): boolean {
  return (
    window.matchMedia?.('(display-mode: standalone)').matches ||
    (navigator as Navigator & { standalone?: boolean }).standalone === true
  );
}

export function pushSupport(): PushSupport {
  if (!window.isSecureContext) return 'insecure';
  if (isIos() && !isStandalone()) return 'ios_needs_install';
  if (!('serviceWorker' in navigator) || !('PushManager' in window) || !('Notification' in window)) return 'unsupported';
  return 'supported';
}

export function registerServiceWorker(): void {
  if (!('serviceWorker' in navigator) || import.meta.env.DEV) return;
  window.addEventListener('load', () => {
    navigator.serviceWorker.register('/sw.js', { scope: '/' }).catch((err) => console.warn('sw register failed', err));
  });
}

async function getVapid(): Promise<VapidKeys> {
  const existing = await loadVapid();
  if (existing) return existing;
  const pair = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, true, ['sign', 'verify']);
  const keys: VapidKeys = {
    publicJwk: await crypto.subtle.exportKey('jwk', pair.publicKey),
    privateJwk: await crypto.subtle.exportKey('jwk', pair.privateKey),
  };
  await saveVapid(keys);
  return keys;
}

/** Uncompressed P-256 point 0x04 || X || Y (65 bytes). */
function rawPublic(jwk: JsonWebKey): Uint8Array<ArrayBuffer> {
  const x = b64urlToBytes(jwk.x!);
  const y = b64urlToBytes(jwk.y!);
  const out = new Uint8Array(65);
  out[0] = 4;
  out.set(x, 1);
  out.set(y, 33);
  return out;
}

function sameKey(a: ArrayBuffer | null, b: Uint8Array): boolean {
  if (!a) return false;
  const x = new Uint8Array(a);
  return x.length === b.length && x.every((v, i) => v === b[i]);
}

async function registration(): Promise<ServiceWorkerRegistration> {
  const existing = await navigator.serviceWorker.getRegistration('/');
  if (existing) return existing;
  await navigator.serviceWorker.register('/sw.js', { scope: '/' });
  return navigator.serviceWorker.ready;
}

export interface PushSubscriptionReq {
  endpoint: string;
  p256dh: string;
  auth: string;
  vapid_private: string;
}

/** Returns the current subscription (creating it if `create`) in the wire format. */
export async function currentSubscription(create: boolean): Promise<PushSubscriptionReq | null> {
  if (pushSupport() !== 'supported') return null;
  const vapid = await getVapid();
  const appKey = rawPublic(vapid.publicJwk);
  const reg = await registration();
  let sub = await reg.pushManager.getSubscription();
  if (sub && !sameKey(sub.options.applicationServerKey, appKey)) {
    await sub.unsubscribe();
    sub = null;
  }
  if (!sub) {
    if (!create) return null;
    sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: appKey });
  }
  const p256dh = sub.getKey('p256dh');
  const auth = sub.getKey('auth');
  if (!p256dh || !auth) throw new Error('订阅缺少密钥');
  return {
    endpoint: sub.endpoint,
    p256dh: bytesToB64url(new Uint8Array(p256dh)),
    auth: bytesToB64url(new Uint8Array(auth)),
    vapid_private: vapid.privateJwk.d!,
  };
}

/** Must be called from a user gesture (iOS). */
export async function enablePush(conns: HostConnection[]): Promise<{ ok: number; failed: string[] }> {
  const perm = await Notification.requestPermission();
  if (perm !== 'granted') throw new Error(perm === 'denied' ? '通知权限已被拒绝，请在系统设置中开启' : '未授予通知权限');
  const sub = await currentSubscription(true);
  if (!sub) throw new Error('当前浏览器不支持推送');
  await savePushEnabled(true);
  return subscribeHosts(conns, sub);
}

export async function subscribeHosts(conns: HostConnection[], sub: PushSubscriptionReq): Promise<{ ok: number; failed: string[] }> {
  let ok = 0;
  const failed: string[] = [];
  await Promise.all(
    conns.map(async (c) => {
      if (c.status !== 'online') {
        failed.push(c.hostHello?.host_name ?? c.host.slice(0, 8));
        return;
      }
      try {
        await c.request({ op: 'push_subscribe', ...sub });
        ok++;
      } catch {
        failed.push(c.hostHello?.host_name ?? c.host.slice(0, 8));
      }
    }),
  );
  return { ok, failed };
}

/** Resend the subscription to a host that just came online (if push is enabled). */
export async function syncPushTo(conn: HostConnection): Promise<void> {
  if (!(await loadPushEnabled()) || pushSupport() !== 'supported' || Notification.permission !== 'granted') return;
  const sub = await currentSubscription(false);
  if (sub) await conn.request({ op: 'push_subscribe', ...sub }).catch(() => undefined);
}

export async function disablePush(conns: HostConnection[]): Promise<void> {
  await savePushEnabled(false);
  await Promise.all(conns.filter((c) => c.status === 'online').map((c) => c.request({ op: 'push_unsubscribe' }).catch(() => undefined)));
  if (pushSupport() !== 'supported') return;
  const reg = await navigator.serviceWorker.getRegistration('/');
  const sub = await reg?.pushManager.getSubscription();
  await sub?.unsubscribe();
}

export async function pushState(): Promise<'on' | 'off' | 'denied'> {
  if (pushSupport() !== 'supported') return 'off';
  if (Notification.permission === 'denied') return 'denied';
  if (!(await loadPushEnabled()) || Notification.permission !== 'granted') return 'off';
  const reg = await navigator.serviceWorker.getRegistration('/');
  const sub = await reg?.pushManager.getSubscription();
  return sub ? 'on' : 'off';
}
