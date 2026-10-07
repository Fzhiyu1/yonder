import type { HostHello } from '../proto/generated/HostHello';
import type { PairPayload } from '../proto/generated/PairPayload';
import { getDeviceKeypair, getDeviceName, type StoredHost } from '../storage';
import { HostLink } from './link';
import { RelayConnection } from './relay';
import { describeError, RequestError, type HostConnection } from './types';

/** Source of host connections: the real relay manager or the in-memory mock. */
export interface ConnectionProvider {
  /** Make the set of live connections match the paired hosts. */
  sync(hosts: StoredHost[]): void;
  get(host: string): HostConnection | undefined;
  /** Handshake with a pair token; resolves with the HostHello once authorized. */
  pair(p: PairPayload): Promise<{ hello: HostHello; conn: HostConnection }>;
  /** Called when a new connection appears (sync or pair). */
  onConnection(fn: (host: string, conn: HostConnection) => void): () => void;
}

export class RelayManager implements ConnectionProvider {
  private relays = new Map<string, RelayConnection>();
  private links = new Map<string, HostLink>();
  private hostRelay = new Map<string, string>();
  private listeners = new Set<(host: string, conn: HostConnection) => void>();
  private deps = { getKeypair: getDeviceKeypair, getDeviceName };

  onConnection(fn: (host: string, conn: HostConnection) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  get(host: string): HostConnection | undefined {
    return this.links.get(host);
  }

  private relayFor(url: string): RelayConnection {
    let r = this.relays.get(url);
    if (!r) {
      r = new RelayConnection(url, getDeviceKeypair);
      this.relays.set(url, r);
      r.start();
    }
    return r;
  }

  private updateWatch(): void {
    for (const [url, relay] of this.relays) {
      const hosts = [...this.hostRelay.entries()].filter(([, u]) => u === url).map(([h]) => h);
      if (!hosts.length) {
        relay.stop();
        this.relays.delete(url);
      } else {
        relay.setWatched(hosts);
      }
    }
  }

  sync(hosts: StoredHost[]): void {
    const want = new Map(hosts.map((h) => [h.host, h]));
    for (const [host, link] of this.links) {
      const h = want.get(host);
      if (!h || this.hostRelay.get(host) !== h.relay) {
        link.close();
        this.links.delete(host);
        this.hostRelay.delete(host);
      }
    }
    for (const h of hosts) {
      if (this.links.has(h.host)) continue;
      this.hostRelay.set(h.host, h.relay);
      const relay = this.relayFor(h.relay);
      const link = new HostLink(h.host, relay, this.deps);
      this.links.set(h.host, link);
      this.listeners.forEach((fn) => fn(h.host, link));
    }
    this.updateWatch();
  }

  pair(p: PairPayload): Promise<{ hello: HostHello; conn: HostConnection }> {
    const existing = this.links.get(p.host);
    if (existing) {
      existing.close();
      this.links.delete(p.host);
    }
    this.hostRelay.set(p.host, p.relay);
    const relay = this.relayFor(p.relay);
    this.updateWatch();
    return new Promise((resolve, reject) => {
      let link: HostLink | undefined = undefined;
      const fail = (msg: string, code: string) => {
        clearTimeout(timer);
        link?.close();
        this.links.delete(p.host);
        this.hostRelay.delete(p.host);
        this.updateWatch();
        reject(new RequestError(code, msg));
      };
      const timer = setTimeout(() => {
        const reason = link?.error ?? relay.lastError;
        fail(reason ? describeError(reason) : '连接主机超时，请确认主机在线', 'timeout');
      }, 25_000);
      link = new HostLink(p.host, relay, this.deps, p.token, (hello) => {
        if (hello.ok) {
          clearTimeout(timer);
          this.links.set(p.host, link!);
          this.listeners.forEach((fn) => fn(p.host, link!));
          resolve({ hello, conn: link! });
        } else {
          fail(describeError(hello.error ?? 'not_paired'), hello.error ?? 'not_paired');
        }
      });
    });
  }
}
