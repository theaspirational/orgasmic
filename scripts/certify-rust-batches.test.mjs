// Hermetic orchestration checks: fake Cargo/classifier, real temporary Git repo.
// No Rust builds, network access, certification of this checkout or publication.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { planBatches } from './certify-rust-batches.mjs';

const scripts = path.dirname(fileURLToPath(import.meta.url));
const target = (kind, name, test = true, doctest = false) => ({ kind: [kind], name, test, doctest });
const metadata = { workspace_members: ['cli', 'core'], packages: [
  { id: 'cli', name: 'orgasmic-cli', targets: [target('bin', 'orgasmic'), target('test', 'dispatch'), target('test', 'plugin_cli')] },
  { id: 'core', name: 'orgasmic-core', targets: [target('lib', 'orgasmic_core', true, true),
    target('example', 'demo', false), target('example', 'tested'), target('bench', 'bench'), target('custom-build', 'build', false)] },
  { id: 'dependency', name: 'dependency', targets: [target('lib', 'dependency')] },
] };

test('plans every default target, doctests and build-only examples without dependency tests', () => {
  const testArgs = (pkg, ...args) => ['bash', 'scripts/run-tests.sh', '--locked', '-p', pkg, ...args];
  assert.deepEqual(planBatches(metadata), [
    testArgs('orgasmic-cli', '--bin', 'orgasmic'), testArgs('orgasmic-cli', '--test', 'dispatch'),
    testArgs('orgasmic-cli', '--test', 'plugin_cli'), testArgs('orgasmic-core', '--lib'),
    testArgs('orgasmic-core', '--doc'), ['cargo', 'build', '--locked', '-p', 'orgasmic-core', '--example', 'demo'],
    testArgs('orgasmic-core', '--example', 'tested'),
  ]);
  assert.throws(() => planBatches({ workspace_members: [] }), /empty workspace/);
  assert.throws(() => planBatches({ workspace_members: ['missing'], packages: [] }), /missing targets/);
  for (const extra of [{ kind: ['new-kind'] }, { 'required-features': ['optional'] }]) {
    const changed = structuredClone(metadata);
    Object.assign(changed.packages[0].targets[0], extra);
    assert.throws(() => planBatches(changed), /unsupported|coverage decision/);
  }
});

test('full certification runs UI, MSRV and app checks before the long Rust suite', () => {
  const source = fs.readFileSync(path.join(scripts, 'certify-release.sh'), 'utf8');
  const rust = source.indexOf('Classified Rust suite');
  for (const early of ['UI tests', 'Workspace MSRV', 'Tauri application check']) {
    assert.ok(source.indexOf(early) < rust, `${early} must run before classified Rust`);
  }
});

test('certification reuses UI and MSRV build work without dropping checks', () => {
  const certification = fs.readFileSync(path.join(scripts, 'certify-release.sh'), 'utf8');
  const appPublisher = fs.readFileSync(path.join(scripts, 'publish-apps.sh'), 'utf8');
  const uiPackage = JSON.parse(fs.readFileSync(path.join(scripts, '../ui/package.json'), 'utf8'));
  assert.match(certification, /target\/certification-msrv-/);
  assert.doesNotMatch(certification, /orgasmic-msrv\.XXXXXX/);
  assert.equal(uiPackage.scripts['build:vite'], 'vite build');
  assert.match(uiPackage.scripts.build, /typecheck/);
  assert.equal((appPublisher.match(/run build:bootstrap\n/g) || []).length, 1);
  assert.equal((appPublisher.match(/--config "\$TAURI_PREBUILT_CONFIG"/g) || []).length, 2);
});

