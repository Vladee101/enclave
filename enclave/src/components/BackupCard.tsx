import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { open, save } from '@tauri-apps/plugin-dialog';
import { useI18n, type TFunc } from '../i18n';
import { Button } from './Button';
import { ErrorText } from './ErrorText';

interface BackupSummary {
  created_at:  string;
  app_version: string;
  documents:   number;
  users:       number;
  departments: number;
  files:       number;
  missing:     number;
  bytes:       number;
}
interface Staged   { backup: BackupSummary; staged_at: string; staged_by: string; }
interface Progress { stage: 'database' | 'files' | 'checking'; done: number; total: number; }

const EXTENSION = 'enclave-backup';

function stageText(t: TFunc, p: Progress | null): string {
  if (!p) return t('backup.stageStart');
  switch (p.stage) {
    case 'database': return t('backup.stageDatabase');
    case 'files':    return t('backup.stageFiles',    { done: p.done, total: p.total });
    case 'checking': return t('backup.stageChecking', { done: p.done, total: p.total });
  }
}

/**
 * Backup and restore (ADR-0026): the database and every document's file in
 * one archive. A restore is checked and prepared while the app runs and
 * replaces the data on the next start.
 */
export function BackupCard() {
  const { t, tError, tPlural, formatDateTime, formatSize } = useI18n();
  const mb = (bytes: number) => formatSize(bytes, 'MB');
  const [busy,     setBusy]     = useState<'backup' | 'restore' | null>(null);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [made,     setMade]     = useState<BackupSummary | null>(null);
  const [staged,   setStaged]   = useState<Staged | null>(null);
  const [error,    setError]    = useState<string | null>(null);

  useEffect(() => {
    invoke<Staged | null>('cmd_backup_staged').then(setStaged).catch(e => setError(tError(e)));
    const unlisten = listen<Progress>('backup-progress', e => setProgress(e.payload));
    return () => { unlisten.then(f => f()); };
  }, []);

  async function makeBackup() {
    const date = new Date().toISOString().slice(0, 10);
    const path = await save({
      defaultPath: `enclave-${date}.${EXTENSION}`,
      filters: [{ name: t('backup.filterName'), extensions: [EXTENSION] }],
    });
    if (!path) return;
    setError(null); setMade(null); setProgress(null); setBusy('backup');
    try {
      setMade(await invoke<BackupSummary>('cmd_backup_create', { path }));
    } catch (e) {
      setError(tError(e));
    } finally {
      setBusy(null);
    }
  }

  async function restore() {
    const path = await open({
      multiple: false,
      directory: false,
      filters: [{ name: t('backup.filterName'), extensions: [EXTENSION] }],
    });
    if (!path) return;
    setError(null); setMade(null);
    let b: BackupSummary;
    try {
      b = await invoke<BackupSummary>('cmd_backup_inspect', { path });
    } catch (e) {
      setError(tError(e));
      return;
    }
    if (!window.confirm(
      t('backup.confirmReplace', { date: formatDateTime(b.created_at) }) + '\n\n' +
      t('backup.confirmContents', {
        documents:   tPlural('backup.nDocuments', b.documents),
        users:       tPlural('backup.nProfiles', b.users),
        departments: tPlural('backup.nDepartments', b.departments),
        size:        mb(b.bytes),
      }) + '\n\n' +
      t('backup.confirmWarning'),
    )) return;
    setProgress(null); setBusy('restore');
    try {
      setStaged(await invoke<Staged>('cmd_backup_restore', { path }));
    } catch (e) {
      setError(tError(e));
    } finally {
      setBusy(null);
    }
  }

  async function cancelRestore() {
    setError(null);
    try {
      await invoke('cmd_backup_cancel_restore');
      setStaged(null);
    } catch (e) {
      setError(tError(e));
    }
  }

  return (
    <div className="card">
      <div style={{ marginBottom: 16 }}>
        <div style={{ fontWeight: 600, fontSize: 15 }}>{t('backup.title')}</div>
        <div className="text-sm text-muted" style={{ marginTop: 2 }}>
          {t('backup.description')}
        </div>
      </div>

      {error && <ErrorText>{error}</ErrorText>}

      {staged ? (
        <div className="backup-staged">
          <div>
            <b>{t('backup.stagedReady')}</b>{' '}
            {t('backup.stagedDesc', {
              date:       formatDateTime(staged.backup.created_at),
              documents:  tPlural('backup.nDocuments', staged.backup.documents),
              by:         staged.staged_by,
            })}
          </div>
          <div className="flex gap-3" style={{ marginTop: 10 }}>
            <Button onClick={() => invoke('cmd_restart_app').catch(e => setError(tError(e)))}>{t('common.restartNow')}</Button>
            <Button variant="ghost" onClick={cancelRestore}>{t('backup.keepCurrent')}</Button>
          </div>
        </div>
      ) : (
        <div className="flex gap-3 items-center">
          <Button onClick={makeBackup} loading={busy === 'backup'} disabled={busy !== null} spinnerSize={14}>
            {t('backup.save')}
          </Button>
          <Button variant="ghost" onClick={restore} loading={busy === 'restore'} disabled={busy !== null} spinnerSize={14}>
            {t('backup.restore')}
          </Button>
          {busy && <span className="text-sm text-muted">{stageText(t, progress)}</span>}
        </div>
      )}

      {made && (
        <div className="text-sm" style={{ marginTop: 12 }}>
          {t('backup.savedSummary', {
            documents: tPlural('backup.nDocuments', made.documents),
            files:     tPlural('backup.nFiles', made.files),
            size:      mb(made.bytes),
          })}
          {made.missing > 0 && (
            <span className="backup-warning">
              {' '}{tPlural('backup.missingWarning', made.missing)}
            </span>
          )}
        </div>
      )}
    </div>
  );
}
