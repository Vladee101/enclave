import React, { useState, useRef, useCallback, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAuth } from '../contexts/AuthContext';
import { useI18n } from '../i18n';
import { useLlmStream, type ChosenPlan, type Clarification } from '../hooks/useLlmStream';
import { Spinner } from '../components/Spinner';
import { DocumentsPanel, type DocInfo } from '../components/DocumentsPanel';
import { ConversationsPanel } from '../components/ConversationsPanel';

interface SourceRef {
  document_id: string;
  filename:    string;
  excerpt:     string;
  score:       number;
}

interface Message {
  id:      number;
  role:    'user' | 'bot';
  content: string;
  sources?: SourceRef[];
  calculation?: string | null;
  clarification?: Clarification | null;
  /** The question a bot message answers — asked again with a chosen plan. */
  question?: string;
  /** The documents a question was limited to, shown under it. */
  scope?: ScopeDoc[];
  /** From history: the answer rests on documents no longer in reach. */
  hidden?: boolean;
  /** From history: a clarification, whose options are not kept. */
  pastClarification?: boolean;
}

/** A saved message as `cmd_get_conversation` returns it (ADR-0030). */
interface StoredMessage {
  role:          'user' | 'assistant';
  content:       string | null;
  hidden:        boolean;
  sources:       SourceRef[];
  calculation:   string | null;
  clarification: boolean;
  scope:         ScopeDoc[];
}

/**
 * Sources are chunks, several often from one document: one chip per
 * document, in order of its first source, with the numbers its chunks
 * have in the answer's [Source N].
 */
interface SourceGroup {
  documentId: string;
  filename:   string;
  numbers:    number[];
  excerpts:   string[];
}

function groupSources(sources: SourceRef[]): SourceGroup[] {
  const groups: SourceGroup[] = [];
  sources.forEach((s, i) => {
    let g = groups.find(g => g.documentId === s.document_id);
    if (!g) {
      g = { documentId: s.document_id, filename: s.filename, numbers: [], excerpts: [] };
      groups.push(g);
    }
    g.numbers.push(i + 1);
    g.excerpts.push(`[${i + 1}] ${s.excerpt}`);
  });
  return groups;
}

/** 1,2,3,5 → "1–3, 5". */
function compactNumbers(ns: number[]): string {
  const parts: string[] = [];
  for (let i = 0; i < ns.length; ) {
    let j = i;
    while (j + 1 < ns.length && ns[j + 1] === ns[j] + 1) j++;
    parts.push(j > i ? `${ns[i]}–${ns[j]}` : `${ns[i]}`);
    i = j + 1;
  }
  return parts.join(', ');
}

/** The last conversation open, per user — reopened when the chat is. */
const lastKey = (userId: string) => `enclave.chat.${userId}`;

type ScopeDoc = Pick<DocInfo, 'id' | 'filename'>;

