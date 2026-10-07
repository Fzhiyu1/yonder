import { memo, useState, type ReactNode } from 'react';
import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { Check, Copy } from 'lucide-react';
import { copyText } from '../../lib/clipboard';
import { useArtifactCtx } from '../artifacts/context';
import { openInViewer } from '../artifacts/Inline';

export function CopyButton({ text, label = '复制', className }: { text: string; label?: string; className?: string }) {
  const [done, setDone] = useState(false);
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      onClick={async () => {
        if (await copyText(text)) {
          setDone(true);
          setTimeout(() => setDone(false), 1200);
        }
      }}
      className={className ?? 'inline-flex h-7 w-7 items-center justify-center rounded text-faint hover:bg-hover hover:text-fg max-md:h-10 max-md:w-11'}
    >
      {done ? <Check size={14} /> : <Copy size={14} />}
    </button>
  );
}

function textOf(node: ReactNode): string {
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(textOf).join('');
  if (node && typeof node === 'object' && 'props' in node) return textOf((node as { props: { children?: ReactNode } }).props.children);
  return '';
}

export function CodeBlock({ code, lang }: { code: string; lang?: string }) {
  return (
    <div className="group/code relative my-2 overflow-hidden rounded-md border border-line bg-code">
      <div className="flex h-7 items-center justify-between border-b border-line pr-1 pl-3 text-[12px] text-faint max-md:h-10">
        <span className="truncate font-mono">{lang || 'text'}</span>
        <CopyButton text={code} label="复制代码" />
      </div>
      <pre className="scroll-thin overflow-x-auto px-3 py-2.5 font-mono text-[12.5px] leading-[1.55]">
        <code>{code}</code>
      </pre>
    </div>
  );
}

/** Links to host files and host-local pages open in the viewer; the phone cannot reach them. */
function Link({ href, children }: { href?: string; children?: ReactNode }) {
  const ctx = useArtifactCtx();
  return (
    <a
      href={href}
      target="_blank"
      rel="noreferrer noopener"
      onClick={(e) => {
        if (openInViewer(href, ctx?.open)) e.preventDefault();
      }}
    >
      {children}
    </a>
  );
}

const components: Components = {
  pre({ children }) {
    const child = Array.isArray(children) ? children[0] : children;
    const cls = (child as { props?: { className?: string } })?.props?.className ?? '';
    const lang = /language-([\w+-]+)/.exec(cls)?.[1];
    return <CodeBlock code={textOf(children).replace(/\n$/, '')} lang={lang} />;
  },
  a({ href, children }) {
    return <Link href={href}>{children}</Link>;
  },
};

export const Markdown = memo(function Markdown({ text, className }: { text: string; className?: string }) {
  return (
    <div className={`md ${className ?? ''}`}>
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
        {text}
      </ReactMarkdown>
    </div>
  );
});
