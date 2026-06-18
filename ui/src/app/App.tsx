// @arch arch_MK2Q2.1
import { Suspense, lazy } from 'react';
import { RouterProvider } from '@tanstack/react-router';

import { router } from './router';

const RouterDevtools = import.meta.env.DEV
  ? lazy(() =>
      import('@tanstack/react-router-devtools').then((module) => ({
        default: module.TanStackRouterDevtools,
      })),
    )
  : null;

export function App() {
  return (
    <>
      <RouterProvider router={router} />
      {RouterDevtools ? (
        <Suspense fallback={null}>
          <RouterDevtools router={router} initialIsOpen={false} />
        </Suspense>
      ) : null}
    </>
  );
}
