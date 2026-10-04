import { useCallback, useEffect, useState, type ReactNode } from 'react';
import { call } from '../api';
import { useI18n } from '../i18n';

export interface ConversationInfo {
  id:         string;
  title:      string;
  updated_at: string;
}

interface Props {
  /** Tabs shown at the top of the side panel (chats | documents). */
  tabs:      ReactNode;
  /** The open conversation, if it has been saved yet. */
  currentId: string | null;
  /** Changes whenever the list may have changed (a new exchange saved). */
  refreshKey: number;
  /** Opening, starting or deleting is not allowed while an answer streams. */
  busy:      boolean;
  onOpen:    (id: string) => void;
  onNew:     () => void;
  /** The conversation was deleted (the page starts a new one if it was open). */
  onDeleted: (id: string) => void;
}

/**
 * The user's conversations (ADR-0030), most recent first — theirs only,
 * by RLS. Open, rename in place, delete.
 */
export function ConversationsPanel({ tabs, currentId, refreshKey, busy, onOpen, onNew, onDeleted }: Props) {
  const { t, tError, formatDateTime } = useI18n();
  const [list,     setList]     = useState<ConversationInfo[]>([]);
  const [error,    setError]    = useState<string | null>(null);
  const [renaming, setRenaming] = useState<{ id: string; title: string } | null>(null);

  const load = useCallback(async () => {
    try {
      setList(await call<ConversationInfo[]>('cmd_list_conversations'));
      setError(null);
    } catch (e) {
      setError(tError(e));
    }
  }, [tError]);

  useEffect(() => { load(); }, [load, refreshKey]);

  async function rename() {
    if (!renaming) return;
    const title = renaming.title.trim();
    setRenaming(null);
    if (!title) return;
    try {
      await call('cmd_rename_conversation', { args: { conversation_id: renaming.id, title } });
    } catch (e) {
      setError(tError(e));
    }
    load();
  }

  async function remove(c: ConversationInfo) {
    if (!window.confirm(t('chat.deleteConfirm', { title: c.title }))) return;
    try {
      await call('cmd_delete_conversation', { conversationId: c.id });
      onDeleted(c.id);
    } catch (e) {
      setError(tError(e));
    }
    load();
  }

  return (
    <aside className="docs-panel">
      {tabs}
      <button type="button" className="btn btn-ghost conv-new" onClick={onNew} disabled={busy}>
        {t('chat.newChat')}
      </button>
      {error && <div className="docs-panel-empty">{error}</div>}
      {!error && list.length === 0 && <div className="docs-panel-empty">{t('chat.noConversations')}</div>}
      <ul className="docs-panel-list">
        {list.map(c => (
          <li key={c.id}>
            {renaming?.id === c.id ? (
              <input
                className="input conv-rename"
                autoFocus
                value={renaming.title}
                maxLength={80}
                onChange={e => setRenaming({ id: c.id, title: e.target.value })}
                onBlur={rename}
                onKeyDown={e => {
                  if (e.key === 'Enter') rename();
                  if (e.key === 'Escape') setRenaming(null);
                }}
                aria-label={t('chat.rename')}
              />
            ) : (
              <div className={`conv-item${c.id === currentId ? ' selected' : ''}`}>
                <button type="button" className="conv-open" onClick={() => onOpen(c.id)} disabled={busy} title={c.title}>
                  <span className="conv-title">{c.title}</span>
                  <span className="conv-date">{formatDateTime(c.updated_at)}</span>
                </button>
                <button type="button" className="conv-action" onClick={() => setRenaming({ id: c.id, title: c.title })} title={t('chat.rename')} aria-label={t('chat.rename')}>✎</button>
                <button type="button" className="conv-action" onClick={() => remove(c)} disabled={busy} title={t('common.delete')} aria-label={t('common.delete')}>×</button>
              </div>
            )}
          </li>
        ))}
      </ul>
    </aside>
  );
}
