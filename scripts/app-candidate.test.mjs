// Hermetic: synthetic signed bytes, fake Git/GitHub; no builds or publication.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const scripts = path.dirname(fileURLToPath(import.meta.url));
const commit = '1'.repeat(40);
const digest = (bytes) => createHash('sha256').update(bytes).digest('hex');
function run(command, args, options = {}) {
  return spawnSync(command, args, { encoding: 'utf8', ...options });
}
function ok(result) { assert.equal(result.status, 0, result.stdout + result.stderr); }
function fixture(t, target = 'all') {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'app-candidate-test-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const source = path.join(root, 'source');
  const candidate = path.join(root, 'candidate');
  fs.mkdirSync(source);
  const url = (name) => `https://github.com/owner/repo/releases/download/apps-stable/${name}`;
  const assets = {
    'orgasmic_darwin_aarch64.dmg': 'signed dmg',
    'orgasmic.app.tar.gz': 'signed updater',
    'orgasmic.app.tar.gz.sig': 'updater signature',
    'orgasmic_android_aarch64.apk': 'signed apk with fixed versionCode',
  };
  assets['latest.json'] = JSON.stringify({ version: '0.0.13', platforms: { 'darwin-aarch64': {
    url: url('orgasmic.app.tar.gz'), signature: assets['orgasmic.app.tar.gz.sig'],
  } } });
  assets['android-latest.json'] = JSON.stringify({ version: '0.0.13', channel: 'apps-stable',
    packageName: 'com.theaspirational.orgasmic', versionCode: 1234,
    apkUrl: url('orgasmic_android_aarch64.apk'), apkSha256: digest(assets['orgasmic_android_aarch64.apk']),
  });
  for (const [name, bytes] of Object.entries(assets)) fs.writeFileSync(path.join(source, name), bytes);
  const create = () => run(process.execPath, [path.join(scripts, 'app-candidate.mjs'), 'create', candidate, source,
    'owner/repo', 'apps-stable', 'stable', '0.0.13', commit, target]);
  const verify = () => run(process.execPath, [path.join(scripts, 'app-candidate.mjs'), 'verify', candidate]);
  ok(create());
  return { root, source, candidate, assets, create, verify };
}
function rewrite(file, value) { fs.chmodSync(file, 0o644); fs.writeFileSync(file, value); }

test('candidate freezes exact signed assets and manifests, refuses replacement', (t) => {
  const f = fixture(t);
  ok(f.verify());
  for (const [name, bytes] of Object.entries(f.assets)) assert.equal(fs.readFileSync(path.join(f.candidate, name), 'utf8'), bytes);
  fs.writeFileSync(path.join(f.source, 'orgasmic.app.tar.gz'), 'rebuilt');
  assert.notEqual(f.create().status, 0);
  assert.equal(fs.readFileSync(path.join(f.candidate, 'orgasmic.app.tar.gz'), 'utf8'), 'signed updater');
});

for (const name of ['orgasmic.app.tar.gz', 'latest.json', 'orgasmic.app.tar.gz.sig', 'orgasmic_android_aarch64.apk', 'candidate.json']) {
  test(`rejects tampered ${name}`, (t) => {
    const f = fixture(t);
    rewrite(path.join(f.candidate, name), 'tampered');
    assert.notEqual(f.verify().status, 0);
    assert.match(f.verify().stderr, /checksum mismatch/);
  });
}
test('rejects missing assets, symlinks and unlisted files', (t) => {
  const f = fixture(t);
  const name = 'orgasmic.app.tar.gz';
  fs.unlinkSync(path.join(f.candidate, name));
  assert.notEqual(f.verify().status, 0);
  fs.symlinkSync(path.join(f.source, name), path.join(f.candidate, name));
  assert.match(f.verify().stderr, /not a regular file/);
  fs.unlinkSync(path.join(f.candidate, name));
  fs.copyFileSync(path.join(f.source, name), path.join(f.candidate, name));
  fs.writeFileSync(path.join(f.candidate, 'unreviewed.apk'), 'extra');
  assert.match(f.verify().stderr, /unexpected candidate files/);
});
test('rejects unsafe metadata even with a matching receipt checksum', (t) => {
  const f = fixture(t);
  const file = path.join(f.candidate, 'candidate.json');
  const receipt = JSON.parse(fs.readFileSync(file));
  receipt.files['../outside'] = 'a'.repeat(64);
  const bytes = JSON.stringify(receipt);
  rewrite(file, bytes);
  rewrite(`${file}.sha256`, digest(bytes));
  assert.match(f.verify().stderr, /wrong asset inventory/);
});

