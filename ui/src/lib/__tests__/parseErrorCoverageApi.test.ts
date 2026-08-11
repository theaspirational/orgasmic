import { beforeEach, describe, expect, it, vi } from 'vitest';

const get = vi.fn();
const getWithHeader = vi.fn();
vi.mock('@/lib/transport', () => ({
  get: (...args: unknown[]) => get(...args),
  getWithHeader: (...args: unknown[]) => getWithHeader(...args),
  post: vi.fn(),
  HttpError: class HttpError extends Error {},
}));

import { fetchParseErrorsWithCoverage, loadFullParseErrorCoverage } from '../api';

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
      },
    });
    expect(getWithHeader).toHaveBeenCalledWith(
      '/graph/parse-errors',
      'x-orgasmic-project-coverage',
    );
  });

  it('loads recursive marker coverage only on the explicit full action', async () => {
    get.mockResolvedValue({ node_id: '__coverage__', files: [] });
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
});
