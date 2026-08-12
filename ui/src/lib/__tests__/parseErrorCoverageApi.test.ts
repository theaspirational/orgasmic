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
      header: 'partial; ready=1/2; markers=0/2',
    });

    await expect(fetchParseErrorsWithCoverage()).resolves.toEqual({
      errors: [],
      coverage: {
        state: 'partial',
        detail: 'partial; ready=1/2; markers=0/2',
        failures: {},
      },
    });
    expect(getWithHeader).toHaveBeenCalledWith(
      '/graph/parse-errors',
      'x-orgasmic-project-coverage',
    );
  });

  it('loads recursive marker coverage only on the explicit full action', async () => {
    get.mockResolvedValue({ node_id: '__coverage__', files: [], projects: {}, failures: {} });
    getWithHeader.mockResolvedValue({
      data: [],
      header: 'complete; ready=2/2; markers=2/2',
    });

    await expect(loadFullParseErrorCoverage()).resolves.toMatchObject({
      coverage: { state: 'complete' },
    });
    expect(get).toHaveBeenCalledTimes(1);
    expect(get).toHaveBeenCalledWith('/graph/markers/__coverage__');
  });

  it('keeps loaded errors and names projects skipped by full marker coverage', async () => {
    get.mockResolvedValue({
      node_id: '__coverage__',
      files: [],
      projects: { healthy: 0 },
      failures: { blocked: 'filesystem scan timed out' },
    });
    getWithHeader.mockResolvedValue({
      data: [{ path: '/healthy/glossary.org', message: 'bad ref', at: 'now' }],
      header: 'partial; ready=1/2; markers=1/2; marker_unloaded=[blocked]',
    });

    await expect(loadFullParseErrorCoverage()).resolves.toEqual({
      errors: [{ path: '/healthy/glossary.org', message: 'bad ref', at: 'now' }],
      coverage: {
        state: 'partial',
        detail: 'partial; ready=1/2; markers=1/2; marker_unloaded=[blocked]; skipped=[blocked]',
        failures: { blocked: 'filesystem scan timed out' },
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
