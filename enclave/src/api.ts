import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';

/**
 * How this Enclave runs (ADR-0031): everything here ('single'), everything
 * here plus the API for the office ('server'), or only this window, with
 * the commands done by the server ('client').
 */
export interface AppMode {
  mode:        'single' | 'server' | 'client';
  /** The server's address, on a client. */
  server:      string | null;
  /** The certificate fingerprint clients pin, on the server. */
  fingerprint: string | null;
  port:        number | null;
}

// Asked once: the mode is read at start and does not change while the app runs.
const modePromise: Promise<AppMode> = invoke<AppMode>('cmd_app_mode');

/**
 * Run a core command. The same call either way: here it is `invoke`; on a
 * client the core forwards it to the server (`office/client.rs`), and
 * answers stream back as the same `llm-token:<id>` events.
 */
export async function call<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const { mode } = await modePromise;
  return mode === 'client'
    ? invoke<T>('cmd_remote_call', { cmd, args: args ?? {} })
    : invoke<T>(cmd, args);
}

export function useAppMode(): AppMode | null {
  const [mode, setMode] = useState<AppMode | null>(null);
  useEffect(() => {
    modePromise.then(setMode).catch(console.error);
  }, []);
  return mode;
}
