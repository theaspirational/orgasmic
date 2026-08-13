#!/usr/bin/env node

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

const usage = () => {
  console.error(`Usage:
  node scripts/runtime-candidate.mjs create --source-dir <dir> --candidate-dir <dir>
    --existing-manifest <file> --repo <owner/name> --tag <tag> --channel <channel>
    --version <version> --commit <sha> --tree <sha> --toolchain <version>
    --certification-context <context> --codesign-requirement <requirement>
  node scripts/runtime-candidate.mjs verify --candidate-dir <dir>
  node scripts/runtime-candidate.mjs validate-version --manifest <file>
    --channel <channel> --version <version> --targets <target[,target...]>
  node scripts/runtime-candidate.mjs hash --file <file>
  node scripts/runtime-candidate.mjs stamp-workspace-version --cargo-toml <file>
    --cargo-lock <file> --version <version>
  node scripts/runtime-candidate.mjs next-patch <version> [<version> ...]`);
};

const die = (message) => {
  console.error(`runtime-candidate: ${message}`);
  process.exit(1);
};

const parseOptions = (args) => {
  const options = {};
  for (let index = 0; index < args.length; index += 2) {
    const name = args[index];
    const value = args[index + 1];
    if (!name?.startsWith('--') || value === undefined) die(`invalid option list near '${name ?? ''}'`);
    options[name.slice(2)] = value;
  }
  return options;
};

const required = (options, name) => {
  const value = options[name];
  if (!value) die(`--${name} is required`);
  return value;
};

const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');
const hashFile = (file) => sha256(fs.readFileSync(file));
const readJson = (file, label) => {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch (error) {
    die(`cannot read ${label} ${file}: ${error.message}`);
  }
};

const safeVersion = (version) => {
  if (!/^[0-9A-Za-z][0-9A-Za-z.+-]*$/.test(version)) die(`unsafe version '${version}'`);
  return version;
};

const semverParts = (version) => {
  const match = String(version).replace(/^v/, '').match(/^(\d+)\.(\d+)\.(\d+)(?:[-+].*)?$/);
  return match ? match.slice(1).map(Number) : null;
};

const compareVersions = (left, right) => {
  const a = semverParts(left);
  const b = semverParts(right);
  if (!a || !b) die(`cannot compare versions '${left}' and '${right}'`);
  for (let index = 0; index < 3; index += 1) {
    if (a[index] !== b[index]) return a[index] - b[index];
  }
  return 0;
};

const targetFromLegacyAsset = (filename) => {
  const match = filename.match(/^orgasmic-runtime_([a-z0-9]+)_(.+)\.tar\.gz$/);
  return match ? `${match[1]}-${match[2]}` : null;
};

const immutableAssetName = (version, legacyName, digest) => {
  const suffix = legacyName.slice('orgasmic-runtime_'.length, -'.tar.gz'.length);
  return `orgasmic-runtime_${safeVersion(version)}_${suffix}_${digest.slice(0, 12)}.tar.gz`;
};