function run(command, args, options = {}) {
  return spawnSync(command, args, { encoding: 'utf8', ...options });
}
function ok(result) { assert.equal(result.status, 0, result.stdout + result.stderr); }
function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'certify-batches-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const repo = path.join(root, 'repo'), bin = path.join(root, 'bin'), log = path.join(root, 'calls');
  fs.mkdirSync(path.join(repo, 'scripts'), { recursive: true }); fs.mkdirSync(bin);
  for (const name of ['certify-rust-batches.mjs', 'certify-pr.sh', 'certify-runtime-fast.sh', 'assert-ci-certified.sh']) {
    fs.copyFileSync(path.join(scripts, name), path.join(repo, 'scripts', name));
  }
  fs.writeFileSync(path.join(root, 'metadata.json'), JSON.stringify(metadata));
  fs.writeFileSync(path.join(bin, 'cargo'), `#!${process.execPath}
const fs = require('node:fs'), path = require('node:path');
const root = process.env.BATCH_FIXTURE, args = process.argv.slice(2);
if (args[0] === 'metadata') { console.log(fs.readFileSync(path.join(root, 'metadata.json'), 'utf8')); process.exit(0); }
if (process.env.CARGO_BUILD_JOBS !== '1' || process.env.RUST_TEST_THREADS !== '1') process.exit(98);
const lock = path.join(root, 'active'); fs.mkdirSync(lock);
fs.appendFileSync(path.join(root, 'calls'), JSON.stringify(args) + '\\n');
// Leave the lock present until this child exits: overlapping batches fail.
Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 15);
fs.rmdirSync(lock);
if (args.includes(process.env.FAIL_TARGET)) process.exit(Number(process.env.FAIL_CODE || 4));
`, { mode: 0o755 });
  fs.writeFileSync(path.join(repo, 'scripts/run-tests.sh'), 'exec cargo test "$@"\n');
  fs.writeFileSync(path.join(repo, 'scripts/certify-release.sh'), `set -e
CERTIFICATION_RUST="1.97.1"
MSRV_RUST="1.88.0"
node scripts/certify-rust-batches.mjs
case "$SOURCE_MUTATION" in
  dirty) printf changed >> scripts/certify-rust-batches.mjs ;;
  commit) git commit -qm moved --allow-empty ;;
esac
`);
  fs.writeFileSync(path.join(bin, 'gh'), `#!${process.execPath}
const { spawnSync } = require('node:child_process');
const args = process.argv.slice(2), head = spawnSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).stdout.trim();
if (args[0] !== 'api') process.exit(97);
if (args.includes('--method')) require('node:fs').appendFileSync(process.env.GH_LOG, JSON.stringify(args) + '\\n');
if (args.some(arg => arg.endsWith('/status'))) console.log(JSON.stringify({ sha: head, statuses: [{ context: 'local/release-certified', state: 'success', description: 'profile=full', creator: { login: 'test' } }] }));
`, { mode: 0o755 });
  const ghLog = path.join(root, 'gh-calls');
  const env = { ...process.env, PATH: `${bin}:${process.env.PATH}`, BATCH_FIXTURE: root, GH_LOG: ghLog };
  const invoke = (command, args, extra = {}) => run(command, args, { cwd: repo, env: { ...env, ...extra } });
  return { root, repo, log, invoke,
    calls: () => fs.existsSync(log) ? fs.readFileSync(log, 'utf8').trim().split('\n').map(JSON.parse) : [],
    ghCalls: () => fs.existsSync(ghLog) ? fs.readFileSync(ghLog, 'utf8').trim().split('\n').map(JSON.parse) : [] };
}

test('runtime-fast reuses an exact-commit full certification without rerunning suites', t => {
  const f = fixture(t);
  const git = (...args) => { const result = f.invoke('git', args); ok(result); return result.stdout.trim(); };
  git('init', '-q', '-b', 'main');
  git('config', 'user.name', 'Certification test'); git('config', 'user.email', 'test@example.invalid');
  git('config', 'commit.gpgsign', 'false'); git('config', 'core.hooksPath', '/dev/null');
  git('add', '.'); git('commit', '-qm', 'initial');
  const base = git('rev-parse', 'HEAD');
  fs.writeFileSync(path.join(f.repo, 'feature.txt'), 'already fully certified\n');
  git('add', '.'); git('commit', '-qm', 'feature');
  const result = f.invoke('bash', ['scripts/certify-runtime-fast.sh', '--repo', 'test/repo', '--base', base]);
  ok(result); assert.match(result.stdout, /reusing exact-commit full certification/);
  ok(f.invoke('bash', ['scripts/certify-runtime-fast.sh', '--repo', 'test/repo', '--base', base]));
  assert.deepEqual(f.calls(), []);
  assert.deepEqual(f.ghCalls(), []);
});

test('change certification selects app, runtime and full profiles fail closed', t => {
  for (const [files, expected] of [
    [['src-tauri/capabilities/android.json'], 'apps-fast'],
    [['crates/core/src/lib.rs'], 'runtime-fast'],
    [['src-tauri/capabilities/android.json', 'crates/core/src/lib.rs'], 'full'],
    [['unclassified/tool'], 'full'],
  ]) {
    const f = fixture(t);
    const git = (...args) => { const result = f.invoke('git', args); ok(result); return result.stdout.trim(); };
    git('init', '-q', '-b', 'main');
    git('config', 'user.name', 'Certification test'); git('config', 'user.email', 'test@example.invalid');
    git('config', 'commit.gpgsign', 'false'); git('config', 'core.hooksPath', '/dev/null');
    git('add', '.'); git('commit', '-qm', 'initial');
    const base = git('rev-parse', 'HEAD');
    for (const file of files) {
      const destination = path.join(f.repo, file);
      fs.mkdirSync(path.dirname(destination), { recursive: true });
      fs.writeFileSync(destination, 'change\n');
    }
    git('add', '.'); git('commit', '-qm', 'change');
    const result = f.invoke('bash', ['scripts/certify-runtime-fast.sh', '--base', base, '--plan']);
    ok(result); assert.equal(result.stdout.trim(), expected, files.join(', '));
  }
});

