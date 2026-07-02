import React, { useState, useRef, useCallback, useEffect } from 'react';
import { useAuth } from '../contexts/AuthContext';
import { useLlmStream } from '../hooks/useLlmStream';
import { Spinner } from '../components/Spinner';

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
}

export function ChatPage() {
  const { user } = useAuth();
  const { partial, sources, streaming, ask } = useLlmStream();
  const [messages, setMessages]   = useState<Message[]>([]);
  const [input,    setInput]      = useState('');
  const streamingId = useRef<number | null>(null);
  const nextId   = useRef(1);
  const bottomRef = useRef<HTMLDivElement>(null);

  const scrollBottom = () => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
  };

  // Reflect the in-flight stream into a placeholder bot message as tokens arrive.
  useEffect(() => {
    if (streamingId.current === null) return;
    const id = streamingId.current;
    setMessages(prev => prev.map(m => (m.id === id ? { ...m, content: partial, sources } : m)));
    scrollBottom();
  }, [partial, sources]);

  const sendMessage = useCallback(async () => {
    const q = input.trim();
    if (!q || streaming || !user) return;

    const userMsg: Message = { id: nextId.current++, role: 'user', content: q };
    const botId = nextId.current++;
    streamingId.current = botId;
    setMessages(prev => [...prev, userMsg, { id: botId, role: 'bot', content: '' }]);
    setInput('');
    setTimeout(scrollBottom, 50);

    try {
      await ask(user.id, q, 5);
    } catch (e) {
      setMessages(prev => prev.map(m => (
        m.id === botId ? { ...m, content: `⚠️ Error: ${String(e)}` } : m
      )));
    } finally {
      streamingId.current = null;
      setTimeout(scrollBottom, 50);
    }
  }, [input, streaming, user, ask]);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendMessage();
    }
  };

  return (
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
      <div className="chat-input-bar">
        <textarea
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
  );
}
