import React, { useState } from 'react';
import { AuthProvider, useAuth } from './contexts/AuthContext';
import { LoginPage }     from './pages/Login';
import { ChatPage }      from './pages/Chat';
import { DocumentsPage } from './pages/Documents';
import { AdminPage }     from './pages/Admin';
import './styles/index.css';

type Page = 'chat' | 'documents' | 'admin';

// ─── Navigation icons ─────────────────────────────────────────────────────────

function IconChat()  { return <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></svg>; }
function IconDocs()  { return <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/></svg>; }
function IconAdmin() { return <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><circle cx="12" cy="12" r="3"/><path d="M19.07 4.93l-1.41 1.41M4.93 4.93l1.41 1.41M12 2v2m0 16v2m7.07-7.07-1.41-1.41M4.93 19.07l1.41-1.41M2 12h2m16 0h2"/></svg>; }
function IconLogout(){ return <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><polyline points="16 17 21 12 16 7"/><line x1="21" y1="12" x2="9" y2="12"/></svg>; }

// ─── Authenticated shell ──────────────────────────────────────────────────────

function AppShell() {
  const { user, logout } = useAuth();
  const [page, setPage]  = useState<Page>('chat');

  const navItems: { id: Page; label: string; icon: React.ReactNode }[] = [
    { id: 'chat',      label: 'Chat',      icon: <IconChat /> },
    { id: 'documents', label: 'Documents', icon: <IconDocs /> },
    ...(user?.is_admin ? [{ id: 'admin' as Page, label: 'Admin', icon: <IconAdmin /> }] : []),
  ];

  const pageTitles: Record<Page, { title: string; subtitle: string }> = {
    chat:      { title: 'Chat',      subtitle: 'Query your knowledge base with privacy-first AI' },
    documents: { title: 'Documents', subtitle: 'Manage and ingest documents for RAG retrieval' },
    admin:     { title: 'Admin',     subtitle: 'Configure departments and LoRA adapters' },
  };

  return (
    <div className="app-layout">
      {/* Sidebar */}
      <aside className="sidebar">
        <div className="sidebar-logo">
          <div className="sidebar-logo-icon">🔒</div>
          <span className="sidebar-logo-text">Enclave</span>
        </div>

        {navItems.map(item => (
          <button
            key={item.id}
            id={`nav-${item.id}`}
            className={`nav-item${page === item.id ? ' active' : ''}`}
            onClick={() => setPage(item.id)}
          >
            {item.icon}
            {item.label}
          </button>
        ))}

        <div className="sidebar-spacer" />

        <div className="sidebar-user">
          <div className="sidebar-avatar">{user?.username[0].toUpperCase()}</div>
          <span className="sidebar-username">{user?.username}</span>
          <button
            id="logout-btn"
            className="sidebar-logout"
            onClick={logout}
            title="Sign out"
          >
            <IconLogout />
          </button>
        </div>
      </aside>

      {/* Main */}
      <div className="main-content">
        <header className="page-header">
          <div>
            <div className="page-title">{pageTitles[page].title}</div>
            <div className="page-subtitle">{pageTitles[page].subtitle}</div>
          </div>
        </header>

        {page === 'chat' ? (
          <ChatPage />
        ) : (
          <div className="page-body">
            {page === 'documents' && <DocumentsPage />}
            {page === 'admin'     && <AdminPage />}
          </div>
        )}
      </div>
    </div>
  );
}

// ─── Root ─────────────────────────────────────────────────────────────────────

export default function App() {
  const { user } = useAuth();
  return user ? <AppShell /> : <LoginPage />;
}

export function AppWithProviders() {
  return (
    <AuthProvider>
      <App />
    </AuthProvider>
  );
}
