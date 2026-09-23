import { createContext, useContext, useState, useCallback, useEffect, ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';

// ─── Types ────────────────────────────────────────────────────────────────────

export interface User {
  id:       string;
  username: string;
  is_admin: boolean;
}

interface AuthContextValue {
  user:   User | null;
  /** False until the core has answered who is signed in. */
  ready:  boolean;
  login:  (userId: string, pin: string) => Promise<boolean>;
  logout: () => Promise<void>;
}

// ─── Context ──────────────────────────────────────────────────────────────────

const AuthContext = createContext<AuthContextValue | null>(null);

/**
 * The session lives in the Rust core (`session.rs`); commands take the
 * caller's identity from there, not from arguments. This context only
 * mirrors it: on startup it asks the core, and it never keeps a copy of its
 * own that could disagree (the old sessionStorage copy could outlive the
 * core's session, or vice versa).
 */
export function AuthProvider({ children }: { children: ReactNode }) {
  const [user,  setUser]  = useState<User | null>(null);
  const [ready, setReady] = useState(false);

  useEffect(() => {
    invoke<User | null>('cmd_current_session')
      .then(setUser)
      .catch(console.error)
      .finally(() => setReady(true));
  }, []);

  const login = useCallback(async (userId: string, pin: string): Promise<boolean> => {
    type LoginResult = { ok: boolean; user_id: string | null; username: string | null; is_admin: boolean | null };
    const res = await invoke<LoginResult>('cmd_login', { args: { user_id: userId, pin } });
    if (res.ok && res.user_id && res.username) {
      setUser({ id: res.user_id, username: res.username, is_admin: res.is_admin ?? false });
      return true;
    }
    setUser(null);
    return false;
  }, []);

  const logout = useCallback(async () => {
    await invoke('cmd_logout').catch(console.error);
    setUser(null);
  }, []);

  return (
    <AuthContext.Provider value={{ user, ready, login, logout }}>
      {children}
    </AuthContext.Provider>
  );
}

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error('useAuth must be used within AuthProvider');
  return ctx;
}
