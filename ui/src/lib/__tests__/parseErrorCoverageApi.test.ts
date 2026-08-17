import { beforeEach, describe, expect, it, vi } from 'vitest';

const get = vi.fn();
const getWithHeader = vi.fn();
vi.mock('@/lib/transport', () => ({
  get: (...args: unknown[]) => get(...args),
  getWithHeader: (...args: unknown[]) => getWithHeader(...args),
  post: vi.fn(),
  HttpError: class HttpError extends Error {},
}));

import {
  fetchParseErrorsWithCoverage,
  fetchTxWithCoverage,
  loadFullParseErrorCoverage,
} from '../api';

beforeEach(() => {
  vi.clearAllMocks();
});

describe('parse-error coverage api', () => {
  it('carries the daemon coverage header with the parse-error rows', async () => {
    getWithHeader.mockResolvedValue({
      data: [],
      header: 'partial; ready=1/2; unloaded=[paused]',
    });

    await expect(fetchParseErrorsWithCoverage()).resolves.toEqual({
      errors: [],
      coverage: {
        state: 'partial',
        detail: 'partial; ready=1/2; unloaded=[paused]',
        failures: {},
      },
    });
    expect(getWithHeader).toHaveBeenCalledWith(
      '/graph/parse-errors',
      'x-orgasmic-project-coverage',
    );
  });

  it('requests full coverage only on the explicit full action', async () => {
    getWithHeader.mockResolvedValue({
      data: [],
      header: 'complete; ready=2/2; unloaded=[]; loading=[]; failed=[]',
    });

    await expect(loadFullParseErrorCoverage()).resolves.toMatchObject({
      coverage: { state: 'complete' },
    });
    expect(get).not.toHaveBeenCalled();
    expect(getWithHeader).toHaveBeenCalledWith(
      '/graph/parse-errors?full=true',
      'x-orgasmic-project-coverage',
    );
  });

  it('names projects skipped by full coverage from the failed segment', async () => {
    getWithHeader.mockResolvedValue({
      data: [{ path: '/healthy/glossary.org', message: 'bad ref', at: 'now' }],
      header: 'partial; ready=1/2; unloaded=[]; loading=[]; failed=[blocked]',
    });

    await expect(loadFullParseErrorCoverage()).resolves.toEqual({
      errors: [{ path: '/healthy/glossary.org', message: 'bad ref', at: 'now' }],
      coverage: {
        state: 'partial',
        detail: 'partial; ready=1/2; unloaded=[]; loading=[]; failed=[blocked]',
        failures: { blocked: '' },
      },
    });
  });

  it('carries partial unscoped tx coverage alongside successful rows', async () => {
    getWithHeader.mockResolvedValue({
      data: [{ project_id: 'healthy', entry: { tx_id: 'tx-healthy' } }],
      header: 'partial; loaded=[healthy]; failed=[blocked]',
    });

    await expect(fetchTxWithCoverage(200)).resolves.toMatchObject({
      records: [{ project_id: 'healthy' }],
      coverage: {
        state: 'partial',
        detail: 'partial; loaded=[healthy]; failed=[blocked]',
      },
    });
    expect(getWithHeader).toHaveBeenCalledWith(
      '/tx?limit=200',
      'x-orgasmic-project-coverage',
    );
  });

  it('preserves the additive delayed tx coverage segment', async () => {
    getWithHeader.mockResolvedValue({
      data: [],
      header: 'partial; loaded=[]; delayed=[queued]; failed=[]',
    });

    await expect(fetchTxWithCoverage()).resolves.toMatchObject({
      records: [],
      coverage: {
        state: 'partial',
        detail: 'partial; loaded=[]; delayed=[queued]; failed=[]',
      },
    });
  });
});
