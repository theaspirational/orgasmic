import { describe, it, expect } from 'vitest';
import { parseProp, parseConfig, spliceConfig } from '../configSplice';

const BASE_ORG = `#+TITLE: Test project

* CONFIG
:PROPERTIES:
:PIPELINE:               worker-a worker-b
:TEST_CMD:               cargo test
:LINT_CMD:               cargo clippy
:BUILD_CMD:              cargo build
:DEFAULT_BRANCH:         main
:CUSTOM_KEY:             preserved-value
:END:

* OTHER SECTION
Some content that must not be touched.
`;

describe('parseProp', () => {
  it('extracts a present property', () => {
    expect(parseProp(BASE_ORG, 'PIPELINE')).toBe('worker-a worker-b');
  });

  it('returns empty string for absent property', () => {
    expect(parseProp(BASE_ORG, 'MISSING_KEY')).toBe('');
  });

  it('does not capture the next line when property is empty', () => {
    const src = ':EMPTY_KEY:\n:NEXT_KEY:  value\n';
    expect(parseProp(src, 'EMPTY_KEY')).toBe('');
  });
});

describe('parseConfig', () => {
  it('parses all known fields', () => {
    const cfg = parseConfig(BASE_ORG);
    expect(cfg.pipeline).toEqual(['worker-a', 'worker-b']);
    expect(cfg.testCmd).toBe('cargo test');
    expect(cfg.lintCmd).toBe('cargo clippy');
    expect(cfg.buildCmd).toBe('cargo build');
    expect(cfg.defaultBranch).toBe('main');
  });

  it('returns empty defaults for missing fields', () => {
    const cfg = parseConfig('');
    expect(cfg.pipeline).toEqual([]);
    expect(cfg.testCmd).toBe('');
  });
});

describe('spliceConfig', () => {
  it('updates known fields in place', () => {
    const result = spliceConfig(BASE_ORG, {
      pipeline: ['worker-x'],
      testCmd: 'npm test',
      lintCmd: 'npm run lint',
      buildCmd: 'npm run build',
      defaultBranch: 'develop',
    });
    expect(result).toContain(':PIPELINE:');
    expect(result).toContain('worker-x');
    expect(result).toContain(':TEST_CMD:');
    expect(result).toContain('npm test');
    expect(result).toContain(':DEFAULT_BRANCH:');
    expect(result).toContain('develop');
  });

  it('preserves unrelated custom keys', () => {
    const result = spliceConfig(BASE_ORG, {
      pipeline: ['worker-x'],
      testCmd: 'cargo test',
      lintCmd: 'cargo clippy',
      buildCmd: 'cargo build',
      defaultBranch: 'main',
    });
    expect(result).toContain(':CUSTOM_KEY:');
    expect(result).toContain('preserved-value');
  });

  it('preserves content outside the CONFIG section', () => {
    const result = spliceConfig(BASE_ORG, {
      pipeline: [],
      testCmd: '',
      lintCmd: '',
      buildCmd: '',
      defaultBranch: '',
    });
    expect(result).toContain('* OTHER SECTION');
    expect(result).toContain('Some content that must not be touched.');
    expect(result).toContain('#+TITLE: Test project');
  });

  it('returns contents unchanged when there is no * CONFIG heading', () => {
    const noConfig = '#+TITLE: No config\n\n* TASKS\nsome tasks\n';
    const result = spliceConfig(noConfig, {
      pipeline: ['worker'],
      testCmd: 'test',
      lintCmd: '',
      buildCmd: '',
      defaultBranch: 'main',
    });
    expect(result).toBe(noConfig);
  });

  it('removes :STAGE_WORKER_*: entries (existing contract)', () => {
    const withStageWorkers = `* CONFIG
:PROPERTIES:
:PIPELINE:               worker-a
:STAGE_WORKER_IMPL:      impl-worker
:STAGE_WORKER_REVIEW:    review-worker
:TEST_CMD:               cargo test
:LINT_CMD:               cargo clippy
:BUILD_CMD:              cargo build
:DEFAULT_BRANCH:         main
:END:
`;
    const result = spliceConfig(withStageWorkers, {
      pipeline: ['worker-a'],
      testCmd: 'cargo test',
      lintCmd: 'cargo clippy',
      buildCmd: 'cargo build',
      defaultBranch: 'main',
    });
    expect(result).not.toContain(':STAGE_WORKER_IMPL:');
    expect(result).not.toContain(':STAGE_WORKER_REVIEW:');
    expect(result).toContain(':PIPELINE:');
  });

  it('inserts a PROPERTIES block when none exists', () => {
    const noProps = '* CONFIG\n\n* TASKS\n';
    const result = spliceConfig(noProps, {
      pipeline: ['worker'],
      testCmd: '',
      lintCmd: '',
      buildCmd: '',
      defaultBranch: 'main',
    });
    expect(result).toContain(':PROPERTIES:');
    expect(result).toContain(':END:');
    expect(result).toContain(':PIPELINE:');
    expect(result).toContain('worker');
  });
});
