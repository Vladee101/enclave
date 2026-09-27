import { useState, useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAuth } from '../contexts/AuthContext';
import { useI18n, type TFunc } from '../i18n';
import { useJobPoller } from '../hooks/useJobPoller';
import { Badge } from '../components/Badge';
import { Spinner } from '../components/Spinner';
import { Button } from '../components/Button';
import { ErrorText } from '../components/ErrorText';

interface DocInfo {
  id:            string;
  filename:      string;
  status:        'pending' | 'ready' | 'failed';
  department_id: string;
  can_delete:    boolean;
}

interface JobStatus {
  job_id:      string;
  document_id: string;
  status:      string;
  attempts:    number;
  error_text:  string | null;
}

// Badge classes per status; the label is translated at render time (i18n).
const STATUS_CLS: Record<string, string> = {
  pending:   'badge-warning',
  ready:     'badge-success',
  failed:    'badge-error',
  queued:    'badge-info',
  running:   'badge-info',
  succeeded: 'badge-success',
};

function statusLabel(t: TFunc, status: string): string {
  switch (status) {
    case 'pending':   return t('documents.statusPending');
    case 'ready':     return t('documents.statusReady');
    case 'failed':    return t('documents.statusFailed');
    case 'queued':    return t('documents.statusQueued');
    case 'running':   return t('documents.statusIngesting');
    case 'succeeded': return t('documents.statusDone');
    default:          return status;
  }
}

function docIcon(filename: string): string {
  const ext = filename.split('.').pop()?.toLowerCase() ?? '';
  if (['pdf'].includes(ext))           return '📄';
  if (['doc','docx'].includes(ext))    return '📝';
  if (['xls','xlsx'].includes(ext))    return '📊';
  if (['ppt','pptx'].includes(ext))    return '📋';
  if (['txt','md'].includes(ext))      return '📃';
  return '📁';
}

interface Dept { id: string; name: string; is_default: boolean; }

/**
 * Which department to preselect as the upload target, or '' for "ask".
 * The one rule: never silently pick the shared department — it is the only
 * direction in which a wrong target exposes a document to everyone. Picking
 * a narrower department by mistake only hides it from colleagues, which the
 * uploader notices and fixes.
 *   - only one department at all → that one (nothing to choose);
 *   - exactly one besides the shared one → that one (the work department);
 *   - otherwise → explicit choice.
 */
function defaultTarget(depts: Dept[]): string {
  if (depts.length === 1) return depts[0].id;
  const specific = depts.filter(d => !d.is_default);
  return specific.length === 1 ? specific[0].id : '';
}

/** The shared department is visible to every user — say so wherever it is chosen. */
function deptLabel(d: Dept, t: TFunc): string {
  return d.is_default ? `${d.name}${t('documents.sharedSuffix')}` : d.name;
}

