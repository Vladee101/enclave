import React, { useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useI18n } from '../i18n';
import { Button } from './Button';
import { FormField } from './FormField';
import { ErrorText } from './ErrorText';

/**
 * Connect this computer to the office server (ADR-0031): the server's
 * name or address and the one-time code its administrator made. The core
 * checks the server's proof of the code before trusting its certificate;
 * the computer becomes a client at the next start. Local commands, not
 * `call()`: they are about this computer.
 */
export function OfficeConnect({ onBack }: { onBack: () => void }) {
  const { t, tError } = useI18n();
  const [address,   setAddress]   = useState('');
  const [code,      setCode]      = useState('');
  const [loading,   setLoading]   = useState(false);
  const [error,     setError]     = useState('');
  const [connected, setConnected] = useState<string | null>(null);

  async function connect(e: React.FormEvent) {
    e.preventDefault();
    setLoading(true); setError('');
    try {
      setConnected(await invoke<string>('cmd_office_pair', { address, code }));
    } catch (err) {
      setError(tError(err));
    } finally {
      setLoading(false);
    }
  }

  if (connected) {
    return (
      <div>
        <p className="text-sm" style={{ marginBottom: 14 }}>{t('office.connected', { server: connected })}</p>
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
    );
  }

  return (
    <form onSubmit={connect}>
      <p className="text-sm text-muted" style={{ marginBottom: 14 }}>{t('office.connectDesc')}</p>
      <FormField label={t('office.serverAddress')} htmlFor="office-address">
        <input
          id="office-address"
          type="text"
          className="input"
          placeholder={t('office.serverAddressExample')}
          value={address}
          onChange={e => setAddress(e.target.value)}
          required
        />
      </FormField>
      <FormField label={t('office.code')} htmlFor="office-code">
        <input
          id="office-code"
          type="text"
          className="input mono"
          placeholder="ABCD-EFGH-IJKL-MNOP"
          value={code}
          onChange={e => setCode(e.target.value)}
          autoComplete="off"
          spellCheck={false}
          required
        />
      </FormField>

      {error && <ErrorText>{error}</ErrorText>}

      <Button type="submit" fullWidth loading={loading} style={{ justifyContent: 'center', padding: 11 }}>
        {t('office.connect')}
      </Button>
      <Button
        type="button"
        variant="ghost"
        fullWidth
        style={{ justifyContent: 'center', marginTop: 8 }}
        onClick={onBack}
      >
        {t('login.back')}
      </Button>
    </form>
  );
}