const create = (options) => {
  const sourceDir = path.resolve(required(options, 'source-dir'));
  const candidateDir = path.resolve(required(options, 'candidate-dir'));
  const existingFile = path.resolve(required(options, 'existing-manifest'));
  const repo = required(options, 'repo');
  const tag = required(options, 'tag');
  const channel = required(options, 'channel');
  const version = safeVersion(required(options, 'version'));
  const commit = required(options, 'commit');
  const tree = required(options, 'tree');
  const toolchain = required(options, 'toolchain');
  const certificationContext = required(options, 'certification-context');
  const codesignRequirement = required(options, 'codesign-requirement');

  if (fs.existsSync(candidateDir)) die(`candidate already exists: ${candidateDir}`);
  const existingBytes = fs.readFileSync(existingFile);
  let manifest;
  try {
    manifest = JSON.parse(existingBytes.toString('utf8')) || {};
  } catch (error) {
    die(`existing manifest is invalid JSON: ${error.message}`);
  }
  if (!manifest.runtimes || typeof manifest.runtimes !== 'object' || Array.isArray(manifest.runtimes)) {
    manifest.runtimes = {};
  }
  if (channel === 'stable') {
    for (const [target, entry] of Object.entries(manifest.runtimes)) {
      if (!entry || typeof entry.version !== 'string' || entry.version.length === 0) {
        die(`existing stable target ${target} has no per-target version`);
      }
    }
  }

  fs.mkdirSync(candidateDir, { recursive: true });
  const artifacts = [];
  const legacyAssets = fs.readdirSync(sourceDir)
    .filter((file) => file.startsWith('orgasmic-runtime_') && file.endsWith('.tar.gz'))
    .sort();
  if (legacyAssets.length === 0) die(`no runtime tarballs found in ${sourceDir}`);

  for (const legacyName of legacyAssets) {
    const target = targetFromLegacyAsset(legacyName);
    if (!target) die(`cannot derive target from ${legacyName}`);
    const previousVersion = manifest.runtimes[target]?.version;
    if (channel === 'stable' && previousVersion && compareVersions(version, previousVersion) <= 0) {
      die(`stable ${target} version ${version} must be newer than ${previousVersion}`);
    }
    const source = path.join(sourceDir, legacyName);
    const digest = hashFile(source);
    const sidecar = `${source}.sha256`;
    if (fs.existsSync(sidecar)) {
      const recorded = fs.readFileSync(sidecar, 'utf8').trim().split(/\s+/)[0];
      if (recorded.toLowerCase() !== digest) die(`source checksum mismatch for ${legacyName}`);
    }
    const filename = immutableAssetName(version, legacyName, digest);
    fs.copyFileSync(source, path.join(candidateDir, filename), fs.constants.COPYFILE_EXCL);
    fs.writeFileSync(path.join(candidateDir, `${filename}.sha256`), `${digest}  ${filename}\n`, { flag: 'wx' });
    const url = `https://github.com/${repo}/releases/download/${tag}/${filename}`;
    manifest.runtimes[target] = { url, sha256: digest, version, commit };
    artifacts.push({ target, filename, sha256: digest, version, url });
  }

  manifest.version = version;
  manifest.channel = channel;
  manifest.commit = commit;
  manifest.pub_date = new Date().toISOString();
  const proposedManifest = `${JSON.stringify(manifest, null, 2)}\n`;
  fs.writeFileSync(path.join(candidateDir, 'runtime-latest.json'), proposedManifest, { flag: 'wx' });

  const candidate = {
    schemaVersion: 1,
    channel,
    tag,
    version,
    commit,
    tree,
    repo,
    toolchain,
    certificationContext,
    codesignRequirement,
    existingManifestSha256: sha256(existingBytes),
    proposedManifestSha256: sha256(Buffer.from(proposedManifest)),
    createdAt: new Date().toISOString(),
    artifacts,
  };
  const candidateBytes = `${JSON.stringify(candidate, null, 2)}\n`;
  fs.writeFileSync(path.join(candidateDir, 'candidate.json'), candidateBytes, { flag: 'wx' });
  fs.writeFileSync(
    path.join(candidateDir, 'candidate.json.sha256'),
    `${sha256(Buffer.from(candidateBytes))}  candidate.json\n`,
    { flag: 'wx' },
  );
  console.log(JSON.stringify(candidate));
};

