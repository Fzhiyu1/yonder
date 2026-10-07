import type { Event } from '../proto/generated/Event';
import type { HostHello } from '../proto/generated/HostHello';
import type { Request } from '../proto/generated/Request';
import type { Response } from '../proto/generated/Response';

export type ConnStatus = 'offline' | 'connecting' | 'online' | 'error';

export interface RequestOptions {
  timeoutMs?: number;
}

export class RequestError extends Error {
  constructor(
    public code: string,
    message: string,
  ) {
    super(message);
  }
}

/** What the UI needs from a host, real (relay + Noise) or mock. */
export interface HostConnection {
  readonly host: string;
  readonly status: ConnStatus;
  /** Machine-readable reason for `error` / `offline`. */
  readonly error?: string;
  readonly hostHello?: HostHello;
  request(req: Request, opts?: RequestOptions): Promise<Response>;
  sendInput(session: string, data: Uint8Array): void;
  sendResize(session: string, cols: number, rows: number): void;
  sendFocus(session?: string): void;
  onEvent(fn: (e: Event) => void): () => void;
  onStatus(fn: (s: ConnStatus) => void): () => void;
  /** Fired after the link came back; views re-attach with `since`. */
  onReconnected(fn: () => void): () => void;
  /** Retry now (e.g. user pressed refresh). */
  wake(): void;
  close(): void;
}

export type ResponseOf<K extends Response['kind']> = Extract<Response, { kind: K }>;

export async function call<K extends Response['kind']>(
  conn: HostConnection,
  req: Request,
  kind: K,
  opts?: RequestOptions,
): Promise<ResponseOf<K>> {
  const res = await conn.request(req, opts);
  if (res.kind !== kind) throw new RequestError('invalid', `unexpected response ${res.kind}`);
  return res as ResponseOf<K>;
}

export function defaultTimeout(req: Request): number {
  return req.op.startsWith('fs_') || req.op === 'upload_temp' ? 120_000 : 30_000;
}

export const HELLO_ERRORS: Record<string, string> = {
  not_paired: '此设备未与该主机配对',
  pair_token_invalid: '配对码无效或已过期，请在主机上重新生成二维码',
  revoked: '此设备已被主机撤销授权',
  protocol_mismatch: '协议版本不兼容，请升级客户端或主机',
  relay_mismatch: '中继地址与主机配置不一致',
  host_offline: '主机离线',
  relay_down: '无法连接中继',
  timeout: '连接超时',
  rate_limited: '请求过于频繁，稍后再试',
  too_many_links: '主机连接数已满',
};

export function describeError(code?: string): string {
  if (!code) return '';
  return HELLO_ERRORS[code] ?? code;
}
