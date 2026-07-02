import { createContext, useContext, useState, useCallback, ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';

// ─── Types ────────────────────────────────────────────────────────────────────

export interface User {
  id:       string;
  username: string;
  is_admin: boolean;
}

interface AuthContextValue {
  user:   User | null;
  login:  (userId: string, pin: string) => Promise<boolean>;
  logout: () => void;
}

// ─── Context ──────────────────────────────────────────────────────────────────

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [user, setUser] = useState<User | null>(() => {
    const saved = sessionStorage.getItem('enclave_user');
    return saved ? JSON.parse(saved) : null;
  });

  const login = useCallback(async (userId: string, pin: string): Promise<boolean> => {
    type LoginResult = { ok: boolean; user_id: string | null; username: string | null; is_admin: boolean | null };
    const res = await invoke<LoginResult>('cmd_login', { args: { user_id: userId, pin } });
    if (res.ok && res.user_id && res.username) {
      const u: User = { id: res.user_id, username: res.username, is_admin: res.is_admin ?? false };
      setUser(u);
      sessionStorage.setItem('enclave_user', JSON.stringify(u));
      return true;
    }
    return false;
  }, []);

  const logout = useCallback(() => {
    setUser(null);
    sessionStorage.removeItem('enclave_user');
    invoke('cmd_logout').catch(() => {});
  }, []);

  return (
    <AuthContext.Provider value={{ user, login, logout }}>
      {children}
    </AuthContext.Provider>
  );
}

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error('useAuth must be used within AuthProvider');
  return ctx;
}
