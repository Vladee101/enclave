import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { useI18n, LangToggle } from '../i18n';

interface ModelStatus {
  key:     string;
  /** The model's or build's own name — never translated. */
  name:    string;
  state:   'ready' | 'missing' | 'unverified';
  size:    number;
  partial: number;
}

interface Progress { key: string; downloaded: number; total: number; }
interface Done     { ok: boolean; error: string | null; }


/**
 * First-run setup (ADR-0024, ADR-0025): shown until both models and the
 * inference engine for this machine are installed.
 * Downloads them from their official repositories (resumable, checked
 * against pinned SHA-256), or takes a file the user already has — for an
 * offline machine. The app restarts to load them.
 */
export function ModelSetup() {
  const { t, tError, formatSize } = useI18n();
  const gb = (bytes: number) => formatSize(bytes, 'GB');
  const itemLabel = (m: ModelStatus) => {
    switch (m.key) {
      case 'chat':                return t('modelSetup.itemChat', { name: m.name });
      case 'embed':               return t('modelSetup.itemEmbed', { name: m.name });
      case 'engine':              return t('modelSetup.itemEngine', { name: m.name });
      case 'engine-cuda-runtime': return t('modelSetup.itemCudaRuntime');
      default:                    return m.name;
    }
  };
  const [models,     setModels]     = useState<ModelStatus[] | null>(null);
  const [progress,   setProgress]   = useState<Record<string, number>>({});
  const [running,    setRunning]    = useState(false);
  const [error,      setError]      = useState<string | null>(null);
  const [collapsed,  setCollapsed]  = useState(false);
  const [paths,      setPaths]      = useState<Record<string, string>>({});
  const [installed,  setInstalled]  = useState(false);

  const refresh = useCallback(async () => {
    try {
      setModels(await invoke<ModelStatus[]>('cmd_models_status'));
    } catch (e) {
      setError(tError(e));
    }
  }, []);

  useEffect(() => { refresh(); }, [refresh]);

  useEffect(() => {
    const unlisten = [
      listen<Progress>('models-progress', e => {
        setRunning(true);
        setProgress(prev => ({ ...prev, [e.payload.key]: e.payload.downloaded }));
      }),
      listen<Done>('models-done', e => {
        setRunning(false);
        if (e.payload.ok) {
          setInstalled(true);
        } else {
          setError(e.payload.error);
        }
        refresh();
      }),
    ];
    return () => { unlisten.forEach(p => p.then(f => f())); };
  }, [refresh]);

  if (!models) return null;
  const pending = models.filter(m => m.state !== 'ready');
  if (pending.length === 0 && !installed) return null;

  const download = async () => {
    setError(null);
    setRunning(true);
    try {
      await invoke('cmd_download_models');
    } catch (e) {
      setRunning(false);
      setError(tError(e));
    }
  };

  const importFile = async (key: string) => {
    setError(null);
    try {
      await invoke('cmd_import_model', { key, path: paths[key] ?? '' });
      setInstalled(true);
      await refresh();
    } catch (e) {
      setError(tError(e));
    }
  };

  const total = pending.reduce((sum, m) => sum + m.size, 0);

  return (
    <div className={`model-setup${collapsed ? ' collapsed' : ''}`}>
      <div className="model-setup-header">
        <span>{pending.length === 0 ? t('modelSetup.modelsInstalled') : t('modelSetup.modelsNeeded')}</span>
        {/* First-run overlay renders above the login card too — the language
            switch must be reachable even before signing in. */}
        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <LangToggle />
          <button type="button" className="model-setup-toggle" onClick={() => setCollapsed(c => !c)}>
            {collapsed ? '▴' : '▾'}
          </button>
        </div>
      </div>
      {!collapsed && (
        <div className="model-setup-body">
          {pending.length === 0 ? (
            <>
              <p>{t('modelSetup.restartToLoad')}</p>
              <button
                type="button"
                className="btn btn-primary"
                onClick={() => invoke('cmd_restart_app').catch(e => setError(tError(e)))}
              >
                {t('common.restartNow')}
              </button>
            </>
          ) : (
            <>
              <p>{t('modelSetup.downloadDesc', { size: gb(total) })}</p>
              {pending.map(m => {
                const done = progress[m.key] ?? m.partial;
                const pct = Math.min(100, Math.round((done / m.size) * 100));
                return (
                  <div key={m.key} className="model-setup-item">
                    <div className="model-setup-row">
                      <span>{itemLabel(m)}</span>
                      <span className="model-setup-size">
                        {done > 0 ? `${gb(done)} / ` : ''}{gb(m.size)}
                      </span>
                    </div>
                    <div className="model-setup-bar"><div style={{ width: `${pct}%` }} /></div>
                    {m.state === 'unverified' && (
                      <div className="model-setup-note">{t('modelSetup.unverified')}</div>
                    )}
                    {!running && !m.key.startsWith('engine') && (
                      <div className="model-setup-import">
                        <input
                          className="input"
                          placeholder={t('modelSetup.importPlaceholder')}
                          value={paths[m.key] ?? ''}
                          onChange={e => setPaths(prev => ({ ...prev, [m.key]: e.target.value }))}
                        />
                        <button type="button" className="btn btn-ghost" disabled={!paths[m.key]} onClick={() => importFile(m.key)}>
                          {t('modelSetup.use')}
                        </button>
                      </div>
                    )}
                  </div>
                );
              })}
              <button type="button" className="btn btn-primary" onClick={download} disabled={running}>
                {running
                  ? t('modelSetup.downloading')
                  : pending.some(m => m.partial > 0) ? t('modelSetup.resume') : t('modelSetup.download')}
              </button>
            </>
          )}
          {error && <div className="model-setup-error">{error}</div>}
        </div>
      )}
    </div>
  );
}
