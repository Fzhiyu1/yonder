import type { RelayClientMsg } from '../proto/generated/RelayClientMsg';
import type { RelayServerMsg } from '../proto/generated/RelayServerMsg';
import { Emitter } from '../lib/emitter';
import { decodeFrame, encodeFrame } from '../lib/frame';
import { loadWasm } from './wasm';

export type RelayState = 'connecting' | 'ready' | 'down';

export interface LinkHandler {
  onData(payload: Uint8Array): void;
  onClosed(reason: string): void;
}

export class OpenFailed extends Error {
  constructor(public reason: string) {
    super(reason);
  }
}

const BACKOFF = [500, 1000, 2000, 4000, 8000, 15000];
const PING_EVERY = 25_000;
const PONG_TIMEOUT = 10_000;

/** One WebSocket to one relay, shared by all hosts paired through it. */
export class RelayConnection {
  state: RelayState = 'down';
  lastError?: string;
  readonly stateChanged = new Emitter<RelayState>();
  readonly presenceChanged = new Emitter<{ host: string; online: boolean }>();
  readonly presence = new Map<string, boolean>();

  private ws?: WebSocket;
  private watched: string[] = [];
  private links = new Map<number, LinkHandler>();
  private pendingOpens = new Map<number, { resolve: (link: number) => void; reject: (e: Error) => void }>();
  private nextReq = 1;
  private attempt = 0;
  private retryTimer?: ReturnType<typeof setTimeout>;
  private pingTimer?: ReturnType<typeof setInterval>;
  private pongTimer?: ReturnType<typeof setTimeout>;
  private stopped = true;
  private cleanupGlobal?: () => void;

  constructor(
    readonly url: string,
    private getKeypair: () => Promise<string>,
  ) {}

  start(): void {
    if (!this.stopped) return;
    this.stopped = false;
    if (typeof window !== 'undefined') {
      const wake = () => this.wake();
      const onVis = () => {
        if (document.visibilityState === 'visible') wake();
      };
      window.addEventListener('online', wake);
      document.addEventListener('visibilitychange', onVis);
      this.cleanupGlobal = () => {
        window.removeEventListener('online', wake);
        document.removeEventListener('visibilitychange', onVis);
      };
    }
    this.connect();
  }

  stop(): void {
    this.stopped = true;
    this.cleanupGlobal?.();
    this.cleanupGlobal = undefined;
    clearTimeout(this.retryTimer);
    this.teardown('stopped');
  }

  /** Reconnect immediately if down; verify liveness if up. */
  wake(): void {
    if (this.stopped) return;
    if (this.state === 'ready') {
      this.sendPing();
      return;
    }
    if (this.state === 'down') {
      clearTimeout(this.retryTimer);
      this.attempt = 0;
      this.connect();
    }
  }

  setWatched(hosts: string[]): void {
    this.watched = [...new Set(hosts)].sort();
    if (this.state === 'ready') this.sendCtl({ t: 'watch', hosts: this.watched });
  }

  open(host: string): Promise<number> {
    if (this.state !== 'ready') return Promise.reject(new OpenFailed('relay_down'));
    const req = this.nextReq++;
    return new Promise<number>((resolve, reject) => {
      this.pendingOpens.set(req, { resolve, reject });
      this.sendCtl({ t: 'open', req, to: host });
      setTimeout(() => {
        if (this.pendingOpens.delete(req)) reject(new OpenFailed('timeout'));
      }, 15_000);
    });
  }

  registerLink(link: number, handler: LinkHandler): void {
    this.links.set(link, handler);
  }

  closeLink(link: number): void {
    if (!this.links.delete(link)) return;
    if (this.state === 'ready') this.sendCtl({ t: 'close', link });
  }

  send(link: number, payload: Uint8Array): boolean {
    if (this.state !== 'ready' || !this.ws) return false;
    this.ws.send(encodeFrame(link, payload));
    return true;
  }

  private setState(s: RelayState): void {
    if (this.state === s) return;
    this.state = s;
    this.stateChanged.emit(s);
  }

