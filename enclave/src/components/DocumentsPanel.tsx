import type React from 'react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { call } from '../api';
import { useI18n, type TStringKey } from '../i18n';

export interface DocInfo {
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

const TYPE_MARK: Record<ColumnOutline['type'], string> = {
  number: '#',
  date:   '◷',
  text:   'Aa',
};

const TYPE_TITLE: Record<ColumnOutline['type'], TStringKey> = {
  number: 'docsPanel.typeNumber',
  date:   'docsPanel.typeDate',
  text:   'docsPanel.typeText',
};

function fileIcon(name: string): string {
  const ext = name.split('.').pop()?.toLowerCase() ?? '';
  if (['xlsx', 'xlsm', 'xlsb', 'xls', 'ods'].includes(ext)) return '📊';
  if (ext === 'pdf') return '📕';
  return '📄';
}

interface Props {
  /** Put text into the question at the cursor (a clicked column name). */
  onInsert: (text: string) => void;
  /** Ids of the documents the question is limited to. */
  selected: string[];
  /** Click on a document: add it to, or take it out of, the selection. */
  onToggleSelect: (doc: DocInfo) => void;
  /** The list as loaded, so that deleted documents leave the selection. */
  onLoaded?: (docs: DocInfo[]) => void;
  /** Tabs shown at the top of the side panel (chats | documents). */
  tabs?: React.ReactNode;
}

/**
 * The user's documents beside the chat (their departments', under RLS), and
 * for spreadsheets the columns of each sheet. Clicking a document limits the
 * next question to it (and any others chosen); clicking a column puts its
 * name into the question — asked in the table's own words, a calculation
 * needs no clarification (ADR-0023). Names and types only — no cell values.
 */
export function DocumentsPanel({ onInsert, selected, onToggleSelect, onLoaded, tabs }: Props) {
  const { t, tError, tPlural, formatNumber } = useI18n();
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
        call<DocInfo[]>('cmd_list_documents'),
        call<TableOutline[]>('cmd_list_document_tables'),
      ]);
      setDocs(d);
      setTables(t);
      onLoaded?.(d);
    } catch (e) {
      setError(tError(e));
    } finally {
      setLoading(false);
    }
  }, [onLoaded]);

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
      {tabs}
      <div className="docs-panel-header">
        <span>{t('docsPanel.title')}</span>
        <button type="button" className="docs-panel-refresh" onClick={load} disabled={loading} title={t('docsPanel.refresh')}>
          ↻
        </button>
      </div>
      <input
        className="input docs-panel-filter"
        placeholder={t('docsPanel.filterPlaceholder')}
        value={filter}
        onChange={e => setFilter(e.target.value)}
      />
      {error && <div className="docs-panel-empty">{error}</div>}
      {!error && shown.length === 0 && (
        <div className="docs-panel-empty">{docs.length === 0 ? t('docsPanel.noDocuments') : t('docsPanel.nothingMatches')}</div>
      )}
      <ul className="docs-panel-list">
        {shown.map(d => {
          const sheets = sheetsOf[d.id] ?? [];
          const expandable = sheets.length > 0;
          const isOpen = !!open[d.id];
          const isSelected = selected.includes(d.id);
          const selectable = d.status === 'ready';
          return (
            <li key={d.id}>
              <div className={`docs-panel-doc${isSelected ? ' selected' : ''}`}>
                <button
                  type="button"
                  className="docs-panel-chevron"
                  onClick={() => expandable && toggle(d.id)}
                  disabled={!expandable}
                  aria-label={isOpen ? t('docsPanel.hideColumns') : t('docsPanel.showColumns')}
                  title={expandable ? (isOpen ? t('docsPanel.hideColumns') : t('docsPanel.showColumns')) : undefined}
                >
                  {expandable ? (isOpen ? '▾' : '▸') : ''}
                </button>
                <button
                  type="button"
                  className="docs-panel-pick"
                  onClick={() => selectable && onToggleSelect(d)}
                  disabled={!selectable}
                  title={selectable
                    ? (isSelected
                        ? t('docsPanel.stopLimiting', { name: d.filename })
                        : t('docsPanel.askOnlyIn', { name: d.filename }))
                    : d.filename}
                >
                  <span>{fileIcon(d.filename)}</span>
                  <span className="docs-panel-name">{d.filename}</span>
                  {isSelected && <span className="docs-panel-check">✓</span>}
                  {d.status !== 'ready' && (
                    <span className={`docs-panel-status ${d.status}`}>
                      {t(d.status === 'failed' ? 'docsPanel.statusFailed' : 'docsPanel.statusPending')}
                    </span>
                  )}
                </button>
              </div>
              {expandable && isOpen && (
                <div className="docs-panel-sheets">
                  {sheets.map(s => (
                    <div key={s.sheet}>
                      {sheets.length > 1 && <div className="docs-panel-sheet">{s.sheet}</div>}
                      <div className="docs-panel-rows">
                        {tPlural('docsPanel.nRows', s.row_count, { count: formatNumber(s.row_count) })}
                      </div>
                      {s.columns.map(c => (
                        <button
                          key={c.name}
                          type="button"
                          className="docs-panel-column"
                          onClick={() => onInsert(`«${c.name}»`)}
                          title={t('docsPanel.insertColumn', { name: c.name })}
                        >
                          <span className="docs-panel-type" title={t(TYPE_TITLE[c.type])}>{TYPE_MARK[c.type]}</span>
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
