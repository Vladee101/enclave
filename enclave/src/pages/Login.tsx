import React, { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAuth } from '../contexts/AuthContext';
import { Button } from '../components/Button';
import { FormField } from '../components/FormField';
import { ErrorText } from '../components/ErrorText';

interface UserInfo {
  id:       string;
  username: string;
}

export function LoginPage() {
  const { login } = useAuth();
  const [users,       setUsers]       = useState<UserInfo[]>([]);
  const [selectedId,  setSelectedId]  = useState<string>('');
  const [pin,         setPin]         = useState('');
  const [error,       setError]       = useState('');
  const [loading,     setLoading]     = useState(false);
  const [showCreate,  setShowCreate]  = useState(false);
  const [newUsername, setNewUsername] = useState('');
  const [newPin,      setNewPin]      = useState('');

  useEffect(() => {
    invoke<UserInfo[]>('cmd_list_users').then(setUsers).catch(console.error);
  }, []);

  async function handleLogin(e: React.FormEvent) {
    e.preventDefault();
    if (!selectedId) { setError('Select a profile.'); return; }
    setLoading(true); setError('');
    const ok = await login(selectedId, pin);
    if (!ok) { setError('Incorrect PIN.'); setLoading(false); }
  }

  async function handleCreate(e: React.FormEvent) {
    e.preventDefault();
    setLoading(true); setError('');
    try {
      await invoke('cmd_create_user', { args: { username: newUsername, pin: newPin } });
      const updated = await invoke<UserInfo[]>('cmd_list_users');
      setUsers(updated);
      setShowCreate(false);
      setNewUsername(''); setNewPin('');
    } catch (err: any) {
      setError(String(err));
    } finally { setLoading(false); }
  }

  return (
    <div className="login-page">
      <div className="login-card">
        <div className="login-logo">
          <div className="login-logo-icon">🔒</div>
          <div className="login-title">Enclave</div>
          <div className="login-subtitle">On-premises knowledge, entirely yours</div>
        </div>

        {!showCreate ? (
          <form onSubmit={handleLogin}>
            <div style={{ marginBottom: 14 }}>
              <div className="form-label" style={{ marginBottom: 8 }}>Select profile</div>
              <div className="user-list">
                {users.length === 0 && (
                  <div className="text-sm text-muted" style={{ padding: '8px 0' }}>
                    No profiles yet. Create one below.
                  </div>
                )}
                {users.map(u => (
                  <div
                    key={u.id}
                    className={`user-option${selectedId === u.id ? ' selected' : ''}`}
                    onClick={() => setSelectedId(u.id)}
                    role="button"
                    tabIndex={0}
                    onKeyDown={ev => ev.key === 'Enter' && setSelectedId(u.id)}
                  >
                    <div className="user-option-avatar">{u.username[0].toUpperCase()}</div>
                    <span style={{ fontWeight: 500, fontSize: 14 }}>{u.username}</span>
                  </div>
                ))}
              </div>
            </div>

            <FormField label="PIN" htmlFor="pin-input">
              <input
                id="pin-input"
                type="password"
                className="input"
                placeholder="Enter your PIN"
                value={pin}
                onChange={e => setPin(e.target.value)}
                autoComplete="current-password"
              />
            </FormField>

            {error && <ErrorText>{error}</ErrorText>}

            <Button
              type="submit"
              id="login-submit-btn"
              fullWidth
              loading={loading}
              style={{ justifyContent: 'center', padding: '11px', marginTop: 4 }}
            >
              Sign In
            </Button>

            <Button
              type="button"
              variant="ghost"
              fullWidth
              style={{ justifyContent: 'center', marginTop: 8 }}
              onClick={() => setShowCreate(true)}
            >
              Create new profile
            </Button>
          </form>
        ) : (
          <form onSubmit={handleCreate}>
            <FormField label="Username" htmlFor="new-username">
              <input
                id="new-username"
                type="text"
                className="input"
                placeholder="e.g. alice"
                value={newUsername}
                onChange={e => setNewUsername(e.target.value)}
                required
              />
            </FormField>
            <FormField label="PIN" htmlFor="new-pin">
              <input
                id="new-pin"
                type="password"
                className="input"
                placeholder="Choose a PIN"
                value={newPin}
                onChange={e => setNewPin(e.target.value)}
                required
              />
            </FormField>

            {error && <ErrorText>{error}</ErrorText>}

            <Button
              type="submit"
              id="create-user-btn"
              fullWidth
              loading={loading}
              style={{ justifyContent: 'center', padding: 11 }}
            >
              Create Profile
            </Button>
            <Button
              type="button"
              variant="ghost"
              fullWidth
              style={{ justifyContent: 'center', marginTop: 8 }}
              onClick={() => setShowCreate(false)}
            >
              Back
            </Button>
          </form>
        )}
      </div>
    </div>
  );
}
