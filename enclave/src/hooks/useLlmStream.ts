import { useState, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

interface SourceRef {
  document_id: string;
  filename:    string;
  excerpt:     string;
  score:       number;
}

/** One answer to a clarification: a complete plan the core re-checks. */
export interface ClarifyOption {
  label: string;
  plan:  unknown;
}

/** The core asks back before calculating (ADR-0022). */
export interface Clarification {
  question: string;
  table_id: string;
  options:  ClarifyOption[];
}

/** The user's pick, sent back with the original question. */
export interface ChosenPlan {
  table_id: string;
  plan:     unknown;
}

interface QueryResult {
  answer:  string;
  sources: SourceRef[];
  /** Set when the answer is a calculation over a spreadsheet (ADR-0022). */
  calculation: string | null;
  /** Set instead of an answer; `answer` then holds its question. */
  clarification: Clarification | null;
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
  const [calculation, setCalculation] = useState<string | null>(null);
  const [clarification, setClarification] = useState<Clarification | null>(null);
  const [streaming, setStreaming] = useState(false);
  const [error,     setError]     = useState<string | null>(null);
  const unlisten = useRef<UnlistenFn | null>(null);

  const cancel = useCallback(() => {
    unlisten.current?.();
    unlisten.current = null;
    setStreaming(false);
  }, []);

  const ask = useCallback(async (query: string, topK = 5, plan?: ChosenPlan) => {
    cancel();
    setPartial('');
    setSources([]);
    setCalculation(null);
    setClarification(null);
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
        args: { query, top_k: topK, plan: plan ?? null }, // identity comes from the core session
      });

      setSources(result.sources);
      setCalculation(result.calculation);
      setClarification(result.clarification);
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

  return { partial, sources, calculation, clarification, streaming, error, ask, cancel };
}