export function DocumentsPage() {
  const { user } = useAuth();
  const { t } = useI18n();
  const [docs,        setDocs]    = useState<DocInfo[]>([]);
  const [pendingJobs, setPending] = useState<Record<string, string>>({});  // doc_id → job_id
  const [dragOver,    setDragOver] = useState(false);
  const [deptId,      setDeptId]  = useState('');
  const [depts,       setDepts]   = useState<Dept[]>([]);
  const [error,       setError]   = useState<string | null>(null);
  const fileInputRef              = useRef<HTMLInputElement>(null);
  const { jobs, track }           = useJobPoller(1500);

  useEffect(() => {
    if (!user) return;
    invoke<DocInfo[]>('cmd_list_documents').then(setDocs).catch(console.error);
    invoke<Dept[]>('cmd_list_my_departments').then(d => {
      setDepts(d);
      setDeptId(defaultTarget(d));
    }).catch(console.error);
  }, [user]);

  // Sync job statuses back into docs list.
  useEffect(() => {
    Object.values(jobs).forEach(job => {
      if (!job) return;
      if (job.status === 'succeeded') {
        setDocs(prev => prev.map(d =>
          d.id === job.document_id ? { ...d, status: 'ready' } : d
        ));
      } else if (job.status === 'failed') {
        setDocs(prev => prev.map(d =>
          d.id === job.document_id ? { ...d, status: 'failed' } : d
        ));
      }
    });
  }, [jobs]);

  async function upload(file: File) {
    if (!user || !deptId) return;
    try {
      const buf = await file.arrayBuffer();
      const file_contents = Array.from(new Uint8Array(buf));
      const job = await invoke<JobStatus>('cmd_upload_document', {
        args: {
          department_id: deptId,
          filename:      file.name,
          mime_type:     file.type || null,
          file_contents,
        },
      });
      // Optimistically add to list — unless this was a re-upload of a file
      // the department already has, in which case the backend returned the
      // existing document (requeued if it had failed).
      setDocs(prev => prev.some(d => d.id === job.document_id)
        ? prev.map(d => d.id === job.document_id && job.status === 'queued' ? { ...d, status: 'pending' } : d)
        : [{
            id: job.document_id,
            filename: file.name,
            status: 'pending',
            department_id: deptId,
            can_delete: true, // the uploader may always delete their own
          }, ...prev]);
      setPending(prev => ({ ...prev, [job.document_id]: job.job_id }));
      track(job.job_id);
    } catch (e) {
      setError(t('documents.uploadFailed', { name: file.name, error: String(e) }));
    }
  }

  async function remove(doc: DocInfo) {
    if (!window.confirm(t('documents.deleteConfirmTitle', { name: doc.filename }) + '\n\n' + t('documents.deleteConfirmBody'))) return;
    setError(null);
    try {
      await invoke('cmd_delete_document', { documentId: doc.id });
      setDocs(prev => prev.filter(d => d.id !== doc.id));
    } catch (e) {
      setError(String(e));
    }
  }

  function handleFiles(files: FileList | null) {
    if (!files || files.length === 0) return;
    if (!deptId) {
      setError(t('documents.chooseDeptError'));
      return;
    }
    setError(null);
    Array.from(files).forEach(upload);
  }

  const target = depts.find(d => d.id === deptId);

  function resolveStatus(doc: DocInfo): { cls: string; label: string; running: boolean } {
    // A live job knows better; an unknown job status falls back to the doc's.
    const jobId = pendingJobs[doc.id];
    const jstatus = jobId ? jobs[jobId]?.status : undefined;
    const status = jstatus && STATUS_CLS[jstatus] ? jstatus : doc.status;
    return {
      cls:     STATUS_CLS[status] ?? 'badge-muted',
      label:   statusLabel(t, status),
      running: status === 'running',
    };
  }

  return (
    <div>
      {/* Department selector — only when there is a choice to make; the
          target is always spelled out in the upload zone below. */}
      {depts.length > 1 && (
      <div className="flex items-center gap-3" style={{ marginBottom: 12 }}>
        <span className="text-sm text-muted">{t('documents.uploadTo')}</span>
        <select
          id="dept-select"
          aria-label={t('documents.uploadToAria')}
          className="input"
          style={{ width: 'auto' }}
          value={deptId}
          onChange={e => { setDeptId(e.target.value); setError(null); }}
        >
          <option value="" disabled>{t('documents.chooseDept')}</option>
          {depts.map(d => (
            <option key={d.id} value={d.id}>{deptLabel(d, t)}</option>
          ))}
        </select>
      </div>
      )}

      {/* Upload zone */}
      <div
        className={`drop-zone${dragOver ? ' drag-over' : ''}`}
        style={{ marginBottom: 24, opacity: target ? 1 : 0.6 }}
        onClick={() => (target ? fileInputRef.current?.click() : setError(t('documents.chooseDeptError')))}
        onDragOver={e => { e.preventDefault(); setDragOver(true); }}
        onDragLeave={() => setDragOver(false)}
        onDrop={e => { e.preventDefault(); setDragOver(false); handleFiles(e.dataTransfer.files); }}
        role="button"
        tabIndex={0}
        aria-label={target ? t('documents.uploadToNameAria', { name: target.name }) : t('documents.chooseFirstAria')}
      >
        <div className="drop-zone-icon">📂</div>
        <div className="drop-zone-text">
          {target ? <>{t('documents.dropZoneIntro')} <strong>{deptLabel(target, t)}</strong></> : t('documents.chooseAbove')}
        </div>
        <div className="drop-zone-hint">{t('documents.dropZoneHint')}</div>
        <input
          id="file-input"
          aria-label={t('documents.chooseFilesAria')}
          ref={fileInputRef}
          type="file"
          style={{ display: 'none' }}
          multiple
          accept=".pdf,.docx,.xlsx,.xlsm,.xlsb,.xls,.ods,.txt,.md,.markdown"
          onChange={e => { handleFiles(e.target.files); e.target.value = ''; }}
        />
      </div>

      {error && <ErrorText>{error}</ErrorText>}

      {/* Documents list */}
      <div className="doc-grid">
        {docs.length === 0 && (
          <div className="card" style={{ textAlign: 'center', color: 'var(--text-secondary)', padding: 32 }}>
            {t('documents.noDocuments')}
          </div>
        )}
        {docs.map(doc => {
          const badge = resolveStatus(doc);
          return (
            <div key={doc.id} className="doc-row">
              <div className="doc-icon">{docIcon(doc.filename)}</div>
              <div className="doc-meta">
                <div className="doc-name">{doc.filename}</div>
                <div className="doc-info">
                  {(() => { const d = depts.find(d => d.id === doc.department_id); return d ? deptLabel(d, t) : t('documents.unknownDept'); })()}
                </div>
              </div>
              <Badge cls={badge.cls}>
                {badge.running && <Spinner size={10} borderWidth={1.5} />}
                {badge.label}
              </Badge>
              {doc.can_delete && (
                <Button variant="ghost" onClick={() => remove(doc)} aria-label={t('documents.deleteAria', { name: doc.filename })}>
                  {t('common.delete')}
                </Button>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
