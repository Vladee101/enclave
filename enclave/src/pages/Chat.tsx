import React, { useState, useRef, useCallback, useEffect } from 'react';
import { useAuth } from '../contexts/AuthContext';
import { useLlmStream, type ChosenPlan, type Clarification } from '../hooks/useLlmStream';
import { Spinner } from '../components/Spinner';
import { DocumentsPanel, type DocInfo } from '../components/DocumentsPanel';

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
}

type ScopeDoc = Pick<DocInfo, 'id' | 'filename'>;

export function ChatPage() {
  const { user } = useAuth();
  const { partial, sources, calculation, clarification, streaming, ask } = useLlmStream();
  const [messages, setMessages]   = useState<Message[]>([]);
  const [input,    setInput]      = useState('');
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
      await ask(q, 5, plan, limitTo.map(d => d.id));
    } catch (e) {
      setMessages(prev => prev.map(m => (
        m.id === botId ? { ...m, content: `⚠️ Error: ${String(e)}` } : m
      )));
    } finally {
      streamingId.current = null;
      setTimeout(scrollBottom, 50);
    }
  }, [streaming, user, ask]);

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
    {panelOpen && (
      <DocumentsPanel
        onInsert={insertAtCursor}
        selected={scope.map(d => d.id)}
        onToggleSelect={toggleScope}
        onLoaded={pruneScope}
      />
    )}
    <button
      type="button"
      className="docs-panel-toggle"
      onClick={togglePanel}
      title={panelOpen ? 'Hide documents' : 'Show documents'}
      aria-label={panelOpen ? 'Hide documents' : 'Show documents'}
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
            <div style={{ fontSize: 18, fontWeight: 600 }}>Ask anything</div>
            <div style={{ fontSize: 14, color: 'var(--text-secondary)' }}>
              Your queries and documents never leave this machine.
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
                  <span style={{ color: 'var(--text-secondary)', fontSize: 13 }}>Thinking…</span>
                </div>
              ) : (
                <div className="message-bubble">{msg.content}</div>
              )}
              {msg.role === 'user' && msg.scope && msg.scope.length > 0 && (
                <div className="message-scope">in {msg.scope.map(d => d.filename).join(', ')}</div>
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
              {msg.calculation && (
                <details className="message-calculation">
                  <summary>How this was calculated</summary>
                  <pre>{msg.calculation}</pre>
                </details>
              )}
              {msg.sources && msg.sources.length > 0 && (
                <div className="message-sources">
                  {msg.sources.map((s, i) => (
                    <span key={i} className="source-chip" title={s.excerpt}>
                      📄 {s.filename}
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
          <span className="chat-scope-label">Ask only in:</span>
          {scope.map(d => (
            <span key={d.id} className="chat-scope-chip">
              {d.filename}
              <button type="button" onClick={() => setScope(prev => prev.filter(s => s.id !== d.id))} aria-label={`Remove ${d.filename}`}>×</button>
            </span>
          ))}
          <button type="button" className="chat-scope-clear" onClick={() => setScope([])}>clear</button>
        </div>
      )}
      <div className="chat-input-bar">
        <textarea
          ref={inputRef}
          id="chat-input"
          className="chat-textarea"
          placeholder="Ask a question about your documents… (Enter to send, Shift+Enter for newline)"
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
          aria-label="Send message"
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
