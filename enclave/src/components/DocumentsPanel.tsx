import { useCallback, useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';

interface DocInfo {
  id:       string;
  filename: string;
  status:   'pending' | 'ready' | 'failed';
}

interface ColumnOutline {
  name: string;
  type: 'number' | 'date' | 'text';
}

interface TableOutline {
  document_id: string;
  sheet:       string;
  row_count:   number;
  columns:     ColumnOutline[];
}

const TYPE_MARK: Record<ColumnOutline['type'], { mark: string; title: string }> = {
  number: { mark: '#',  title: 'number' },
  date:   { mark: '◷',  title: 'date' },
  text:   { mark: 'Aa', title: 'text' },
};

function fileIcon(name: string): string {
  const ext = name.split('.').pop()?.toLowerCase() ?? '';
  if (['xlsx', 'xlsm', 'xlsb', 'xls', 'ods'].includes(ext)) return '📊';
  if (ext === 'pdf') return '📕';
  return '📄';
}

/**
 * The user's documents beside the chat (their departments', under RLS), and
 * for spreadsheets the columns of each sheet. Clicking a column puts its
 * name into the question: asked in the table's own words, a calculation
 * needs no clarification (ADR-0023). Names and types only — no cell values.
 */
export function DocumentsPanel({ onInsert }: { onInsert: (text: string) => void }) {
  const [docs,     setDocs]     = useState<DocInfo[]>([]);
  const [tables,   setTables]   = useState<TableOutline[]>([]);
  const [filter,   setFilter]   = useState('');
  const [open,     setOpen]     = useState<Record<string, boolean>>({});
  const [loading,  setLoading]  = useState(false);
  const [error,    setError]    = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [d, t] = await Promise.all([
        invoke<DocInfo[]>('cmd_list_documents'),
        invoke<TableOutline[]>('cmd_list_document_tables'),
      ]);
      setDocs(d);
      setTables(t);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { load(); }, [load]);

  const sheetsOf = useMemo(() => {
    const by: Record<string, TableOutline[]> = {};
    for (const t of tables) (by[t.document_id] ??= []).push(t);
    return by;
  }, [tables]);

  const shown = useMemo(() => {
    const f = filter.trim().toLowerCase();
    return f ? docs.filter(d => d.filename.toLowerCase().includes(f)) : docs;
  }, [docs, filter]);

  const toggle = (id: string) => setOpen(prev => ({ ...prev, [id]: !prev[id] }));

  return (
    <aside className="docs-panel">
      <div className="docs-panel-header">
        <span>Documents</span>
        <button type="button" className="docs-panel-refresh" onClick={load} disabled={loading} title="Refresh">
          ↻
        </button>
      </div>
      <input
        className="input docs-panel-filter"
        placeholder="Filter by name…"
        value={filter}
        onChange={e => setFilter(e.target.value)}
      />
      {error && <div className="docs-panel-empty">{error}</div>}
      {!error && shown.length === 0 && (
        <div className="docs-panel-empty">{docs.length === 0 ? 'No documents yet.' : 'Nothing matches.'}</div>
      )}
      <ul className="docs-panel-list">
        {shown.map(d => {
          const sheets = sheetsOf[d.id] ?? [];
          const expandable = sheets.length > 0;
          const isOpen = !!open[d.id];
          return (
            <li key={d.id}>
              <button
                type="button"
                className={`docs-panel-doc${expandable ? ' expandable' : ''}`}
                onClick={() => expandable && toggle(d.id)}
                title={d.filename}
              >
                <span className="docs-panel-chevron">{expandable ? (isOpen ? '▾' : '▸') : ''}</span>
                <span>{fileIcon(d.filename)}</span>
                <span className="docs-panel-name">{d.filename}</span>
                {d.status !== 'ready' && <span className={`docs-panel-status ${d.status}`}>{d.status}</span>}
              </button>
              {expandable && isOpen && (
                <div className="docs-panel-sheets">
                  {sheets.map(s => (
                    <div key={s.sheet}>
                      {sheets.length > 1 && <div className="docs-panel-sheet">{s.sheet}</div>}
                      <div className="docs-panel-rows">{s.row_count.toLocaleString('ru-RU')} rows</div>
                      {s.columns.map(c => (
                        <button
                          key={c.name}
                          type="button"
                          className="docs-panel-column"
                          onClick={() => onInsert(`«${c.name}»`)}
                          title={`Insert «${c.name}» into the question`}
                        >
                          <span className="docs-panel-type" title={TYPE_MARK[c.type].title}>{TYPE_MARK[c.type].mark}</span>
                          <span className="docs-panel-name">{c.name}</span>
                        </button>
                      ))}
                    </div>
                  ))}
                </div>
              )}
            </li>
          );
        })}
      </ul>
    </aside>
  );
}
