import React, { useState, useEffect } from 'react';
import { call, useAppMode } from '../api';
import { useAuth } from '../contexts/AuthContext';
import { useI18n, LangToggle } from '../i18n';
import { Button } from '../components/Button';
import { FormField } from '../components/FormField';
import { ErrorText } from '../components/ErrorText';
import { OfficeConnect } from '../components/OfficeConnect';
import { invoke } from '@tauri-apps/api/core';

interface UserInfo {
  id:       string;
  username: string;
}

export function LoginPage() {
  const { login } = useAuth();
  const { t, tError } = useI18n();
  // On an office client profiles are made by an administrator (ADR-0031).
  const mode = useAppMode();
  const client = mode?.mode === 'client';
  const [connecting,   setConnecting]   = useState(false);
  const [disconnected, setDisconnected] = useState(false);

  async function disconnect() {
    if (!window.confirm(t('office.disconnectConfirm', { server: mode?.server ?? '' }))) return;
    try {
      await invoke('cmd_office_set_mode', { target: 'single' });
      setDisconnected(true);
    } catch (err) {
      setError(tError(err));
    }
  }
  const [users,       setUsers]       = useState<UserInfo[]>([]);
  const [selectedId,  setSelectedId]  = useState<string>('');
  const [pin,         setPin]         = useState('');
  const [error,       setError]       = useState('');
  const [loading,     setLoading]     = useState(false);
  const [showCreate,  setShowCreate]  = useState(false);
  const [newUsername, setNewUsername] = useState('');
  const [newPin,      setNewPin]      = useState('');

  useEffect(() => {
    call<UserInfo[]>('cmd_list_users').then(setUsers).catch(console.error);
  }, []);

  async function handleLogin(e: React.FormEvent) {
    e.preventDefault();
    if (!selectedId) { setError(t('login.selectProfileError')); return; }
    setLoading(true); setError('');
    try {
      const ok = await login(selectedId, pin);
      if (!ok) { setError(t('login.incorrectPin')); setLoading(false); }
    } catch (err) {
      // Too many wrong PINs, or the office server out of reach.
      setError(tError(err));
      setLoading(false);
    }
  }

  async function handleCreate(e: React.FormEvent) {
    e.preventDefault();
    setLoading(true); setError('');
    try {
      await call('cmd_create_user', { args: { username: newUsername, pin: newPin } });
      const updated = await call<UserInfo[]>('cmd_list_users');
      setUsers(updated);
      setShowCreate(false);
      setNewUsername(''); setNewPin('');
    } catch (err: any) {
      setError(tError(err));
    } finally { setLoading(false); }
  }

  return (
    <div className="login-page">
      <div className="login-card" style={{ position: 'relative' }}>
        <LangToggle className="lang-toggle-floating" />
        <div className="login-logo">
          <div className="login-logo-icon">🔒</div>
          <div className="login-title">Enclave</div>
          <div className="login-subtitle">{t('login.subtitle')}</div>
        </div>

        {disconnected ? (
          <div>
            <p className="text-sm" style={{ marginBottom: 14 }}>{t('office.restartToApply')}</p>
            {error && <ErrorText>{error}</ErrorText>}
            <Button
              type="button"
              fullWidth
              style={{ justifyContent: 'center', padding: 11 }}
              onClick={() => invoke('cmd_restart_app').catch(e => setError(tError(e)))}
            >
              {t('common.restartNow')}
            </Button>
          </div>
        ) : connecting ? (
          <OfficeConnect onBack={() => setConnecting(false)} />
        ) : !showCreate ? (
          <form onSubmit={handleLogin}>
            <div style={{ marginBottom: 14 }}>
              <div className="form-label" style={{ marginBottom: 8 }}>{t('login.selectProfile')}</div>
              <div className="user-list">
                {users.length === 0 && (
                  <div className="text-sm text-muted" style={{ padding: '8px 0' }}>
                    {client ? t('login.noProfilesClient') : t('login.noProfiles')}
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

            <FormField label={t('login.pin')} htmlFor="pin-input">
              <input
                id="pin-input"
                type="password"
                className="input"
                placeholder={t('login.enterPin')}
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
              {t('login.signIn')}
            </Button>

            {client ? (
              <div className="text-sm text-muted" style={{ marginTop: 10, textAlign: 'center' }}>
                {t('login.profilesOnServer')}
                <div style={{ marginTop: 6 }}>
                  {t('office.serverIs', { server: mode?.server ?? '' })}{' '}
                  <button type="button" className="link-button" onClick={disconnect}>{t('office.disconnect')}</button>
                </div>
              </div>
            ) : (
              <>
                <Button
                  type="button"
                  variant="ghost"
                  fullWidth
                  style={{ justifyContent: 'center', marginTop: 8 }}
                  onClick={() => setShowCreate(true)}
                >
                  {t('login.createNewProfile')}
                </Button>
                {mode?.mode === 'single' && (
                  <Button
                    type="button"
                    variant="ghost"
                    fullWidth
                    style={{ justifyContent: 'center', marginTop: 4 }}
                    onClick={() => setConnecting(true)}
                  >
                    {t('office.connectToServer')}
                  </Button>
                )}
              </>
            )}
          </form>
        ) : (
          <form onSubmit={handleCreate}>
            <FormField label={t('login.username')} htmlFor="new-username">
              <input
                id="new-username"
                type="text"
                className="input"
                placeholder={t('login.usernameExample')}
                value={newUsername}
                onChange={e => setNewUsername(e.target.value)}
                required
              />
            </FormField>
            <FormField label={t('login.pin')} htmlFor="new-pin">
              <input
                id="new-pin"
                type="password"
                className="input"
                placeholder={t('login.choosePin')}
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
              {t('login.createProfile')}
            </Button>
            <Button
              type="button"
              variant="ghost"
              fullWidth
              style={{ justifyContent: 'center', marginTop: 8 }}
              onClick={() => setShowCreate(false)}
            >
              {t('login.back')}
            </Button>
          </form>
        )}
      </div>
    </div>
  );
}
