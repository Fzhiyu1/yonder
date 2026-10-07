import type { ConnectionProvider } from './manager';
import { RelayManager } from './manager';
import { isMockMode, MockProvider } from './mock';
import type { HostConnection } from './types';

let provider: ConnectionProvider | null = null;

export function getProvider(): ConnectionProvider {
  if (!provider) provider = isMockMode() ? new MockProvider() : new RelayManager();
  return provider;
}

export function getConn(host: string): HostConnection | undefined {
  return getProvider().get(host);
}
