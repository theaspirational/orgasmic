#!/usr/bin/env node
// Local, checksummed build receipt; not a signature against a malicious operator.
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

const filesFor = (target) => [
  ...(target !== 'android' ? ['latest.json', 'orgasmic_darwin_aarch64.dmg', 'orgasmic.app.tar.gz', 'orgasmic.app.tar.gz.sig'] : []),
  ...(target !== 'mac' ? ['android-latest.json', 'orgasmic_android_aarch64.apk'] : []),
].sort();
const hash = (bytes) => createHash('sha256').update(bytes).digest('hex');
const read = (dir, name) => {
  const file = path.join(dir, name);
  if (!fs.lstatSync(file).isFile()) throw new Error(`not a regular file: ${file}`);
  return fs.readFileSync(file);
};
const check = (condition, message) => { if (!condition) throw new Error(message); };

function validate(dir, receipt) {
  check(receipt.schemaVersion === 1, 'unsupported candidate schema');
  for (const [field, pattern] of Object.entries({
    repo: /^[\w.-]+\/[\w.-]+$/, tag: /^[\w][\w.-]*$/,
    channel: /^(stable|nightly)$/, target: /^(mac|android|all)$/,
    version: /^\d+\.\d+\.\d+(?:[-+][\w.+-]+)?$/, commit: /^[a-f0-9]{40}$/,
  })) check(typeof receipt[field] === 'string' && pattern.test(receipt[field]), `invalid ${field}`);
  const names = filesFor(receipt.target);
  check(JSON.stringify(Object.keys(receipt.files ?? {}).sort()) === JSON.stringify(names), 'wrong asset inventory');
  for (const name of names) check(hash(read(dir, name)) === receipt.files[name], `checksum mismatch: ${name}`);
  const url = (name) => `https://github.com/${receipt.repo}/releases/download/${receipt.tag}/${name}`;
  if (receipt.target !== 'android') {
    const manifest = JSON.parse(read(dir, 'latest.json'));
    check(manifest.version === receipt.version, 'macOS version mismatch');
    const platform = manifest.platforms?.['darwin-aarch64'];
    check(Object.keys(manifest.platforms ?? {}).length === 1 && platform?.url === url('orgasmic.app.tar.gz'), 'macOS target/URL mismatch');
    check(platform.signature === read(dir, 'orgasmic.app.tar.gz.sig').toString().trim() && platform.signature.length > 0, 'macOS signature mismatch');
  }
  if (receipt.target !== 'mac') {
    const manifest = JSON.parse(read(dir, 'android-latest.json'));
    check(manifest.version === receipt.version && manifest.channel === receipt.tag, 'Android version/channel mismatch');
    check(manifest.packageName === 'com.theaspirational.orgasmic' && Number.isSafeInteger(manifest.versionCode) && manifest.versionCode > 0, 'invalid Android package/versionCode');
    check(manifest.apkUrl === url('orgasmic_android_aarch64.apk'), 'Android URL mismatch');
    check(manifest.apkSha256 === receipt.files['orgasmic_android_aarch64.apk'], 'Android digest mismatch');
  }
}

try {
  const [command, dir, ...args] = process.argv.slice(2);
  check(dir, 'usage: app-candidate.mjs create <destination> <source> <repo> <tag> <channel> <version> <commit> <target> | verify <directory>');
  if (command === 'create') {
    check(args.length === 7, 'invalid create arguments');
    const [source, repo, tag, channel, version, commit, target] = args;
    const receipt = { schemaVersion: 1, repo, tag, channel, version, commit, target, files: {} };
    for (const name of filesFor(target)) receipt.files[name] = hash(read(source, name));
    validate(source, receipt);
    // mkdir without recursive on the final component refuses every overwrite.
    fs.mkdirSync(path.dirname(path.resolve(dir)), { recursive: true });
    fs.mkdirSync(dir);
    for (const name of filesFor(target)) {
      fs.copyFileSync(path.join(source, name), path.join(dir, name), fs.constants.COPYFILE_EXCL);
    }
    validate(dir, receipt);
    const bytes = JSON.stringify(receipt, null, 2) + '\n';
    fs.writeFileSync(path.join(dir, 'candidate.json'), bytes, { flag: 'wx' });
    fs.writeFileSync(path.join(dir, 'candidate.json.sha256'), hash(bytes) + '\n', { flag: 'wx' });
    for (const name of fs.readdirSync(dir)) fs.chmodSync(path.join(dir, name), 0o444);
    console.log(path.resolve(dir));
  } else if (command === 'verify') {
    check(args.length === 0, 'invalid verify arguments');
    const bytes = read(dir, 'candidate.json');
    check(hash(bytes) === read(dir, 'candidate.json.sha256').toString().trim(), 'candidate metadata checksum mismatch');
    const receipt = JSON.parse(bytes);
    validate(dir, receipt);
    check(JSON.stringify(fs.readdirSync(dir).sort()) === JSON.stringify([...filesFor(receipt.target), 'candidate.json', 'candidate.json.sha256'].sort()), 'unexpected candidate files');
    console.log([receipt.repo, receipt.tag, receipt.channel, receipt.version, receipt.commit, receipt.target].join('\t'));
  } else throw new Error('unknown command');
} catch (error) {
  console.error(`app-candidate: ${error.message}`);
  process.exitCode = 1;
}