export function ChatPage() {
  const { user } = useAuth();
  const { t, tError } = useI18n();
  const { partial, sources, calculation, clarification, streaming, ask } = useLlmStream();
  const [messages, setMessages]   = useState<Message[]>([]);
  const [input,    setInput]      = useState('');
  // The open conversation (ADR-0030): null until the first exchange of a
  // new chat is saved. The list refreshes when refreshKey changes.
  const [conversationId, setConversationId] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [sideTab, setSideTab] = useState<'chats' | 'documents'>(() => {
    try { return localStorage.getItem('enclave.sideTab') === 'documents' ? 'documents' : 'chats'; } catch { return 'chats'; }
  });
  const chooseTab = (tab: 'chats' | 'documents') => {
    setSideTab(tab);
    try { localStorage.setItem('enclave.sideTab', tab); } catch { /* ignore */ }
  };

  const remember = useCallback((id: string | null) => {
    setConversationId(id);
    if (!user) return;
    try {
      if (id) localStorage.setItem(lastKey(user.id), id);
      else localStorage.removeItem(lastKey(user.id));
    } catch { /* storage unavailable: nothing is reopened */ }
  }, [user]);

  const openConversation = useCallback(async (id: string) => {
    try {
      const stored = await invoke<StoredMessage[]>('cmd_get_conversation', { conversationId: id });
      setMessages(stored.map(m => ({
        id: nextId.current++,
        role: m.role === 'user' ? 'user' : 'bot',
        content: m.hidden ? t('chat.hiddenAnswer') : (m.content ?? ''),
        sources: m.sources,
        calculation: m.calculation,
        scope: m.scope,
        hidden: m.hidden,
        pastClarification: m.role === 'assistant' && m.clarification && !m.hidden,
      })));
      remember(id);
      setTimeout(scrollBottom, 50);
    } catch {
      // Deleted elsewhere, or not this user's: start afresh.
      remember(null);
      setMessages([]);
    }
  }, [remember, t]);

  const newChat = useCallback(() => {
    remember(null);
    setMessages([]);
    inputRef.current?.focus();
  }, [remember]);

  // Reopen the last conversation when the chat page opens — after a page
  // switch or a restart, the chat is where it was left.
  useEffect(() => {
    if (!user) return;
    let last: string | null = null;
    try { last = localStorage.getItem(lastKey(user.id)); } catch { /* none */ }
    if (last) openConversation(last);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [user?.id]);
  const streamingId = useRef<number | null>(null);
  const nextId   = useRef(1);
  const bottomRef = useRef<HTMLDivElement>(null);
  const inputRef  = useRef<HTMLTextAreaElement>(null);

  // Documents the next question is limited to (clicked in the panel).
  const [scope, setScope] = useState<ScopeDoc[]>([]);
  const toggleScope = useCallback((doc: DocInfo) => setScope(prev =>
    prev.some(d => d.id === doc.id) ? prev.filter(d => d.id !== doc.id) : [...prev, { id: doc.id, filename: doc.filename }]
  ), []);
  // A document deleted or no longer ready leaves the selection.
  const pruneScope = useCallback((docs: DocInfo[]) => setScope(prev =>
    prev.filter(s => docs.some(d => d.id === s.id && d.status === 'ready'))
  ), []);

  // Panel open/closed is a per-viewer convenience; storage may be
  // unavailable, and then the panel simply starts open.
  const [panelOpen, setPanelOpen] = useState<boolean>(() => {
    try { return localStorage.getItem('enclave.docsPanel') !== 'closed'; } catch { return true; }
  });
  const togglePanel = () => setPanelOpen(prev => {
    const next = !prev;
    try { localStorage.setItem('enclave.docsPanel', next ? 'open' : 'closed'); } catch { /* ignore */ }
    return next;
  });

  // A column name clicked in the panel goes in at the cursor, with spaces
  // around it where needed, and the cursor lands after it.
  const insertAtCursor = useCallback((text: string) => {
    const el = inputRef.current;
    setInput(prev => {
      const start = el?.selectionStart ?? prev.length;
      const end   = el?.selectionEnd ?? prev.length;
      const before = prev.slice(0, start);
      const after  = prev.slice(end);
      const lead  = before && !/\s$/.test(before) ? ' ' : '';
      const trail = after && !/^\s/.test(after) ? ' ' : '';
      const next = before + lead + text + trail + after;
      const caret = (before + lead + text + trail).length;
      requestAnimationFrame(() => {
        el?.focus();
        el?.setSelectionRange(caret, caret);
      });
      return next;
    });
  }, []);

  const scrollBottom = () => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
  };

  // Reflect the in-flight stream into a placeholder bot message as tokens arrive.
  useEffect(() => {
    if (streamingId.current === null) return;
    const id = streamingId.current;
    setMessages(prev => prev.map(m => (m.id === id ? { ...m, content: partial, sources, calculation, clarification } : m)));
    scrollBottom();
  }, [partial, sources, calculation, clarification]);

  // `shown` is what appears as the user's message: the question, or the
  // label of the option picked in a clarification.
  const run = useCallback(async (q: string, shown: string, plan?: ChosenPlan, limitTo: ScopeDoc[] = []) => {
    if (streaming || !user) return;

    const userMsg: Message = { id: nextId.current++, role: 'user', content: shown, scope: limitTo };
    const botId = nextId.current++;
    streamingId.current = botId;
    setMessages(prev => [...prev, userMsg, { id: botId, role: 'bot', content: '', question: q, scope: limitTo }]);
    setTimeout(scrollBottom, 50);

    try {
      const result = await ask(q, 5, plan, limitTo.map(d => d.id), conversationId, shown);
      if (result?.conversation_id) {
        remember(result.conversation_id);
        setRefreshKey(k => k + 1);
      }
    } catch (e) {
      setMessages(prev => prev.map(m => (
        m.id === botId ? { ...m, content: t('chat.error', { error: tError(e) }) } : m
      )));
    } finally {
      streamingId.current = null;
      setTimeout(scrollBottom, 50);
    }
  }, [streaming, user, ask, t, tError, conversationId, remember]);

  const sendMessage = useCallback(() => {
    const q = input.trim();
    if (!q) return;
    setInput('');
    run(q, q, undefined, scope);
  }, [input, run, scope]);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendMessage();
    }
  };

  return (
    <div className="chat-page">
    {panelOpen && (() => {
      const tabs = (
        <div className="side-tabs" role="tablist">
          <button type="button" role="tab" aria-selected={sideTab === 'chats'} className={sideTab === 'chats' ? 'on' : ''} onClick={() => chooseTab('chats')}>
            {t('chat.tabChats')}
          </button>
          <button type="button" role="tab" aria-selected={sideTab === 'documents'} className={sideTab === 'documents' ? 'on' : ''} onClick={() => chooseTab('documents')}>
            {t('chat.tabDocuments')}
          </button>
        </div>
      );
      return sideTab === 'chats' ? (
        <ConversationsPanel
          tabs={tabs}
          currentId={conversationId}
          refreshKey={refreshKey}
          busy={streaming}
          onOpen={openConversation}
          onNew={newChat}
          onDeleted={id => { if (id === conversationId) newChat(); }}
        />
      ) : (
        <DocumentsPanel
          tabs={tabs}
          onInsert={insertAtCursor}
          selected={scope.map(d => d.id)}
          onToggleSelect={toggleScope}
          onLoaded={pruneScope}
        />
      );
    })()}
    <button
      type="button"
      className="docs-panel-toggle"
      onClick={togglePanel}
      title={panelOpen ? t('chat.hideDocuments') : t('chat.showDocuments')}
      aria-label={panelOpen ? t('chat.hideDocuments') : t('chat.showDocuments')}
    >
      {panelOpen ? '‹' : '›'}
    </button>
    <div className="chat-layout">
      {/* ── Messages ── */}
      <div className="chat-messages">
        {messages.length === 0 && (
          <div style={{
            flex: 1,
            display: 'flex',
            flexDirection: 'column',
            alignItems: 'center',
            justifyContent: 'center',
            gap: 12,
            opacity: 0.5,
            paddingTop: 60,
          }}>
            <div style={{ fontSize: 48 }}>🔒</div>
            <div style={{ fontSize: 18, fontWeight: 600 }}>{t('chat.emptyTitle')}</div>
            <div style={{ fontSize: 14, color: 'var(--text-secondary)' }}>
              {t('chat.emptySubtitle')}
            </div>
          </div>
        )}

        {messages.map(msg => (
          <div key={msg.id} className={`message ${msg.role}`}>
            <div className={`message-avatar ${msg.role === 'user' ? 'user-avatar' : 'bot-avatar'}`}>
              {msg.role === 'user' ? user?.username[0].toUpperCase() : '🔒'}
            </div>
            <div className="message-body">
              {msg.role === 'bot' && msg.content === '' && streaming ? (
                <div className="message-bubble" style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
                  <Spinner size={16} />
                  <span style={{ color: 'var(--text-secondary)', fontSize: 13 }}>{t('chat.thinking')}</span>
                </div>
              ) : (
                <div className={`message-bubble${msg.hidden ? ' message-hidden' : ''}`}>{msg.content}</div>
              )}
              {msg.role === 'user' && msg.scope && msg.scope.length > 0 && (
                <div className="message-scope">{t('chat.inScope', { files: msg.scope.map(d => d.filename).join(', ') })}</div>
              )}
              {msg.clarification && msg.question && (
                <div className="message-clarify">
                  {msg.clarification.options.map((o, i) => (
                    <button
                      key={i}
                      type="button"
                      className="clarify-option"
                      disabled={streaming}
                      onClick={() => run(msg.question!, o.label, { table_id: msg.clarification!.table_id, plan: o.plan }, msg.scope)}
                    >
                      {o.label}
                    </button>
                  ))}
                </div>
              )}
              {msg.pastClarification && (
                <div className="message-scope">{t('chat.pastClarification')}</div>
              )}
              {msg.calculation && (
                <details className="message-calculation">
                  <summary>{t('chat.howCalculated')}</summary>
                  <pre>{msg.calculation}</pre>
                </details>
              )}
              {msg.sources && msg.sources.length > 0 && (
                <div className="message-sources">
                  {groupSources(msg.sources).map(g => (
                    <span key={g.documentId} className="source-chip" title={g.excerpts.join('\n\n')}>
                      📄 {g.filename}
                      <span className="source-nums">{compactNumbers(g.numbers)}</span>
                    </span>
                  ))}
                </div>
              )}
            </div>
          </div>
        ))}

        <div ref={bottomRef} />
      </div>

      {/* ── Input bar ── */}
      {scope.length > 0 && (
        <div className="chat-scope">
          <span className="chat-scope-label">{t('chat.askOnlyIn')}</span>
          {scope.map(d => (
            <span key={d.id} className="chat-scope-chip">
              {d.filename}
              <button type="button" onClick={() => setScope(prev => prev.filter(s => s.id !== d.id))} aria-label={t('chat.removeScope', { name: d.filename })}>×</button>
            </span>
          ))}
          <button type="button" className="chat-scope-clear" onClick={() => setScope([])}>{t('chat.clear')}</button>
        </div>
      )}
      <div className="chat-input-bar">
        <textarea
          ref={inputRef}
          id="chat-input"
          className="chat-textarea"
          placeholder={t('chat.placeholder')}
          value={input}
          onChange={e => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          rows={1}
          disabled={streaming}
        />
        <button
          id="chat-send-btn"
          type="button"
          className="chat-send-btn"
          onClick={sendMessage}
          disabled={streaming || !input.trim()}
          aria-label={t('chat.send')}
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5">
            <line x1="22" y1="2" x2="11" y2="13" />
            <polygon points="22 2 15 22 11 13 2 9 22 2" />
          </svg>
        </button>
      </div>
    </div>
    </div>
  );
}
