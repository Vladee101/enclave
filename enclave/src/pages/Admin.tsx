import React, { useState, useEffect } from 'react';
import { call, useAppMode } from '../api';
import { useAuth } from '../contexts/AuthContext';
import { useI18n } from '../i18n';
import { Badge } from '../components/Badge';
import { Button } from '../components/Button';
import { FormField } from '../components/FormField';
import { ErrorText } from '../components/ErrorText';
import { BackupCard } from '../components/BackupCard';
import { OfficeServerCard } from '../components/OfficeServerCard';

interface Dept    { id: string; name: string; is_default: boolean; member_count: number; document_count: number; instructions: string | null; }

/** Same limit as the core (instructions::MAX_CHARS) and migration 019. */
const MAX_INSTRUCTIONS = 1000;
interface Adapter { id: string; department_id: string; adapter_path: string; scale: number; is_active: boolean; }
interface UserRow { id: string; username: string; is_admin: boolean; }
interface Membership { user_id: string; username: string; department_id: string; department_name: string; }
interface AuditEntry {
  id: string;
  created_at: string;
  username: string | null;
  department_name: string | null;
  event_type: string;
  payload: Record<string, unknown> | null;
}

export function AdminPage() {
  const { user } = useAuth();
  const { t, tError, tPlural, formatDateTime } = useI18n();
  const mode = useAppMode();
  const [depts,     setDepts]     = useState<Dept[]>([]);
  const [adapters,  setAdapters]  = useState<Adapter[]>([]);
  const [newDept,   setNewDept]   = useState('');
  const [adapterForm, setAdapterForm] = useState({
    department_id: '',
    adapter_path:  '',
    scale:         '1.0',
  });
  const [users,       setUsers]       = useState<UserRow[]>([]);
  const [memberships, setMemberships] = useState<Membership[]>([]);
  const [memberForm,  setMemberForm]  = useState({ user_id: '', department_id: '' });
  const [audit,       setAudit]       = useState<AuditEntry[]>([]);
  const [saving, setSaving] = useState(false);
  const [error,  setError]  = useState<string | null>(null);
  // The department whose instructions (ADR-0029) are open for editing.
  const [editing, setEditing] = useState<{ id: string; text: string } | null>(null);

  async function saveInstructions() {
    if (!editing) return;
    setError(null);
    try {
      await call('cmd_set_department_instructions', {
        args: { department_id: editing.id, instructions: editing.text },
      });
      setEditing(null);
      await load();
    } catch (e) {
      setError(tError(e));
    }
  }

  async function load() {
    if (!user) return;
    const [d, a, u, m, log] = await Promise.all([
      call<Dept[]>('cmd_list_departments'),
      call<Adapter[]>('cmd_list_adapters'),
      call<UserRow[]>('cmd_list_users'),
      call<Membership[]>('cmd_list_memberships'),
      call<AuditEntry[]>('cmd_list_audit', { limit: 100 }),
    ]);
    setDepts(d);
    setAdapters(a);
    setUsers(u);
    setMemberships(m);
    setAudit(log);
    if (d.length > 0 && !adapterForm.department_id) {
      setAdapterForm(prev => ({ ...prev, department_id: d[0].id }));
    }
    if (d.length > 0 && u.length > 0 && !memberForm.user_id) {
      setMemberForm({ user_id: u[0].id, department_id: d[0].id });
    }
  }

  useEffect(() => { load(); }, [user]);

  async function deleteDept(d: Dept) {
    if (!window.confirm(
      t('admin.deleteDeptConfirmTitle', {
        name:     d.name,
        docs:     tPlural('admin.nDocuments', d.document_count),
        members:  tPlural('admin.nMemberships', d.member_count),
      }) + '\n\n' + t('admin.deleteDeptConfirmBody'),
    )) return;
    setError(null);
    try {
      await call('cmd_delete_department', { departmentId: d.id });
    } catch (e) {
      setError(tError(e));
    }
    await load();
  }

  async function createDept(e: React.FormEvent) {
    e.preventDefault();
    if (!user) return;
    setSaving(true);
    await call('cmd_create_department', {
      args: { name: newDept },
    }).catch(e => setError(tError(e)));
    setNewDept('');
    await load();
    setSaving(false);
  }

  async function addMember(e: React.FormEvent) {
    e.preventDefault();
    if (!user) return;
    setSaving(true);
    await call('cmd_add_member', {
      args: memberForm,
    }).catch(e => setError(tError(e)));
    await load();
    setSaving(false);
  }

  async function removeMember(m: Membership) {
    if (!user) return;
    await call('cmd_remove_member', {
      args: { user_id: m.user_id, department_id: m.department_id },
    }).catch(e => setError(tError(e)));
    await load();
  }

  async function addAdapter(e: React.FormEvent) {
    e.preventDefault();
    if (!user) return;
    setSaving(true);
    await call('cmd_add_adapter', {
      args: {
        department_id: adapterForm.department_id,
        adapter_path:  adapterForm.adapter_path,
        scale:         parseFloat(adapterForm.scale),
      },
    }).catch(e => setError(tError(e)));
    setAdapterForm(prev => ({ ...prev, adapter_path: '' }));
    await load();
    setSaving(false);
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 24 }}>

      {/* ── Departments ── */}
      <div className="card">
        <div className="flex justify-between items-center" style={{ marginBottom: 16 }}>
          <div>
            <div style={{ fontWeight: 600, fontSize: 15 }}>{t('admin.departments')}</div>
            <div className="text-sm text-muted" style={{ marginTop: 2 }}>
              {t('admin.departmentsDesc')}
            </div>
          </div>
        </div>

        {error && <ErrorText>{error}</ErrorText>}

        {depts.length > 0 && (
          <table className="admin-table" style={{ marginBottom: 20 }}>
            <thead>
              <tr>
                <th>{t('admin.thName')}</th>
                <th>{t('admin.members')}</th>
                <th>{t('nav.documents')}</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {depts.map(d => (
                <React.Fragment key={d.id}>
                <tr>
                  <td style={{ fontWeight: 500 }}>
                    {d.name}
                    {d.is_default && <> <Badge cls="badge-info">{t('admin.defaultBadge')}</Badge></>}
                  </td>
                  <td>{d.member_count}</td>
                  <td>{d.document_count}</td>
                  <td style={{ textAlign: 'right', whiteSpace: 'nowrap' }}>
                    <Button
                      variant="ghost"
                      onClick={() => setEditing(editing?.id === d.id ? null : { id: d.id, text: d.instructions ?? '' })}
                      aria-expanded={editing?.id === d.id}
                    >
                      {t('admin.instructions')}
                      {d.instructions && <> <Badge cls="badge-success">{t('admin.instructionsSet')}</Badge></>}
                    </Button>
                    {!d.is_default && (
                      <Button variant="ghost" onClick={() => deleteDept(d)} aria-label={t('admin.deleteDeptAria', { name: d.name })}>
                        {t('common.delete')}
                      </Button>
                    )}
                  </td>
                </tr>
                {editing?.id === d.id && (
                  <tr className="instructions-row">
                    <td colSpan={4}>
                      <div className="text-sm text-muted" style={{ marginBottom: 6 }}>
                        {d.is_default ? t('admin.instructionsHintDefault') : t('admin.instructionsHintDept', { name: d.name })}
                      </div>
                      <textarea
                        className="input instructions-input"
                        rows={4}
                        maxLength={MAX_INSTRUCTIONS}
                        placeholder={t('admin.instructionsPlaceholder')}
                        value={editing.text}
                        onChange={e => setEditing({ id: d.id, text: e.target.value })}
                        aria-label={t('admin.instructionsFor', { name: d.name })}
                      />
                      <div className="flex justify-between items-center" style={{ marginTop: 6 }}>
                        <span className="text-sm text-muted">{editing.text.length} / {MAX_INSTRUCTIONS}</span>
                        <div className="flex gap-2">
                          <Button variant="ghost" onClick={() => setEditing(null)}>{t('admin.cancel')}</Button>
                          <Button onClick={saveInstructions}>{t('admin.save')}</Button>
                        </div>
                      </div>
                    </td>
                  </tr>
                )}
                </React.Fragment>
              ))}
            </tbody>
          </table>
        )}

        <form onSubmit={createDept} className="flex gap-3 items-center">
          <input
            id="new-dept-name"
            type="text"
            className="input"
            placeholder={t('admin.newDeptName')}
            value={newDept}
            onChange={e => setNewDept(e.target.value)}
            required
          />
          <Button id="create-dept-btn" type="submit" loading={saving} spinnerSize={14} style={{ flexShrink: 0 }}>
            {t('common.add')}
          </Button>
        </form>
      </div>

      {/* ── Members ── */}
      <div className="card">
        <div style={{ marginBottom: 16 }}>
          <div style={{ fontWeight: 600, fontSize: 15 }}>{t('admin.members')}</div>
          <div className="text-sm text-muted" style={{ marginTop: 2 }}>
            {t('admin.membersDesc')}
          </div>
        </div>

        {memberships.length > 0 && (
          <table className="admin-table" style={{ marginBottom: 20 }}>
            <thead>
              <tr>
                <th>{t('admin.department')}</th>
                <th>{t('admin.user')}</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {memberships.map(m => (
                <tr key={`${m.department_id}:${m.user_id}`}>
                  <td style={{ fontWeight: 500 }}>{m.department_name}</td>
                  <td>{m.username}</td>
                  <td style={{ textAlign: 'right' }}>
                    <Button variant="ghost" onClick={() => removeMember(m)}>{t('admin.remove')}</Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}

        <form onSubmit={addMember} style={{ display: 'grid', gridTemplateColumns: '1fr 1fr auto', gap: 10, alignItems: 'end' }}>
          <FormField label={t('admin.user')} htmlFor="member-user" style={{ marginBottom: 0 }}>
            <select
              id="member-user"
              aria-label={t('admin.user')}
              className="input"
              value={memberForm.user_id}
              onChange={e => setMemberForm(p => ({ ...p, user_id: e.target.value }))}
              required
            >
              {users.map(u => <option key={u.id} value={u.id}>{u.username}</option>)}
            </select>
          </FormField>
          <FormField label={t('admin.department')} htmlFor="member-dept" style={{ marginBottom: 0 }}>
            <select
              id="member-dept"
              aria-label={t('admin.department')}
              className="input"
              value={memberForm.department_id}
              onChange={e => setMemberForm(p => ({ ...p, department_id: e.target.value }))}
              required
            >
              {depts.map(d => <option key={d.id} value={d.id}>{d.name}</option>)}
            </select>
          </FormField>
          <Button id="add-member-btn" type="submit" loading={saving} spinnerSize={14} style={{ flexShrink: 0 }}>
            {t('common.add')}
          </Button>
        </form>
      </div>

      {/* ── LoRA Adapters ── */}
      <div className="card">
        <div style={{ marginBottom: 16 }}>
          <div style={{ fontWeight: 600, fontSize: 15 }}>{t('admin.adapters')}</div>
          <div className="text-sm text-muted" style={{ marginTop: 2 }}>
            {t('admin.adaptersDescPre')} <code>.gguf</code> {t('admin.adaptersDescMid')} <code>binaries/adapters/</code>{t('admin.adaptersDescPost')}
          </div>
        </div>

        {adapters.length > 0 && (
          <table className="admin-table" style={{ marginBottom: 20 }}>
            <thead>
              <tr>
                <th>{t('admin.department')}</th>
                <th>{t('admin.thAdapterPath')}</th>
                <th>{t('admin.thScale')}</th>
                <th>{t('admin.thActive')}</th>
              </tr>
            </thead>
            <tbody>
              {adapters.map(a => (
                <tr key={a.id}>
                  <td>{depts.find(d => d.id === a.department_id)?.name ?? '—'}</td>
                  <td className="mono" style={{ fontSize: 12 }}>{a.adapter_path}</td>
                  <td>{a.scale.toFixed(2)}</td>
                  <td>
                    <Badge cls={a.is_active ? 'badge-success' : 'badge-muted'}>
                      {a.is_active ? t('admin.active') : t('admin.off')}
                    </Badge>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}

        {/* An adapter is a file on the server's disk: registered there, not from a client. */}
        {mode?.mode !== 'client' && <form onSubmit={addAdapter} style={{ display: 'grid', gridTemplateColumns: '1fr 1fr auto', gap: 10 }}>
          <FormField label={t('admin.department')} htmlFor="adapter-dept" style={{ marginBottom: 0 }}>
            <select
              id="adapter-dept"
              aria-label={t('admin.department')}
              className="input"
              value={adapterForm.department_id}
              onChange={e => setAdapterForm(p => ({ ...p, department_id: e.target.value }))}
              required
            >
              {depts.map(d => <option key={d.id} value={d.id}>{d.name}</option>)}
            </select>
          </FormField>
          <FormField label={t('admin.thAdapterPath')} htmlFor="adapter-path" style={{ marginBottom: 0 }}>
            <input
              id="adapter-path"
              type="text"
              className="input"
              placeholder="adapters/legal-v1.gguf"
              value={adapterForm.adapter_path}
              onChange={e => setAdapterForm(p => ({ ...p, adapter_path: e.target.value }))}
              required
            />
          </FormField>
          <FormField label={t('admin.thScale')} htmlFor="adapter-scale" style={{ marginBottom: 0 }}>
            <div className="flex gap-2 items-center">
              <input
                id="adapter-scale"
                aria-label={t('admin.thScale')}
                type="number"
                min="0" max="2" step="0.1"
                className="input"
                style={{ width: 80 }}
                value={adapterForm.scale}
                onChange={e => setAdapterForm(p => ({ ...p, scale: e.target.value }))}
              />
              <Button id="add-adapter-btn" type="submit" loading={saving} spinnerSize={14} style={{ flexShrink: 0 }}>
                {t('common.add')}
              </Button>
            </div>
          </FormField>
        </form>}
      </div>

      {/* ── Office server (ADR-0031) ── */}
      {mode && mode.mode !== 'client' && <OfficeServerCard mode={mode} />}

      {/* ── Backup: of the server's data, made on the server ── */}
      {mode?.mode !== 'client' && <BackupCard />}

      {/* ── Audit log ── */}
      <div className="card">
        <div style={{ marginBottom: 16 }}>
          <div style={{ fontWeight: 600, fontSize: 15 }}>{t('admin.auditLog')}</div>
          <div className="text-sm text-muted" style={{ marginTop: 2 }}>
            {t('admin.auditDesc')}
          </div>
        </div>
        {audit.length === 0 ? (
          <div className="text-sm text-muted">{t('admin.noEvents')}</div>
        ) : (
          <table className="admin-table">
            <thead>
              <tr>
                <th>{t('admin.thTime')}</th>
                <th>{t('admin.user')}</th>
                <th>{t('admin.thEvent')}</th>
                <th>{t('admin.department')}</th>
                <th>{t('admin.thDetails')}</th>
              </tr>
            </thead>
            <tbody>
              {audit.map(e => (
                <tr key={e.id}>
                  <td style={{ whiteSpace: 'nowrap' }}>{formatDateTime(e.created_at)}</td>
                  <td>{e.username ?? '—'}</td>
                  <td className="mono" style={{ fontSize: 12 }}>{e.event_type}</td>
                  <td>{e.department_name ?? '—'}</td>
                  <td className="mono" style={{ fontSize: 11, color: 'var(--text-muted)' }}>
                    {e.payload && Object.keys(e.payload).length > 0 ? JSON.stringify(e.payload) : ''}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}
