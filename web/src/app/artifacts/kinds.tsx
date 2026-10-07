import { FileCode2, FileText, Globe, Image as ImageIcon, type LucideProps } from 'lucide-react';
import type { ArtifactKind } from '../../lib/artifacts';

export const KIND_LABEL: Record<ArtifactKind, string> = { image: '图片', pdf: 'PDF', html: 'HTML', web: '网页', text: '文本' };

export function KindIcon({ kind, ...p }: { kind: ArtifactKind } & LucideProps) {
  switch (kind) {
    case 'image':
      return <ImageIcon {...p} />;
    case 'pdf':
      return <FileText {...p} />;
    case 'html':
      return <FileCode2 {...p} />;
    case 'web':
      return <Globe {...p} />;
    default:
      return <FileText {...p} />;
  }
}

export const KIND_TONE: Record<ArtifactKind, string> = {
  image: 'text-[#0d9488]',
  pdf: 'text-danger',
  html: 'text-warn',
  web: 'text-accent',
  text: 'text-muted',
};
