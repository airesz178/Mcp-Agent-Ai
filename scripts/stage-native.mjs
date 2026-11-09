import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { copyFile, lstat, mkdir, rename, rm, stat, open } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { pruneDuplicateReleases } from './release-cleanup.mjs';

// Android-only: one stable APK, no Windows product distribution.
if (process.platform !== 'win32') throw new Error('Windows build host required');
const platform = process.argv[2];
if (platform !== 'android') throw new Error('Only Android APK staging is supported');
const root = fileURLToPath(new URL('..', import.meta.url));
const releaseDir = path.join(root, 'release');
const source = path.join(root, 'src-tauri', 'gen', 'android', 'app', 'build', 'outputs', 'apk', 'universal', 'debug', 'app-universal-debug.apk');
const name = 'Mcp-Agent-Ai-Android.apk';
const target = path.join(releaseDir, name);
const staged = path.join(releaseDir, `.${name}.staging`);
if ((await stat(source)).size < 1024 * 1024) throw new Error('Android APK missing or suspiciously small');
const handle = await open(source, 'r');
try {
  const magic = Buffer.alloc(4);
  await handle.read(magic, 0, 4, 0);
  if (magic.toString('ascii', 0, 2) !== 'PK')
    throw new Error('Generated artifact is not an APK ZIP container');
} finally {
  await handle.close();
}
await mkdir(releaseDir, { recursive: true });
async function hash(file) {
  const digest = createHash('sha256');
  for await (const chunk of createReadStream(file)) digest.update(chunk);
  return digest.digest('hex').toUpperCase();
}
if (await lstat(staged).then(() => true, () => false)) throw new Error('A previous staging file is present; refusing overwrite');
try {
  await copyFile(source, staged);
  const expected = await hash(source);
  if ((await hash(staged)) !== expected) throw new Error('Staged APK checksum differs from build');
  // The last verified Android APK stays untouched if staging or rename fails.
  await rename(staged, target);
  if ((await hash(target)) !== expected) throw new Error('Staged Android release checksum mismatch');
  const removed = await pruneDuplicateReleases({ releaseDir, platform: 'android' });
  // The Gradle-generated copy is redundant after verified release staging.
  if ((await hash(source)) === expected) await rm(source);
  console.log(`${platform}: ${name} — SHA256 ${expected}; removed ${removed.length} old release artifact(s)`);
} finally {
  // This file was absent before this invocation. Do not leave a huge .staging
  // copy behind when verification/rename fails.
  await rm(staged, { force: true });
}
