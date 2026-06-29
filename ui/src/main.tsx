import { createRoot } from 'react-dom/client';

import { App } from './app/App';
import { BackendProfileProvider } from './lib/backend';
import { ThemeProvider } from './lib/theme';
import { RefreshProvider } from './hooks/useRefreshBus';
import { initAndroidInsets } from './lib/androidInsets';
import './styles.css';

initAndroidInsets();

createRoot(document.getElementById('root')!).render(
  <BackendProfileProvider>
    <ThemeProvider>
      <RefreshProvider>
        <App />
      </RefreshProvider>
    </ThemeProvider>
  </BackendProfileProvider>,
);
