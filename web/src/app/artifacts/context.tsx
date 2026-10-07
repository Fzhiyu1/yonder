import { createContext, useContext } from 'react';
import type { Artifact } from '../../lib/artifacts';

/** What chat items need to show and open artifacts. */
export interface ArtifactCtx {
  host: string;
  /** Viewer key (chatKey). */
  chat: string;
  cwd?: string;
  home?: string;
  /** The device may read files on this host. */
  files: boolean;
  open(a: Artifact): void;
}

export const ArtifactContext = createContext<ArtifactCtx | null>(null);

export const useArtifactCtx = () => useContext(ArtifactContext);