function publisher(f) {
  const dir = path.join(f.root, 'scripts');
  const bin = path.join(f.root, 'bin');
  fs.mkdirSync(dir); fs.mkdirSync(bin);
  for (const name of ['publish-apps.sh', 'app-candidate.mjs', 'assert-ci-certified.sh']) fs.copyFileSync(path.join(scripts, name), path.join(dir, name));
  for (const name of ['sync-release-metadata.sh', 'refresh-release-publication.sh']) {
    fs.writeFileSync(path.join(dir, name), 'echo metadata >> "$TEST_LOG"\n');
  }
  const shim = `#!${process.execPath}
import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
const name = path.basename(process.argv[1]), args = process.argv.slice(2);
if (name === 'git') {
  if (args[0] === 'status') { if (process.env.STATUS_FAIL) process.exit(1); process.stdout.write(process.env.DIRTY || ''); }
  else if (args[0] === 'rev-parse') console.log(process.env.WRONG_HEAD ? '2'.repeat(40) : '${commit}');
  else if (args[0] === 'symbolic-ref') console.log('main');
  else if (args[0] === 'fetch') process.exit(process.env.FETCH_FAIL ? 1 : 0);
  else process.exit(88);
} else if (name === 'gh') {
  if (args[0] === 'api') {
    if (process.env.MUTATE_CANDIDATE) {
      const file = path.join(process.env.MUTATE_CANDIDATE, 'orgasmic.app.tar.gz');
      fs.chmodSync(file, 0o644); fs.writeFileSync(file, 'changed during certification');
    }
    console.log(JSON.stringify({sha: '${commit}', statuses: [{context: 'local/release-certified', state: process.env.UNCERTIFIED ? 'failure' : 'success'}]}));
  } else if (args[0] === 'release' && args[1] === 'upload') {
    const assets = args.slice(5, -1).map(file => ({ name: path.basename(file), hash: createHash('sha256').update(fs.readFileSync(file)).digest('hex') }));
    fs.appendFileSync(process.env.TEST_LOG, JSON.stringify(assets) + '\\n');
    if (process.env.UPLOAD_FAIL) process.exit(1);
  } else process.exit(89);
} else { fs.appendFileSync(process.env.TEST_LOG, 'BUILD INVOKED\\n'); process.exit(90); }
`;
  fs.writeFileSync(path.join(bin, 'shim.mjs'), shim, { mode: 0o755 });
  for (const name of ['git', 'gh', 'npm', 'cargo', 'rustc', 'rustup']) fs.symlinkSync('shim.mjs', path.join(bin, name));
  const log = path.join(f.root, 'calls');
  return { log, run: (args = [], extra = {}) => run('bash', [path.join(dir, 'publish-apps.sh'), ...args], {
    cwd: f.root, env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, TEST_LOG: log, ...extra },
  }) };
}
for (const target of ['mac', 'android', 'all']) {
  test(`promotion uploads only frozen ${target} bytes without invoking builders`, (t) => {
    const f = fixture(t, target), p = publisher(f);
    ok(p.run(['--candidate', f.candidate, '--dry-run']));
    assert.equal(fs.existsSync(p.log), false);
    ok(p.run(['--candidate', f.candidate]));
    const lines = fs.readFileSync(p.log, 'utf8').trim().split('\n');
    assert.equal(lines[0], 'metadata'); assert.equal(lines[2], 'metadata'); assert.equal(lines.length, 3);
    const receipt = JSON.parse(fs.readFileSync(path.join(f.candidate, 'candidate.json')));
    assert.deepEqual(Object.fromEntries(JSON.parse(lines[1]).map(a => [a.name, a.hash])), receipt.files);
  });
}
test('promotion fails before mutation on dirty/wrong/unpushed/uncertified source or overrides', (t) => {
  const f = fixture(t), p = publisher(f);
  for (const env of [{DIRTY: ' M source'}, {WRONG_HEAD: '1'}, {FETCH_FAIL: '1'}, {UNCERTIFIED: '1'}, {STATUS_FAIL: '1'}]) {
    assert.notEqual(p.run(['--candidate', f.candidate], env).status, 0);
    assert.equal(fs.existsSync(p.log), false);
  }
  assert.notEqual(p.run(['--candidate', f.candidate, '--channel', 'nightly']).status, 0);
  assert.match(p.run(['--channel', 'stable']).stderr, /requires --candidate/);
  assert.equal(fs.existsSync(p.log), false);
});
test('changes to the original candidate during certification cannot change uploaded bytes', (t) => {
  const f = fixture(t), p = publisher(f);
  const receipt = JSON.parse(fs.readFileSync(path.join(f.candidate, 'candidate.json')));
  ok(p.run(['--candidate', f.candidate], {MUTATE_CANDIDATE: f.candidate}));
  const upload = JSON.parse(fs.readFileSync(p.log, 'utf8').trim().split('\n')[1]);
  assert.deepEqual(Object.fromEntries(upload.map(a => [a.name, a.hash])), receipt.files);
  assert.notEqual(f.verify().status, 0);
});
test('upload failure is non-green and does not refresh publication', (t) => {
  const f = fixture(t), p = publisher(f);
  assert.notEqual(p.run(['--candidate', f.candidate], {UPLOAD_FAIL: '1'}).status, 0);
  assert.equal(fs.readFileSync(p.log, 'utf8').trim().split('\n').length, 2);
});
