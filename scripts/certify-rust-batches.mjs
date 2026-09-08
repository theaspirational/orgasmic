// Same default workspace test coverage, one Cargo target at a time. No receipts
// or partial-run reuse: certify-pr owns the single all-green tree receipt.
import { spawnSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';

export function planBatches(metadata) {
  const batches = [];
  if (!metadata.workspace_members?.length) throw new Error('empty workspace');
  for (const id of metadata.workspace_members) {
    const pkg = metadata.packages.find(p => p.id === id);
    if (!pkg?.targets?.length) throw new Error(`missing targets for ${id}`);
    for (const target of pkg.targets) {
      const kind = target.kind[0];
      if (kind === 'custom-build' || kind === 'bench') continue;
      // Fail closed if a future target needs feature selection. Do not quietly
      // omit it or enable features absent from the default certification gate.
      if (target['required-features']?.length) {
        throw new Error(`feature-gated target needs a coverage decision: ${pkg.name}/${target.name}`);
      }
      const selector = { lib: ['--lib'], 'proc-macro': ['--lib'],
        bin: ['--bin', target.name], test: ['--test', target.name],
        example: ['--example', target.name] }[kind];
      if (!selector) throw new Error(`unsupported target kind: ${kind}`);
      const args = ['--locked', '-p', pkg.name];
      if (target.test) batches.push(['bash', 'scripts/run-tests.sh', ...args, ...selector]);
      // Default cargo test builds examples even when test=false.
      else if (kind === 'example') batches.push(['cargo', 'build', ...args, ...selector]);
      if (target.doctest) batches.push(['bash', 'scripts/run-tests.sh', ...args, '--doc']);
    }
  }
  if (!batches.length) throw new Error('empty test plan');
  return batches;
}

function run(command, args, capture = false) {
  const result = spawnSync(command, args, {
    stdio: capture ? ['ignore', 'pipe', 'inherit'] : 'inherit', encoding: 'utf8',
    env: { ...process.env, CARGO_BUILD_JOBS: '1', RUST_TEST_THREADS: '1' },
  });
  if (result.error || result.status !== 0) {
    throw Object.assign(new Error(`${command} failed (${result.signal ?? result.status}): ${result.error?.message ?? args.join(' ')}`),
      { exitCode: result.status || 1 });
  }
  return result.stdout;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const args = process.argv.slice(2);
    if (args.length && (args.length !== 1 || args[0] !== '--plan')) throw new Error('usage: node scripts/certify-rust-batches.mjs [--plan]');
    const plan = planBatches(JSON.parse(run('cargo', ['metadata', '--no-deps', '--format-version', '1', '--locked'], true)));
    if (args[0] === '--plan') console.log(JSON.stringify(plan, null, 2));
    else {
      for (const [index, [command, ...argv]] of plan.entries()) {
        console.log(`\n==> Rust batch ${index + 1}/${plan.length}: ${command} ${argv.join(' ')}`);
        run(command, argv);
      }
      console.log(`Rust batches: GREEN (${plan.length}/${plan.length})`);
    }
  } catch (error) {
    console.error(`certify-rust-batches: ${error.message}`);
    process.exitCode = error.exitCode || 1;
  }
}
