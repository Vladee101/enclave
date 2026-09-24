import { useState, useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAuth } from '../contexts/AuthContext';
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

const STATUS_BADGE: Record<string, { cls: string; label: string }> = {
  pending:   { cls: 'badge-warning', label: 'Pending' },
  ready:     { cls: 'badge-success', label: 'Ready' },
  failed:    { cls: 'badge-error',   label: 'Failed' },
  queued:    { cls: 'badge-info',    label: 'Queued' },
  running:   { cls: 'badge-info',    label: 'Ingesting…' },
  succeeded: { cls: 'badge-success', label: 'Done' },
};

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
function deptLabel(d: Dept): string {
  return d.is_default ? `${d.name} (visible to everyone)` : d.name;
}

export function DocumentsPage() {
  const { user } = useAuth();
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
      setError(`Upload of "${file.name}" failed: ${e}`);
    }
  }

  async function remove(doc: DocInfo) {
    if (!window.confirm(`Delete "${doc.filename}"? Its text is removed from search immediately; this cannot be undone.`)) return;
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
      setError('Choose the department to upload to first.');
      return;
    }
    setError(null);
    Array.from(files).forEach(upload);
  }

  const target = depts.find(d => d.id === deptId);

  function resolveStatus(doc: DocInfo): { cls: string; label: string } {
    const jobId = pendingJobs[doc.id];
    if (jobId && jobs[jobId]) {
      const j = jobs[jobId];
      return STATUS_BADGE[j.status] ?? STATUS_BADGE[doc.status];
    }
    return STATUS_BADGE[doc.status] ?? { cls: 'badge-muted', label: doc.status };
  }

  return (
    <div>
      {/* Department selector — only when there is a choice to make; the
          target is always spelled out in the upload zone below. */}
      {depts.length > 1 && (
      <div className="flex items-center gap-3" style={{ marginBottom: 12 }}>
        <span className="text-sm text-muted">Upload to:</span>
        <select
          id="dept-select"
          aria-label="Upload to department"
          className="input"
          style={{ width: 'auto' }}
          value={deptId}
          onChange={e => { setDeptId(e.target.value); setError(null); }}
        >
          <option value="" disabled>Choose a department…</option>
          {depts.map(d => (
            <option key={d.id} value={d.id}>{deptLabel(d)}</option>
          ))}
        </select>
      </div>
      )}

      {/* Upload zone */}
      <div
        className={`drop-zone${dragOver ? ' drag-over' : ''}`}
        style={{ marginBottom: 24, opacity: target ? 1 : 0.6 }}
        onClick={() => (target ? fileInputRef.current?.click() : setError('Choose the department to upload to first.'))}
        onDragOver={e => { e.preventDefault(); setDragOver(true); }}
        onDragLeave={() => setDragOver(false)}
        onDrop={e => { e.preventDefault(); setDragOver(false); handleFiles(e.dataTransfer.files); }}
        role="button"
        tabIndex={0}
        aria-label={target ? `Upload documents to ${target.name}` : 'Choose a department first'}
      >
        <div className="drop-zone-icon">📂</div>
        <div className="drop-zone-text">
          {target ? <>Drop documents here to upload to <strong>{deptLabel(target)}</strong></> : 'Choose a department above first'}
        </div>
        <div className="drop-zone-hint">PDF, DOCX, TXT, MD — all stored locally</div>
        <input
          id="file-input"
          aria-label="Choose files to upload"
          ref={fileInputRef}
          type="file"
          style={{ display: 'none' }}
          multiple
          accept=".pdf,.docx,.txt,.md,.markdown"
          onChange={e => { handleFiles(e.target.files); e.target.value = ''; }}
        />
      </div>

      {error && <ErrorText>{error}</ErrorText>}

      {/* Documents list */}
      <div className="doc-grid">
        {docs.length === 0 && (
          <div className="card" style={{ textAlign: 'center', color: 'var(--text-secondary)', padding: 32 }}>
            No documents yet. Upload one above.
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
                  {(() => { const d = depts.find(d => d.id === doc.department_id); return d ? deptLabel(d) : 'Unknown dept'; })()}
                </div>
              </div>
              <Badge cls={badge.cls}>
                {badge.label === 'Ingesting…' && <Spinner size={10} borderWidth={1.5} />}
                {badge.label}
              </Badge>
              {doc.can_delete && (
                <Button variant="ghost" onClick={() => remove(doc)} aria-label={`Delete ${doc.filename}`}>
                  Delete
                </Button>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