  private connect(): void {
    if (this.stopped) return;
    this.teardown('reconnect');
    this.setState('connecting');
    let ws: WebSocket;
    try {
      ws = new WebSocket(this.url);
    } catch (err) {
      this.lastError = String(err);
      this.scheduleRetry();
      return;
    }
    ws.binaryType = 'arraybuffer';
    this.ws = ws;
    ws.onmessage = (ev) => {
      if (this.ws !== ws) return;
      if (typeof ev.data === 'string') void this.onControl(ev.data);
      else this.onBinary(ev.data as ArrayBuffer);
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.teardown('relay_down');
      this.scheduleRetry();
    };
    ws.onerror = () => {
      /* onclose follows */
    };
  }

  private scheduleRetry(): void {
    if (this.stopped) return;
    clearTimeout(this.retryTimer);
    const delay = BACKOFF[Math.min(this.attempt, BACKOFF.length - 1)];
    this.attempt++;
    this.retryTimer = setTimeout(() => this.connect(), delay);
  }

  private teardown(reason: string): void {
    clearInterval(this.pingTimer);
    clearTimeout(this.pongTimer);
    const ws = this.ws;
    this.ws = undefined;
    if (ws) {
      ws.onclose = null;
      ws.onmessage = null;
      try {
        ws.close();
      } catch {
        /* ignore */
      }
    }
    const links = [...this.links.values()];
    this.links.clear();
    for (const p of this.pendingOpens.values()) p.reject(new OpenFailed(reason));
    this.pendingOpens.clear();
    this.presence.clear();
    this.setState('down');
    for (const l of links) l.onClosed(reason);
  }

  private sendCtl(msg: RelayClientMsg): void {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(msg));
  }

  private sendPing(): void {
    if (!this.ws || this.pongTimer) return;
    this.sendCtl({ t: 'ping', ts: Date.now() });
    this.pongTimer = setTimeout(() => {
      this.pongTimer = undefined;
      this.lastError = 'ping timeout';
      this.teardown('relay_down');
      this.connect();
    }, PONG_TIMEOUT);
  }

  private async onControl(text: string): Promise<void> {
    let msg: RelayServerMsg;
    try {
      msg = JSON.parse(text) as RelayServerMsg;
    } catch {
      return;
    }
    switch (msg.t) {
      case 'challenge': {
        const ws = this.ws;
        try {
          const [w, kp] = await Promise.all([loadWasm(), this.getKeypair()]);
          if (this.ws !== ws) return;
          const proof = w.relayAuthProof(kp, msg.relay_pub, msg.nonce);
          this.sendCtl({ t: 'auth', role: 'device', public: w.publicKeyOf(kp), proof, protocol: 1 });
        } catch (err) {
          this.lastError = String(err);
          ws?.close();
        }
        return;
      }
      case 'welcome':
        this.attempt = 0;
        this.lastError = undefined;
        this.sendCtl({ t: 'watch', hosts: this.watched });
        clearInterval(this.pingTimer);
        this.pingTimer = setInterval(() => this.sendPing(), PING_EVERY);
        this.setState('ready');
        return;
      case 'error':
        this.lastError = `${msg.code}: ${msg.message}`;
        return;
      case 'presence':
        this.presence.set(msg.host, msg.online);
        this.presenceChanged.emit({ host: msg.host, online: msg.online });
        return;
      case 'opened': {
        const p = this.pendingOpens.get(msg.req);
        this.pendingOpens.delete(msg.req);
        if (p) p.resolve(msg.link);
        else this.sendCtl({ t: 'close', link: msg.link });
        return;
      }
      case 'open_failed': {
        const p = this.pendingOpens.get(msg.req);
        this.pendingOpens.delete(msg.req);
        p?.reject(new OpenFailed(msg.reason));
        if (msg.reason === 'host_offline') {
          this.presence.set(msg.to, false);
          this.presenceChanged.emit({ host: msg.to, online: false });
        }
        return;
      }
      case 'closed': {
        const h = this.links.get(msg.link);
        this.links.delete(msg.link);
        h?.onClosed(msg.reason);
        return;
      }
      case 'pong':
        clearTimeout(this.pongTimer);
        this.pongTimer = undefined;
        return;
      case 'incoming':
        return;
    }
  }

  private onBinary(buf: ArrayBuffer): void {
    let frame;
    try {
      frame = decodeFrame(buf);
    } catch {
      return;
    }
    this.links.get(frame.link)?.onData(frame.payload);
  }
}