test('runs bounded batches serially and --plan never runs tests or reports green', t => {
  const f = fixture(t);
  const preview = f.invoke(process.execPath, ['scripts/certify-rust-batches.mjs', '--plan']);
  ok(preview); assert.deepEqual(JSON.parse(preview.stdout), planBatches(metadata));
  assert.deepEqual(f.calls(), []);
  const result = f.invoke(process.execPath, ['scripts/certify-rust-batches.mjs']);
  ok(result); assert.match(result.stdout, /GREEN \(7\/7\)/);
  assert.deepEqual(f.calls(), planBatches(metadata).map(([command, , ...args]) => [command === 'bash' ? 'test' : 'build', ...args]));
});

for (const code of [1, 2, 3, 4]) {
  test(`batch exit ${code} stops the plan and cannot report green`, t => {
    const f = fixture(t);
    const result = f.invoke(process.execPath, ['scripts/certify-rust-batches.mjs'], { FAIL_TARGET: 'dispatch', FAIL_CODE: String(code) });
    assert.equal(result.status, code); assert.equal(f.calls().length, 2);
    assert.doesNotMatch(result.stdout, /GREEN/);
  });
}

for (const [name, count] of [['--doc', 5], ['demo', 6]]) {
  test(`failure in ${name} also prevents completion`, t => {
    const f = fixture(t);
    const result = f.invoke(process.execPath, ['scripts/certify-rust-batches.mjs'], { FAIL_TARGET: name, FAIL_CODE: '101' });
    assert.equal(result.status, 101); assert.equal(f.calls().length, count);
    assert.doesNotMatch(result.stdout, /GREEN/);
  });
}

for (const certifier of ['certify-pr.sh', 'certify-runtime-fast.sh']) {
  test(`${certifier}: only complete unchanged trees get reusable receipts`, t => {
    const f = fixture(t);
    const git = (...args) => { const result = f.invoke('git', args); ok(result); return result.stdout.trim(); };
    git('init', '-q', '-b', 'main');
    git('config', 'user.name', 'Certification test'); git('config', 'user.email', 'test@example.invalid');
    git('config', 'commit.gpgsign', 'false'); git('config', 'core.hooksPath', '/dev/null');
    git('add', '.'); git('commit', '-qm', 'initial');
    const base = git('rev-parse', 'HEAD');
    const remote = path.join(f.root, 'remote.git');
    ok(f.invoke('git', ['init', '--bare', '-q', remote]));
    git('remote', 'add', 'origin', remote); git('push', '-q', 'origin', 'main');
    fs.appendFileSync(path.join(f.repo, 'scripts/certify-rust-batches.mjs'), '\n// new gate\n');
    git('add', '.'); git('commit', '-qm', 'gate');
    const head = git('rev-parse', 'HEAD');
    const args = [`scripts/${certifier}`, '--no-publish', ...(certifier.includes('fast') ? ['--base', base] : [])];
    const certify = extra => f.invoke('bash', args, extra);
    const receipts = () => fs.readdirSync(path.join(f.repo, '.git/orgasmic-certifications'), { recursive: true });
    const failed = certify({ FAIL_TARGET: 'dispatch' });
    assert.notEqual(failed.status, 0); assert.doesNotMatch(failed.stdout, /certification: GREEN/);
    assert.equal(fs.existsSync(path.join(f.repo, '.git/orgasmic-certifications')), false);
    for (const mutation of ['dirty', 'commit']) {
      const changed = certify({ SOURCE_MUTATION: mutation });
      assert.notEqual(changed.status, 0); assert.match(changed.stderr, /source changed/);
      assert.equal(fs.existsSync(path.join(f.repo, '.git/orgasmic-certifications')), false);
      // Disposable synthetic repository only.
      git('restore', 'scripts/certify-rust-batches.mjs'); git('checkout', '-q', head);
    }
    ok(certify()); assert.equal(receipts().length, 1);
    const callCount = f.calls().length;
    const reused = certify(); ok(reused); assert.match(reused.stdout, /reusing/);
    assert.equal(f.calls().length, callCount);
    fs.appendFileSync(path.join(f.repo, 'scripts/certify-rust-batches.mjs'), '\n// different tree\n');
    git('add', '.'); git('commit', '-qm', 'change batch inputs');
    ok(certify()); assert.equal(receipts().length, 2);
    assert.equal(f.calls().length, callCount + planBatches(metadata).length);
  });
}
