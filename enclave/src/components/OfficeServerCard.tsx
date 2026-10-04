import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useI18n } from '../i18n';
import type { AppMode } from '../api';
import { Button } from './Button';
import { ErrorText } from './ErrorText';

interface Invitation {
  code:       string;
  expires_in: number;
  port:       number;
  addresses:  string[];
}

/**
 * The office server (ADR-0031), on the Admin page of the server itself:
 * making this computer the server (from the next start), and the one-time
 * codes other computers connect with. Local commands: they are this
 * computer's, never available to a client.
 */
export function OfficeServerCard({ mode }: { mode: AppMode }) {
  const { t, tError } = useI18n();
  const [invite,   setInvite]   = useState<Invitation | null>(null);
  const [left,     setLeft]     = useState(0);
  const [switched, setSwitched] = useState(false);
  const [busy,     setBusy]     = useState(false);
  const [error,    setError]    = useState('');

  useEffect(() => {
    if (!invite) return;
    setLeft(invite.expires_in);
    const timer = setInterval(() => setLeft(s => Math.max(0, s - 1)), 1000);
    return () => clearInterval(timer);
  }, [invite]);

  async function makeInvite() {
    setBusy(true); setError('');
    try {
      setInvite(await invoke<Invitation>('cmd_office_invite'));
    } catch (e) {
      setError(tError(e));
    } finally {
      setBusy(false);
    }
  }

  async function switchTo(target: 'single' | 'server') {
    if (target === 'single' && !window.confirm(t('office.stopServerConfirm'))) return;
    setBusy(true); setError('');
    try {
      await invoke('cmd_office_set_mode', { target });
      setSwitched(true);
    } catch (e) {
      setError(tError(e));
    } finally {
      setBusy(false);
    }
  }

  const minutes = `${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')}`;

  return (
    <div className="card">
      <div style={{ fontWeight: 600, fontSize: 15 }}>{t('admin.officeServer')}</div>

      {switched ? (
        <>
          <div className="text-sm" style={{ margin: '8px 0 12px' }}>{t('office.restartToApply')}</div>
          <Button onClick={() => invoke('cmd_restart_app').catch(e => setError(tError(e)))}>
            {t('common.restartNow')}
          </Button>
        </>
      ) : mode.mode === 'single' ? (
        <>
          <div className="text-sm text-muted" style={{ margin: '2px 0 12px' }}>{t('office.becomeServerDesc')}</div>
          <Button loading={busy} onClick={() => switchTo('server')}>{t('office.becomeServer')}</Button>
        </>
      ) : (
        <>
          <div className="text-sm text-muted" style={{ margin: '2px 0 12px' }}>
            {t('admin.officeServerDesc', { port: String(mode.port ?? '') })}
          </div>

          {invite && left > 0 ? (
            <div className="office-invite">
              <div className="text-sm">{t('office.inviteHowTo')}</div>
              <div className="office-invite-row">
                <span className="text-sm text-muted">{t('office.serverAddress')}</span>
                <code className="mono">{invite.addresses.join('  ·  ') || '—'}</code>
              </div>
              <div className="office-invite-row">
                <span className="text-sm text-muted">{t('office.code')}</span>
                <code className="office-code">{invite.code}</code>
              </div>
              <div className="text-sm text-muted">{t('office.inviteExpires', { time: minutes })}</div>
            </div>
          ) : (
            invite && <div className="text-sm text-muted" style={{ marginBottom: 10 }}>{t('office.inviteExpired')}</div>
          )}

          <div className="flex gap-2" style={{ marginTop: 12 }}>
            <Button loading={busy} onClick={makeInvite}>{t('office.connectComputer')}</Button>
            <Button variant="ghost" disabled={busy} onClick={() => switchTo('single')}>{t('office.stopServer')}</Button>
          </div>
        </>
      )}

      {error && <ErrorText>{error}</ErrorText>}
    </div>
  );
}
