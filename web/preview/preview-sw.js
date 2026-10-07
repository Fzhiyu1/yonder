// Yonder preview service worker, served from the preview origin (separate from the app) with
// scope "/". Pages shown in the artifact viewer run on this origin; every request they make is
// answered by the Yonder app in the parent window, which loads it from the host over the
// end-to-end encrypted channel. The server only serves the bridge page and this script, so the
// relay never sees preview content.
//
// Which preview a request belongs to:
//   /__p/<id>/<path>   prefixed pages (files: relative assets stay under the prefix)
//   clientId -> id     subresources of a document this worker created
//   referrer           navigations (bridge URL, prefixed URL, or a known page URL)
'use strict';

const BRIDGE = '/__preview/bridge.html';
const ID = /^[0-9a-f]{32}$/;
const PREFIX = /^\/__p\/([0-9a-f]{32})(\/.*)?$/;
const OWNER_CACHE = 'yonder-preview-owners-v1';
const FETCH_TIMEOUT_MS = 120000;
const NULL_BODY = new Set([101, 204, 205, 304]);
const REDIRECT = new Set([301, 302, 303, 307, 308]);

/** clientId -> preview id. Mirrored in Cache Storage because idle workers are stopped. */
const owners = new Map();
let lastId;
let writes = 0;

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));

function ownerKey(clientId) {
  return new URL('/__owner/' + encodeURIComponent(clientId), self.location.origin).href;
}

async function prune(cache) {
  const live = new Set((await self.clients.matchAll({ includeUncontrolled: true })).map((c) => c.id));
  for (const req of await cache.keys()) {
    const clientId = decodeURIComponent(new URL(req.url).pathname.slice('/__owner/'.length));
    if (!live.has(clientId)) {
      owners.delete(clientId);
      await cache.delete(req);
    }
  }
}

async function remember(clientId, id) {
  if (!clientId) return;
  owners.set(clientId, id);
  try {
    const cache = await caches.open(OWNER_CACHE);
    await cache.put(ownerKey(clientId), new Response(id));
    if (++writes % 50 === 0) await prune(cache);
  } catch {
    // Storage may be unavailable (private mode); the in-memory map still works.
  }
}

async function ownerOf(clientId) {
  if (!clientId) return undefined;
  const hit = owners.get(clientId);
  if (hit) return hit;
  try {
    const cache = await caches.open(OWNER_CACHE);
    const res = await cache.match(ownerKey(clientId));
    const id = res && (await res.text());
    if (id && ID.test(id)) {
      owners.set(clientId, id);
      return id;
    }
  } catch {
    // ignore
  }
  return undefined;
}

function windows() {
  return self.clients.matchAll({ type: 'window', includeUncontrolled: true });
}

function bridgeIdOf(url) {
  if (url.origin !== self.location.origin || url.pathname !== BRIDGE) return undefined;
  const id = url.searchParams.get('id');
  return id && ID.test(id) ? id : undefined;
}

function stripHash(href) {
  const i = href.indexOf('#');
  return i < 0 ? href : href.slice(0, i);
}

async function referrerOwner(referrer) {
  let url;
  try {
    url = new URL(referrer);
  } catch {
    url = undefined;
  }
  const all = await windows();
  const live = new Set(all.map((c) => bridgeIdOf(new URL(c.url))).filter(Boolean));
  if (url && url.origin === self.location.origin) {
    const fromBridge = bridgeIdOf(url);
    if (fromBridge) return fromBridge;
    const m = url.pathname.match(PREFIX);
    if (m) return m[1];
    // Dev-server pages keep their real paths: find the preview with a document at that URL.
    const ids = new Set();
    for (const c of all) {
      if (stripHash(c.url) !== stripHash(url.href)) continue;
      const id = await ownerOf(c.id);
      if (id) ids.add(id);
    }
    if (ids.size === 1) return [...ids][0];
    if (ids.size > 1 && lastId && ids.has(lastId)) return lastId;
  }
  if (live.size === 1) return [...live][0];
  return lastId && live.has(lastId) ? lastId : undefined;
}

async function resolve(event, url) {
  const m = url.pathname.match(PREFIX);
  if (m) return { id: m[1], path: (m[2] || '/') + url.search, prefixed: true };
  // `../` out of a prefixed page lands on /__p/<something else>: never part of a preview.
  if (url.pathname.startsWith('/__p/')) return undefined;
  const path = url.pathname + url.search;
  if (event.request.mode === 'navigate') {
    const id = await referrerOwner(event.request.referrer);
    return id ? { id, path, prefixed: false } : undefined;
  }
  let id = await ownerOf(event.clientId);
  let prefixed = false;
  const client = event.clientId ? await self.clients.get(event.clientId) : undefined;
  const cm = client && new URL(client.url).pathname.match(PREFIX);
  if (cm) {
    id = id || cm[1];
    prefixed = true;
  }
  if (!id) id = await referrerOwner(event.request.referrer);
  return id ? { id, path, prefixed } : undefined;
}

async function findBridge(id) {
  for (const c of await windows()) {
    if (bridgeIdOf(new URL(c.url)) === id) return c;
  }
  return undefined;
}

function text(status, body) {
  return new Response(body, { status, headers: { 'content-type': 'text/plain; charset=utf-8', 'cache-control': 'no-store' } });
}

async function forward(target) {
  const bridge = await findBridge(target.id);
  if (!bridge) return text(502, 'Preview closed');
  const channel = new MessageChannel();
  const reply = new Promise((done) => {
    const timer = setTimeout(() => done(undefined), FETCH_TIMEOUT_MS);
    channel.port1.onmessage = (e) => {
      clearTimeout(timer);
      channel.port1.close();
      done(e.data);
    };
  });
  bridge.postMessage({ type: 'yonder-preview-fetch', id: target.id, path: target.path }, [channel.port2]);
  const d = await reply;
  if (!d || typeof d !== 'object') return text(504, 'Preview timed out');
  const status = Number.isInteger(d.status) && d.status >= 200 && d.status <= 599 ? d.status : 502;
  // Redirects stay inside the preview: the app turns same-server locations into paths.
  if (REDIRECT.has(status) && typeof d.location === 'string' && d.location.startsWith('/') && !d.location.startsWith('//')) {
    const to = (target.prefixed ? '/__p/' + target.id : '') + d.location;
    return Response.redirect(new URL(to, self.location.origin).href, status);
  }
  const body = NULL_BODY.has(status) || !(d.body instanceof ArrayBuffer) ? null : d.body;
  return new Response(body, {
    status,
    headers: { 'content-type': typeof d.type === 'string' && d.type ? d.type : 'application/octet-stream', 'cache-control': 'no-store' },
  });
}

self.addEventListener('fetch', (event) => {
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname.startsWith('/__preview/') || url.pathname === '/preview-sw.js') return;
  event.respondWith(
    (async () => {
      const target = await resolve(event, url);
      if (!target) return text(404, 'Not part of a preview');
      lastId = target.id;
      if (event.resultingClientId) await remember(event.resultingClientId, target.id);
      if (event.request.method !== 'GET' && event.request.method !== 'HEAD') return text(405, 'Only GET is supported in previews');
      return forward(target);
    })(),
  );
});
