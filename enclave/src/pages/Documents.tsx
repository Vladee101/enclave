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

export function DocumentsPage() {
  const { user } = useAuth();
  const [docs,        setDocs]    = useState<DocInfo[]>([]);
  const [pendingJobs, setPending] = useState<Record<string, string>>({});  // doc_id → job_id
  const [dragOver,    setDragOver] = useState(false);
  const [deptId,      setDeptId]  = useState('');
  const [depts,       setDepts]   = useState<{ id: string; name: string }[]>([]);
  const [error,       setError]   = useState<string | null>(null);
  const fileInputRef              = useRef<HTMLInputElement>(null);
  const { jobs, track }           = useJobPoller(1500);

  useEffect(() => {
    if (!user) return;
    invoke<DocInfo[]>('cmd_list_documents').then(setDocs).catch(console.error);
    invoke<{ id: string; name: string }[]>('cmd_list_my_departments').then(d => {
      setDepts(d);
      if (d.length > 0) setDeptId(d[0].id);
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
      console.error('Upload failed:', e);
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
    if (!files) return;
    Array.from(files).forEach(upload);
  }

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
      {/* Upload zone */}
      <div
        className={`drop-zone${dragOver ? ' drag-over' : ''}`}
        style={{ marginBottom: 24 }}
        onClick={() => fileInputRef.current?.click()}
        onDragOver={e => { e.preventDefault(); setDragOver(true); }}
        onDragLeave={() => setDragOver(false)}
        onDrop={e => { e.preventDefault(); setDragOver(false); handleFiles(e.dataTransfer.files); }}
        role="button"
        tabIndex={0}
        aria-label="Upload documents"
      >
        <div className="drop-zone-icon">📂</div>
        <div className="drop-zone-text">Drop documents here or click to browse</div>
        <div className="drop-zone-hint">PDF, DOCX, TXT, MD — all stored locally</div>
        <input
          id="file-input"
          aria-label="Choose files to upload"
          ref={fileInputRef}
          type="file"
          style={{ display: 'none' }}
          multiple
          accept=".pdf,.doc,.docx,.txt,.md,.xls,.xlsx,.ppt,.pptx"
          onChange={e => handleFiles(e.target.files)}
        />
      </div>

      {/* Department selector */}
      {depts.length > 0 && (
        <div className="flex items-center gap-3" style={{ marginBottom: 16 }}>
          <span className="text-sm text-muted">Upload to:</span>
          <select
            id="dept-select"
            aria-label="Upload to department"
            className="input"
            style={{ width: 'auto' }}
            value={deptId}
            onChange={e => setDeptId(e.target.value)}
          >
            {depts.map(d => (
              <option key={d.id} value={d.id}>{d.name}</option>
            ))}
          </select>
        </div>
      )}

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
                  {depts.find(d => d.id === doc.department_id)?.name ?? 'Unknown dept'}
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
