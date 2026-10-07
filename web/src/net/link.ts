import type { AppClientMsg } from '../proto/generated/AppClientMsg';
import type { AppHostMsg } from '../proto/generated/AppHostMsg';
import type { Event } from '../proto/generated/Event';
import type { HostHello } from '../proto/generated/HostHello';
import type { Request } from '../proto/generated/Request';
import type { Response } from '../proto/generated/Response';
import type { Channel, Handshake } from '../wasm/yonder_wasm.js';
import { bytesToB64 } from '../lib/base64';
import { Emitter } from '../lib/emitter';
import { OpenFailed, type RelayConnection } from './relay';
import { defaultTimeout, RequestError, type ConnStatus, type HostConnection, type RequestOptions } from './types';
import { loadWasm } from './wasm';

interface Pending {
  resolve: (r: Response) => void;
  reject: (e: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

export interface LinkDeps {
  getKeypair: () => Promise<string>;
  getDeviceName: () => Promise<string>;
}

/** Errors after which retrying without user action is pointless. */
const FATAL = new Set(['not_paired', 'pair_token_invalid', 'revoked', 'protocol_mismatch', 'relay_mismatch']);
const RETRY = [1000, 2000, 4000, 8000, 15000];

/** One encrypted link to one host through a shared relay connection. */
export class HostLink implements HostConnection {
  status: ConnStatus = 'offline';
  error?: string;
  hostHello?: HostHello;

  private link?: number;
  private channel?: Channel;
  private handshake?: Handshake;
  private handshakeTimer?: ReturnType<typeof setTimeout>;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private waiters: Array<() => void> = [];
  private events = new Emitter<Event>();
  private statusEm = new Emitter<ConnStatus>();
  private reconnectedEm = new Emitter<void>();
  private everOnline = false;
  private connecting = false;
  private closed = false;
  private retryTimer?: ReturnType<typeof setTimeout>;
  private attempt = 0;
  private unsub: Array<() => void> = [];
  private lastFocus: AppClientMsg | null = null;

  constructor(
    readonly host: string,
    readonly relay: RelayConnection,
    private deps: LinkDeps,
    private pairToken?: string,
    private onHello?: (hello: HostHello) => void,
  ) {
    this.unsub.push(
      relay.stateChanged.on((s) => {
        if (s === 'ready') this.connect();
        else if (s === 'down') this.setStatus(this.status === 'error' ? 'error' : 'connecting');
      }),
      relay.presenceChanged.on(({ host: h, online }) => {
        if (h !== this.host) return;
        if (online && this.status !== 'online') {
          this.attempt = 0;
          this.connect();
        } else if (!online && this.status !== 'online' && this.status !== 'error') {
          this.setStatus('offline');
        }
      }),
    );
    if (relay.state === 'ready') this.connect();
    else this.setStatus('connecting');
  }

  onEvent(fn: (e: Event) => void): () => void {
    return this.events.on(fn);
  }

  onStatus(fn: (s: ConnStatus) => void): () => void {
    return this.statusEm.on(fn);
  }

  onReconnected(fn: () => void): () => void {
    return this.reconnectedEm.on(fn);
  }

  wake(): void {
    if (this.closed) return;
    this.relay.wake();
    if (this.status !== 'online') {
      this.attempt = 0;
      if (this.relay.state === 'ready') this.connect(true);
    }
  }

  close(): void {
    this.closed = true;
    clearTimeout(this.retryTimer);
    this.unsub.forEach((u) => u());
    this.dropLink('closed', false);
    this.setStatus('offline');
  }

  /** Resolves once online; rejects after the timeout or on a fatal error. */
  private waitOnline(timeoutMs: number): Promise<void> {
    if (this.status === 'online') return Promise.resolve();
    if (this.status === 'error') return Promise.reject(new RequestError('unavailable', this.error ?? 'error'));
    return new Promise((resolve, reject) => {
      const done = () => {
        clearTimeout(t);
        resolve();
      };
      const t = setTimeout(() => {
        this.waiters = this.waiters.filter((w) => w !== done);
        reject(new RequestError('unavailable', '主机未连接'));
      }, timeoutMs);
      this.waiters.push(done);
    });
  }

  async request(req: Request, opts: RequestOptions = {}): Promise<Response> {
    const timeoutMs = opts.timeoutMs ?? defaultTimeout(req);
    await this.waitOnline(Math.min(timeoutMs, 15_000));
    const id = this.nextId++;
    return new Promise<Response>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new RequestError('timeout', '请求超时'));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      if (!this.sendMsg({ t: 'req', id, req })) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(new RequestError('unavailable', '主机未连接'));
      }
    });
  }

  sendInput(session: string, data: Uint8Array): void {
    this.sendMsg({ t: 'input', session, data: bytesToB64(data) });
  }

  sendResize(session: string, cols: number, rows: number): void {
    this.sendMsg({ t: 'resize', session, cols, rows });
  }

  sendFocus(session?: string): void {
    const msg: AppClientMsg = session ? { t: 'focus', session } : { t: 'focus' };
    this.lastFocus = msg;
    this.sendMsg(msg);
  }

  private setStatus(s: ConnStatus): void {
    if (s === 'online') {
      const ws = this.waiters;
      this.waiters = [];
      ws.forEach((w) => w());
    }
    if (this.status === s) return;
    this.status = s;
    this.statusEm.emit(s);
  }

  private sendMsg(msg: AppClientMsg): boolean {
    if (!this.channel || this.link === undefined) return false;
    const frames = this.channel.encrypt(JSON.stringify(msg)) as Uint8Array[];
    for (const f of frames) {
      if (!this.relay.send(this.link, f)) return false;
    }
    return true;
  }

  private async connect(force = false): Promise<void> {
    if (this.closed || this.connecting || this.link !== undefined) return;
    if (this.status === 'error' && !force) return;
    if (this.relay.presence.get(this.host) === false && !force) {
      this.setStatus('offline');
      return;
    }
    clearTimeout(this.retryTimer);
    this.connecting = true;
    this.error = undefined;
    this.setStatus('connecting');
    try {
      const [w, kp, name] = await Promise.all([loadWasm(), this.deps.getKeypair(), this.deps.getDeviceName()]);
      const link = await this.relay.open(this.host);
      if (this.closed) {
        this.relay.closeLink(link);
        return;
      }
      this.link = link;
      this.relay.registerLink(link, {
        onData: (p) => this.onData(p),
        onClosed: (reason) => this.onLinkClosed(reason),
      });
      const hs = new w.Handshake(kp, this.host);
      this.handshake = hs;
      const hello = { protocol: 1, device_name: name, client: 'web', ...(this.pairToken ? { pair_token: this.pairToken } : {}) };
      this.relay.send(link, hs.writeHello(JSON.stringify(hello)));
      this.handshakeTimer = setTimeout(() => {
        if (!this.channel) this.dropLink('timeout', true);
      }, 15_000);
    } catch (err) {
      const reason = err instanceof OpenFailed ? err.reason : 'relay_down';
      this.error = reason;
      this.setStatus(reason === 'host_offline' ? 'offline' : 'connecting');
      this.scheduleRetry();
    } finally {
      this.connecting = false;
    }
  }

  private scheduleRetry(): void {
    if (this.closed || this.status === 'error') return;
    clearTimeout(this.retryTimer);
    const delay = RETRY[Math.min(this.attempt, RETRY.length - 1)];
    this.attempt++;
    this.retryTimer = setTimeout(() => {
      if (this.relay.state === 'ready') void this.connect();
    }, delay);
  }

  private onData(payload: Uint8Array): void {
    if (!this.channel) {
      const hs = this.handshake;
      if (!hs) return;
      clearTimeout(this.handshakeTimer);
      let hello: HostHello;
      try {
        const ch = hs.readResponse(payload);
        hs.free();
        this.handshake = undefined;
        hello = JSON.parse(ch.hostHello()) as HostHello;
        this.channel = ch;
      } catch {
        this.dropLink('handshake_failed', true);
        return;
      }
      this.hostHello = hello;
      if (!hello.ok) {
        this.error = hello.error ?? 'not_paired';
        this.dropLink(this.error, false);
        this.setStatus('error');
        this.onHello?.(hello);
        return;
      }
      this.pairToken = undefined;
      this.attempt = 0;
      this.error = undefined;
      this.onHello?.(hello);
      this.setStatus('online');
      if (this.lastFocus) this.sendMsg(this.lastFocus);
      if (this.everOnline) this.reconnectedEm.emit();
      this.everOnline = true;
      return;
    }
    let text: string | undefined;
    try {
      text = this.channel.decrypt(payload);
    } catch {
      this.dropLink('decrypt_failed', true);
      return;
    }
    if (text === undefined) return;
    let msg: AppHostMsg;
    try {
      msg = JSON.parse(text) as AppHostMsg;
    } catch {
      return;
    }
    if (msg.t === 'res') {
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      clearTimeout(p.timer);
      if (msg.ok && msg.data) p.resolve(msg.data);
      else if (msg.ok) p.resolve({ kind: 'ok' });
      else p.reject(new RequestError(msg.error?.code ?? 'internal', msg.error?.message ?? '未知错误'));
    } else {
      this.events.emit(msg.event);
    }
  }

  private onLinkClosed(reason: string): void {
    this.link = undefined;
    this.dropLink(reason, !FATAL.has(reason));
  }

  private dropLink(reason: string, retry: boolean): void {
    clearTimeout(this.handshakeTimer);
    if (this.link !== undefined) {
      this.relay.closeLink(this.link);
      this.link = undefined;
    }
    this.handshake?.free();
    this.handshake = undefined;
    this.channel?.free();
    this.channel = undefined;
    const pend = [...this.pending.values()];
    this.pending.clear();
    for (const p of pend) {
      clearTimeout(p.timer);
      p.reject(new RequestError('disconnected', '连接已断开'));
    }
    if (this.closed || this.status === 'error') return;
    if (FATAL.has(reason)) {
      this.error = reason;
      this.setStatus('error');
      return;
    }
    this.setStatus(this.relay.presence.get(this.host) === false ? 'offline' : 'connecting');
    if (retry) this.scheduleRetry();
  }
}
