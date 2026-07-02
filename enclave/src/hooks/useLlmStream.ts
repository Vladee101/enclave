import { useState, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

interface SourceRef {
  document_id: string;
  filename:    string;
  excerpt:     string;
  score:       number;
}

interface QueryResult {
  answer:  string;
  sources: SourceRef[];
}

interface StreamTokenEvent {
  token: string;
}

/**
 * Streams a RAG + LoRA completion token-by-token via `cmd_query_stream`.
 * The backend emits `llm-token:<requestId>` events as llama-server streams
 * the response (ADR-0003, 0004); this hook accumulates them into `partial`
 * and resolves `sources` once the underlying command settles.
 */
export function useLlmStream() {
  const [partial,   setPartial]   = useState('');
  const [sources,   setSources]   = useState<SourceRef[]>([]);
  const [streaming, setStreaming] = useState(false);
  const [error,     setError]     = useState<string | null>(null);
  const unlisten = useRef<UnlistenFn | null>(null);

  const cancel = useCallback(() => {
    unlisten.current?.();
    unlisten.current = null;
    setStreaming(false);
  }, []);

  const ask = useCallback(async (userId: string, query: string, topK = 5) => {
    cancel();
    setPartial('');
    setSources([]);
    setError(null);
    setStreaming(true);

    const requestId = crypto.randomUUID();

    try {
      unlisten.current = await listen<StreamTokenEvent>(
        `llm-token:${requestId}`,
        event => setPartial(prev => prev + event.payload.token),
      );

      const result = await invoke<QueryResult>('cmd_query_stream', {
        requestId,
        args: { user_id: userId, query, top_k: topK },
      });

      setSources(result.sources);
      // Reconcile to the authoritative final answer in case streamed
      // tokens and the buffered result diverge (e.g. trailing whitespace).
      setPartial(result.answer);
      return result;
    } catch (e) {
      setError(String(e));
      throw e;
    } finally {
      unlisten.current?.();
      unlisten.current = null;
      setStreaming(false);
    }
  }, [cancel]);

  return { partial, sources, streaming, error, ask, cancel };
}