const verify = (options) => {
  const candidateDir = path.resolve(required(options, 'candidate-dir'));
  const candidateFile = path.join(candidateDir, 'candidate.json');
  const recordedCandidateHash = fs.readFileSync(`${candidateFile}.sha256`, 'utf8').trim().split(/\s+/)[0];
  if (hashFile(candidateFile) !== recordedCandidateHash) die('candidate metadata checksum mismatch');
  const candidate = readJson(candidateFile, 'candidate');
  if (candidate.schemaVersion !== 1) die(`unsupported candidate schema ${candidate.schemaVersion}`);
  for (const field of [
    'channel', 'tag', 'version', 'commit', 'tree', 'repo', 'toolchain',
    'certificationContext', 'codesignRequirement', 'existingManifestSha256',
    'proposedManifestSha256',
  ]) {
    if (typeof candidate[field] !== 'string' || candidate[field].length === 0) {
      die(`candidate field '${field}' is missing`);
    }
  }
  if (!Array.isArray(candidate.artifacts) || candidate.artifacts.length === 0) {
    die('candidate has no artifacts');
  }
  const proposedFile = path.join(candidateDir, 'runtime-latest.json');
  if (hashFile(proposedFile) !== candidate.proposedManifestSha256) {
    die('proposed manifest checksum mismatch');
  }
  const manifest = readJson(proposedFile, 'proposed manifest');
  for (const artifact of candidate.artifacts) {
    if (!artifact.filename || path.basename(artifact.filename) !== artifact.filename) {
      die('candidate contains an unsafe artifact filename');
    }
    const artifactFile = path.join(candidateDir, artifact.filename);
    const digest = hashFile(artifactFile);
    if (digest !== artifact.sha256) die(`artifact checksum mismatch for ${artifact.filename}`);
    const sidecar = fs.readFileSync(`${artifactFile}.sha256`, 'utf8').trim().split(/\s+/)[0];
    if (sidecar !== digest) die(`checksum sidecar mismatch for ${artifact.filename}`);
    const entry = manifest.runtimes?.[artifact.target];
    if (!entry || entry.sha256 !== digest || entry.version !== candidate.version || entry.url !== artifact.url) {
      die(`manifest does not describe candidate artifact ${artifact.target}`);
    }
  }
  console.log(JSON.stringify(candidate));
};

const validateVersion = (options) => {
  const manifest = readJson(path.resolve(required(options, 'manifest')), 'manifest');
  const channel = required(options, 'channel');
  const version = required(options, 'version');
  const targets = required(options, 'targets').split(',').filter(Boolean);
  if (targets.length === 0) die('no targets supplied for version validation');
  if (channel !== 'stable') return;
  for (const target of targets) {
    const previousVersion = manifest.runtimes?.[target]?.version;
    if (previousVersion && compareVersions(version, previousVersion) <= 0) {
      die(`stable ${target} version ${version} must be newer than ${previousVersion}`);
    }
  }
};

const nextPatch = (versions) => {
  if (versions.length === 0) die('next-patch requires at least one version');
  let maximum = [0, 0, 0];
  for (const version of versions) {
    const parts = semverParts(version);
    if (!parts) die(`cannot derive patch version from '${version}'`);
    if (parts.some((part, index) => part > maximum[index] && parts.slice(0, index).every((p, i) => p === maximum[i]))) {
      maximum = parts;
    }
  }
  maximum[2] += 1;
  console.log(maximum.join('.'));
};

const stampWorkspaceVersion = (options) => {
  const manifestFile = path.resolve(required(options, 'cargo-toml'));
  const lockFile = path.resolve(required(options, 'cargo-lock'));
  const version = safeVersion(required(options, 'version'));
  const manifest = fs.readFileSync(manifestFile, 'utf8');
  const packageSection = /(\[workspace\.package\][\s\S]*?\nversion = ")[^"]+("[\s\S]*?)(?=\n\[|$)/;
  if (!packageSection.test(manifest)) die('Cargo.toml has no workspace package version');
  fs.writeFileSync(manifestFile, manifest.replace(packageSection, `$1${version}$2`));

  let lock = fs.readFileSync(lockFile, 'utf8');
  const workspacePackages = ['orgasmic-core', 'orgasmic-daemon', 'orgasmic-cli', 'orgasmic-drivers'];
  for (const packageName of workspacePackages) {
    const escaped = packageName.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const packageVersion = new RegExp(`(\\[\\[package\\]\\]\\nname = "${escaped}"\\nversion = ")[^"]+(")`);
    if (!packageVersion.test(lock)) die(`Cargo.lock has no workspace package ${packageName}`);
    lock = lock.replace(packageVersion, `$1${version}$2`);
  }
  fs.writeFileSync(lockFile, lock);
};

const [command, ...rest] = process.argv.slice(2);
switch (command) {
  case 'create': create(parseOptions(rest)); break;
  case 'verify': verify(parseOptions(rest)); break;
  case 'validate-version': validateVersion(parseOptions(rest)); break;
  case 'hash': console.log(hashFile(required(parseOptions(rest), 'file'))); break;
  case 'next-patch': nextPatch(rest); break;
  case 'stamp-workspace-version': stampWorkspaceVersion(parseOptions(rest)); break;
  default: usage(); process.exit(2);
}
