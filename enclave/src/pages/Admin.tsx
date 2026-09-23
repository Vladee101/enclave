import React, { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAuth } from '../contexts/AuthContext';
import { Badge } from '../components/Badge';
import { Button } from '../components/Button';
import { FormField } from '../components/FormField';
import { ErrorText } from '../components/ErrorText';

interface Dept    { id: string; name: string; is_default: boolean; member_count: number; document_count: number; }
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

  async function load() {
    if (!user) return;
    const [d, a, u, m, log] = await Promise.all([
      invoke<Dept[]>('cmd_list_departments'),
      invoke<Adapter[]>('cmd_list_adapters'),
      invoke<UserRow[]>('cmd_list_users'),
      invoke<Membership[]>('cmd_list_memberships'),
      invoke<AuditEntry[]>('cmd_list_audit', { limit: 100 }),
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
    const docs = d.document_count === 1 ? '1 document' : `${d.document_count} documents`;
    const members = d.member_count === 1 ? '1 membership' : `${d.member_count} memberships`;
    if (!window.confirm(
      `Delete department "${d.name}" together with its ${docs} and ${members}?\n\n` +
      'The documents\' text is removed from search immediately. Users stay, in their other departments. This cannot be undone.',
    )) return;
    setError(null);
    try {
      await invoke('cmd_delete_department', { departmentId: d.id });
    } catch (e) {
      setError(String(e));
    }
    await load();
  }

  async function createDept(e: React.FormEvent) {
    e.preventDefault();
    if (!user) return;
    setSaving(true);
    await invoke('cmd_create_department', {
      args: { name: newDept },
    }).catch(console.error);
    setNewDept('');
    await load();
    setSaving(false);
  }

  async function addMember(e: React.FormEvent) {
    e.preventDefault();
    if (!user) return;
    setSaving(true);
    await invoke('cmd_add_member', {
      args: memberForm,
    }).catch(console.error);
    await load();
    setSaving(false);
  }

  async function removeMember(m: Membership) {
    if (!user) return;
    await invoke('cmd_remove_member', {
      args: { user_id: m.user_id, department_id: m.department_id },
    }).catch(console.error);
    await load();
  }

  async function addAdapter(e: React.FormEvent) {
    e.preventDefault();
    if (!user) return;
    setSaving(true);
    await invoke('cmd_add_adapter', {
      args: {
        department_id: adapterForm.department_id,
        adapter_path:  adapterForm.adapter_path,
        scale:         parseFloat(adapterForm.scale),
      },
    }).catch(console.error);
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
            <div style={{ fontWeight: 600, fontSize: 15 }}>Departments</div>
            <div className="text-sm text-muted" style={{ marginTop: 2 }}>
              Each department can have its own documents and LoRA adapter. New profiles join only the default department; add them to others under Members.
            </div>
          </div>
        </div>

        {error && <ErrorText>{error}</ErrorText>}

        {depts.length > 0 && (
          <table className="admin-table" style={{ marginBottom: 20 }}>
            <thead>
              <tr>
                <th>Name</th>
                <th>Members</th>
                <th>Documents</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {depts.map(d => (
                <tr key={d.id}>
                  <td style={{ fontWeight: 500 }}>
                    {d.name}
                    {d.is_default && <> <Badge cls="badge-info">default · everyone</Badge></>}
                  </td>
                  <td>{d.member_count}</td>
                  <td>{d.document_count}</td>
                  <td style={{ textAlign: 'right' }}>
                    {!d.is_default && (
                      <Button variant="ghost" onClick={() => deleteDept(d)} aria-label={`Delete department ${d.name}`}>
                        Delete
                      </Button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}

        <form onSubmit={createDept} className="flex gap-3 items-center">
          <input
            id="new-dept-name"
            type="text"
            className="input"
            placeholder="New department name…"
            value={newDept}
            onChange={e => setNewDept(e.target.value)}
            required
          />
          <Button id="create-dept-btn" type="submit" loading={saving} spinnerSize={14} style={{ flexShrink: 0 }}>
            + Add
          </Button>
        </form>
      </div>

      {/* ── Members ── */}
      <div className="card">
        <div style={{ marginBottom: 16 }}>
          <div style={{ fontWeight: 600, fontSize: 15 }}>Members</div>
          <div className="text-sm text-muted" style={{ marginTop: 2 }}>
            Membership is what RLS checks on every query (ADR-0008): a change applies to the member's next request.
          </div>
        </div>

        {memberships.length > 0 && (
          <table className="admin-table" style={{ marginBottom: 20 }}>
            <thead>
              <tr>
                <th>Department</th>
                <th>User</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {memberships.map(m => (
                <tr key={`${m.department_id}:${m.user_id}`}>
                  <td style={{ fontWeight: 500 }}>{m.department_name}</td>
                  <td>{m.username}</td>
                  <td style={{ textAlign: 'right' }}>
                    <Button variant="ghost" onClick={() => removeMember(m)}>Remove</Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}

        <form onSubmit={addMember} style={{ display: 'grid', gridTemplateColumns: '1fr 1fr auto', gap: 10, alignItems: 'end' }}>
          <FormField label="User" htmlFor="member-user" style={{ marginBottom: 0 }}>
            <select
              id="member-user"
              aria-label="User"
              className="input"
              value={memberForm.user_id}
              onChange={e => setMemberForm(p => ({ ...p, user_id: e.target.value }))}
              required
            >
              {users.map(u => <option key={u.id} value={u.id}>{u.username}</option>)}
            </select>
          </FormField>
          <FormField label="Department" htmlFor="member-dept" style={{ marginBottom: 0 }}>
            <select
              id="member-dept"
              aria-label="Department"
              className="input"
              value={memberForm.department_id}
              onChange={e => setMemberForm(p => ({ ...p, department_id: e.target.value }))}
              required
            >
              {depts.map(d => <option key={d.id} value={d.id}>{d.name}</option>)}
            </select>
          </FormField>
          <Button id="add-member-btn" type="submit" loading={saving} spinnerSize={14} style={{ flexShrink: 0 }}>
            + Add
          </Button>
        </form>
      </div>

      {/* ── LoRA Adapters ── */}
      <div className="card">
        <div style={{ marginBottom: 16 }}>
          <div style={{ fontWeight: 600, fontSize: 15 }}>LoRA Adapters</div>
          <div className="text-sm text-muted" style={{ marginTop: 2 }}>
            Per-department adapters are hot-swapped per request (ADR-0003, 0004).
            Place <code>.gguf</code> adapter files in <code>binaries/adapters/</code>.
          </div>
        </div>

        {adapters.length > 0 && (
          <table className="admin-table" style={{ marginBottom: 20 }}>
            <thead>
              <tr>
                <th>Department</th>
                <th>Adapter path</th>
                <th>Scale</th>
                <th>Active</th>
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
                      {a.is_active ? 'Active' : 'Off'}
                    </Badge>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}

        <form onSubmit={addAdapter} style={{ display: 'grid', gridTemplateColumns: '1fr 1fr auto', gap: 10 }}>
          <FormField label="Department" htmlFor="adapter-dept" style={{ marginBottom: 0 }}>
            <select
              id="adapter-dept"
              aria-label="Department"
              className="input"
              value={adapterForm.department_id}
              onChange={e => setAdapterForm(p => ({ ...p, department_id: e.target.value }))}
              required
            >
              {depts.map(d => <option key={d.id} value={d.id}>{d.name}</option>)}
            </select>
          </FormField>
          <FormField label="Adapter path" htmlFor="adapter-path" style={{ marginBottom: 0 }}>
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
          <FormField label="Scale" htmlFor="adapter-scale" style={{ marginBottom: 0 }}>
            <div className="flex gap-2 items-center">
              <input
                id="adapter-scale"
                aria-label="Scale"
                type="number"
                min="0" max="2" step="0.1"
                className="input"
                style={{ width: 80 }}
                value={adapterForm.scale}
                onChange={e => setAdapterForm(p => ({ ...p, scale: e.target.value }))}
              />
              <Button id="add-adapter-btn" type="submit" loading={saving} spinnerSize={14} style={{ flexShrink: 0 }}>
                + Add
              </Button>
            </div>
          </FormField>
        </form>
      </div>

      {/* ── Audit log ── */}
      <div className="card">
        <div style={{ marginBottom: 16 }}>
          <div style={{ fontWeight: 600, fontSize: 15 }}>Audit log</div>
          <div className="text-sm text-muted" style={{ marginTop: 2 }}>
            Last 100 events. Queries are logged by the documents and chunks they cited, not by their text.
          </div>
        </div>
        {audit.length === 0 ? (
          <div className="text-sm text-muted">No events yet.</div>
        ) : (
          <table className="admin-table">
            <thead>
              <tr>
                <th>Time</th>
                <th>User</th>
                <th>Event</th>
                <th>Department</th>
                <th>Details</th>
              </tr>
            </thead>
            <tbody>
              {audit.map(e => (
                <tr key={e.id}>
                  <td style={{ whiteSpace: 'nowrap' }}>{new Date(e.created_at).toLocaleString()}</td>
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
