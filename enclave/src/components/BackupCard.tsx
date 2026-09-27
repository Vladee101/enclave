import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { open, save } from '@tauri-apps/plugin-dialog';
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
const mb = (bytes: number) => (bytes / 1e6).toFixed(1) + ' MB';
const when = (iso: string) => new Date(iso).toLocaleString();

function stageText(p: Progress | null): string {
  if (!p) return 'Starting…';
  switch (p.stage) {
    case 'database': return 'Database… a large one takes a few minutes';
    case 'files':    return `Document files ${p.done} / ${p.total}…`;
    case 'checking': return `Checking the backup ${p.done} / ${p.total}…`;
  }
}

/**
 * Backup and restore (ADR-0026): the database and every document's file in
 * one archive. A restore is checked and prepared while the app runs and
 * replaces the data on the next start.
 */
export function BackupCard() {
  const [busy,     setBusy]     = useState<'backup' | 'restore' | null>(null);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [made,     setMade]     = useState<BackupSummary | null>(null);
  const [staged,   setStaged]   = useState<Staged | null>(null);
  const [error,    setError]    = useState<string | null>(null);

  useEffect(() => {
    invoke<Staged | null>('cmd_backup_staged').then(setStaged).catch(e => setError(String(e)));
    const unlisten = listen<Progress>('backup-progress', e => setProgress(e.payload));
    return () => { unlisten.then(f => f()); };
  }, []);

  async function makeBackup() {
    const date = new Date().toISOString().slice(0, 10);
    const path = await save({
      defaultPath: `enclave-${date}.${EXTENSION}`,
      filters: [{ name: 'Enclave backup', extensions: [EXTENSION] }],
    });
    if (!path) return;
    setError(null); setMade(null); setProgress(null); setBusy('backup');
    try {
      setMade(await invoke<BackupSummary>('cmd_backup_create', { path }));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  }

  async function restore() {
    const path = await open({
      multiple: false,
      directory: false,
      filters: [{ name: 'Enclave backup', extensions: [EXTENSION] }],
    });
    if (!path) return;
    setError(null); setMade(null);
    let b: BackupSummary;
    try {
      b = await invoke<BackupSummary>('cmd_backup_inspect', { path });
    } catch (e) {
      setError(String(e));
      return;
    }
    if (!window.confirm(
      `Replace ALL data with the backup of ${when(b.created_at)}?\n\n` +
      `It holds ${b.documents} documents, ${b.users} profiles, ${b.departments} departments (${mb(b.bytes)}).\n\n` +
      'Everything added since then is lost. Profiles and PINs become those of the backup — ' +
      'you will sign in with a profile from it. The backup is checked now; the data is replaced when Enclave restarts.',
    )) return;
    setProgress(null); setBusy('restore');
    try {
      setStaged(await invoke<Staged>('cmd_backup_restore', { path }));
    } catch (e) {
      setError(String(e));
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
      setError(String(e));
    }
  }

  return (
    <div className="card">
      <div style={{ marginBottom: 16 }}>
        <div style={{ fontWeight: 600, fontSize: 15 }}>Backup</div>
        <div className="text-sm text-muted" style={{ marginTop: 2 }}>
          One file with the database and every document, of all departments. It is not encrypted:
          keep it as carefully as this computer. Models are not included — they download again.
        </div>
      </div>

      {error && <ErrorText>{error}</ErrorText>}

      {staged ? (
        <div className="backup-staged">
          <div>
            <b>Restore ready.</b> The backup of {when(staged.backup.created_at)} ({staged.backup.documents} documents)
            was checked by {staged.staged_by}; it replaces the current data when Enclave restarts.
          </div>
          <div className="flex gap-3" style={{ marginTop: 10 }}>
            <Button onClick={() => invoke('cmd_restart_app').catch(e => setError(String(e)))}>Restart now</Button>
            <Button variant="ghost" onClick={cancelRestore}>Keep the current data</Button>
          </div>
        </div>
      ) : (
        <div className="flex gap-3 items-center">
          <Button onClick={makeBackup} loading={busy === 'backup'} disabled={busy !== null} spinnerSize={14}>
            Save a backup…
          </Button>
          <Button variant="ghost" onClick={restore} loading={busy === 'restore'} disabled={busy !== null} spinnerSize={14}>
            Restore from a backup…
          </Button>
          {busy && <span className="text-sm text-muted">{stageText(progress)}</span>}
        </div>
      )}

      {made && (
        <div className="text-sm" style={{ marginTop: 12 }}>
          Saved: {made.documents} documents, {made.files} files, {mb(made.bytes)}.
          {made.missing > 0 && (
            <span className="backup-warning">
              {' '}{made.missing} document file(s) were missing or damaged on disk and are not in the backup.
            </span>
          )}
        </div>
      )}
    </div>
  );
}
