import React, { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAuth } from '../contexts/AuthContext';
import { Badge } from '../components/Badge';
import { Button } from '../components/Button';
import { FormField } from '../components/FormField';

interface Dept    { id: string; name: string; }
interface Adapter { id: string; department_id: string; adapter_path: string; scale: number; is_active: boolean; }

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
  const [saving, setSaving] = useState(false);

  async function load() {
    if (!user) return;
    const [d, a] = await Promise.all([
      invoke<Dept[]>('cmd_list_departments', { requestingUserId: user.id }),
      invoke<Adapter[]>('cmd_list_adapters', { requestingUserId: user.id }),
    ]);
    setDepts(d);
    setAdapters(a);
    if (d.length > 0 && !adapterForm.department_id) {
      setAdapterForm(prev => ({ ...prev, department_id: d[0].id }));
    }
  }

  useEffect(() => { load(); }, [user]);

  async function createDept(e: React.FormEvent) {
    e.preventDefault();
    if (!user) return;
    setSaving(true);
    await invoke('cmd_create_department', {
      args: { requesting_user_id: user.id, name: newDept },
    }).catch(console.error);
    setNewDept('');
    await load();
    setSaving(false);
  }

  async function addAdapter(e: React.FormEvent) {
    e.preventDefault();
    if (!user) return;
    setSaving(true);
    await invoke('cmd_add_adapter', {
      args: {
        requesting_user_id: user.id,
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
              Each department can have its own documents and LoRA adapter.
            </div>
          </div>
        </div>

        {depts.length > 0 && (
          <table className="admin-table" style={{ marginBottom: 20 }}>
            <thead>
              <tr>
                <th>Name</th>
                <th>ID</th>
              </tr>
            </thead>
            <tbody>
              {depts.map(d => (
                <tr key={d.id}>
                  <td style={{ fontWeight: 500 }}>{d.name}</td>
                  <td className="mono" style={{ color: 'var(--text-muted)', fontSize: 11 }}>{d.id}</td>
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
    </div>
  );
}
