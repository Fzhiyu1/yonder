import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './app/App';
import { trackVisualViewport } from './lib/viewport';
import { registerServiceWorker } from './push';
import './index.css';

// Apply the stored/system theme before first paint to avoid a flash.
if (window.matchMedia('(prefers-color-scheme: dark)').matches) document.documentElement.classList.add('dark');

trackVisualViewport();
registerServiceWorker();

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
