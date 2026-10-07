import { useEffect, useState } from 'react';
import { ArrowUp, Check, Folder, X } from 'lucide-react';
import { breadcrumbs, parentPath } from '../../lib/paths';
import { getConn } from '../../net/provider';
import { call } from '../../net/types';
import type { DirListing } from '../../proto/generated/DirListing';
import { Button, IconButton, Spinner } from '../ui/primitives';

/** Inline folder browser (folders only) used by the new-session dialog. */
export function FolderPicker({ host, start, sep, onPick, onClose }: { host: string; start?: string; sep: string; onPick: (path: string) => void; onClose: () => void }) {
  const [listing, setListing] = useState<DirListing>();
  const [error, setError] = useState<string>();
  const [loading, setLoading] = useState(false);

  const load = async (path?: string) => {
    const conn = getConn(host);
    if (!conn) return;
    setLoading(true);
    setError(undefined);
    try {
      const target = path || (await call(conn, { op: 'fs_home' }, 'path')).path;
      const res = await call(conn, { op: 'fs_list', path: target, hidden: false }, 'dir');
      setListing(res.listing);
    } catch (err) {
      setError((err as Error).message);
      if (path) {
        const res = await call(conn, { op: 'fs_home' }, 'path').catch(() => undefined);
        if (res && res.path !== path) void load(res.path);
      }
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void load(start);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [host]);

  const dirs = listing?.entries.filter((e) => e.kind === 'dir') ?? [];
  const up = listing ? (listing.parent ?? parentPath(listing.path, sep)) : null;

  return (
    <div className="flex flex-col overflow-hidden rounded-lg border border-line bg-bg">
      <div className="flex h-10 items-center gap-1 border-b border-line pr-1 pl-1">
        <IconButton label="上一级" size="sm" disabled={!up || loading} onClick={() => up && void load(up)}>
          <ArrowUp size={15} />
        </IconButton>
        <div className="scroll-thin flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto text-[12.5px] whitespace-nowrap">
          {listing &&
            breadcrumbs(listing.path, sep).map((c, i, arr) => (
              <span key={c.path} className="flex items-center">
                <button type="button" onClick={() => void load(c.path)} className={`rounded px-1 py-0.5 hover:bg-hover ${i === arr.length - 1 ? 'font-medium text-fg' : 'text-muted'}`}>
                  {c.name}
                </button>
                {i < arr.length - 1 && c.name !== '/' && <span className="text-faint">{sep}</span>}
              </span>
            ))}
        </div>
        {loading && <Spinner size={13} className="mr-1 text-muted" />}
        <IconButton label="关闭" size="sm" onClick={onClose}>
          <X size={15} />
        </IconButton>
      </div>
      <div className="scroll-thin h-56 overflow-y-auto p-1">
        {error && <div className="p-2 text-[13px] text-danger">{error}</div>}
        {!error && listing && !dirs.length && <div className="p-3 text-center text-[13px] text-faint">没有子文件夹</div>}
        {dirs.map((d) => (
          <button key={d.path} type="button" onClick={() => void load(d.path)} className="flex h-8 w-full items-center gap-2 rounded-md px-2 text-left text-[13.5px] hover:bg-hover max-md:h-11">
            <Folder size={15} className="shrink-0 text-muted" />
            <span className="truncate">{d.name}</span>
          </button>
        ))}
      </div>
      <div className="flex items-center justify-end gap-2 border-t border-line p-2">
        <Button size="sm" variant="primary" disabled={!listing} icon={<Check size={14} />} onClick={() => listing && onPick(listing.path)}>
          选择此文件夹
        </Button>
      </div>
    </div>
  );
}
