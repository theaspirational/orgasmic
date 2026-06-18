export type ProjectConfig = {
  pipeline: string[];
  testCmd: string;
  lintCmd: string;
  buildCmd: string;
  defaultBranch: string;
};

export function emptyConfig(): ProjectConfig {
  return { pipeline: [], testCmd: '', lintCmd: '', buildCmd: '', defaultBranch: '' };
}

export function parseProp(contents: string, key: string): string {
  // [^\S\r\n]* = inline whitespace only, so an empty property does not let \s*
  // swallow the newline and capture the following line.
  return new RegExp(`^:${key}:[^\\S\\r\\n]*(.*)$`, 'm').exec(contents)?.[1]?.trim() ?? '';
}

export function parseConfig(contents: string): ProjectConfig {
  return {
    pipeline: parseProp(contents, 'PIPELINE').split(/\s+/).filter(Boolean),
    testCmd: parseProp(contents, 'TEST_CMD'),
    lintCmd: parseProp(contents, 'LINT_CMD'),
    buildCmd: parseProp(contents, 'BUILD_CMD'),
    defaultBranch: parseProp(contents, 'DEFAULT_BRANCH'),
  };
}

/** Upsert each `:KEY: value` line inside the config.org `* CONFIG` property drawer, preserving everything else. */
export function spliceConfig(contents: string, config: ProjectConfig): string {
  const props: Array<[string, string]> = [
    ['PIPELINE', config.pipeline.map((worker) => worker.trim()).filter(Boolean).join(' ')],
    ['TEST_CMD', config.testCmd.trim()],
    ['LINT_CMD', config.lintCmd.trim()],
    ['BUILD_CMD', config.buildCmd.trim()],
    ['DEFAULT_BRANCH', config.defaultBranch.trim()],
  ];
  const lines = contents.split(/\r?\n/);
  const configIndex = lines.findIndex((line) => /^\* CONFIG\b/.test(line));
  if (configIndex < 0) return contents;
  let start = lines.findIndex((line, i) => i > configIndex && line.trim() === ':PROPERTIES:');
  if (start < 0) {
    lines.splice(configIndex + 1, 0, ':PROPERTIES:', ':END:');
    start = configIndex + 1;
  }
  let end = lines.findIndex((line, i) => i > start && line.trim() === ':END:');
  if (end < 0) {
    lines.splice(start + 1, 0, ':END:');
    end = start + 1;
  }
  for (let i = end - 1; i > start; i--) {
    if (/^:STAGE_WORKER_[A-Z_]+:/.test(lines[i])) {
      lines.splice(i, 1);
      end--;
    }
  }
  for (const [key, value] of props) {
    const tag = `:${key}:`;
    const line = `${tag.padEnd(25)}${value}`;
    const existing = lines.findIndex((c, i) => i > start && i < end && c.startsWith(tag));
    if (existing >= 0) lines[existing] = line;
    else {
      lines.splice(end, 0, line);
      end++;
    }
  }
  return lines.join('\n');
}
