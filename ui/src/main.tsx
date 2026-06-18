import { createRoot } from 'react-dom/client';

import { App } from './app/App';
import { BackendProfileProvider } from './lib/backend';
import { ThemeProvider } from './lib/theme';
import { RefreshProvider } from './hooks/useRefreshBus';
import './styles.css';

createRoot(document.getElementById('root')!).render(
  <BackendProfileProvider>
    <ThemeProvider>
      <RefreshProvider>
        <App />
      </RefreshProvider>
    </ThemeProvider>
  </BackendProfileProvider>,
);
