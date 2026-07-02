import React from 'react';
import ReactDOM from 'react-dom/client';
import { AppWithProviders } from './App';

ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
  <React.StrictMode>
    <AppWithProviders />
  </React.StrictMode>,
);
