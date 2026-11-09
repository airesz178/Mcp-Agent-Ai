import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, stat, rm, readdir } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { pruneOldReleases, pruneDuplicateReleases } from '../scripts/release-cleanup.mjs';

test('Android legacy cleanup retains the current/newer APK and unrelated files', async t => {
  const releaseDir = await mkdtemp(path.join(os.tmpdir(), 'mcp-agent-android-cleanup-'));
  t.after(() => rm(releaseDir, { recursive: true, force: true }));
  const keep = [
    'Mcp-Agent-Ai-0.2.2-preview-Android-debug.apk',
    'Mcp-Agent-Ai-0.2.2-preview-Android-debug.apk.sha256',
    'Mcp-Agent-Ai-0.3.0-preview-Android-debug.apk',
    'Mcp-Agent-Ai-0.2.2-rc1-Android-debug.apk',
    'notes.txt',
    'user-created-backup.apk',
  ];
  const obsolete = [
    'Mcp-Agent-Ai-0.2.0-preview-Android-debug.apk',
    'Mcp-Agent-Ai-0.2.0-preview-Android-debug.apk.sha256',
    'COS-Fusion-0.2.1-preview-Android-debug.apk',
  ];
  for (const item of [...keep, ...obsolete]) await writeFile(path.join(releaseDir, item), 'synthetic');
  const removed = await pruneOldReleases({ releaseDir, platform: 'android', version: '0.2.2-preview' });
  assert.deepEqual(removed.sort(), obsolete.sort());
  for (const item of keep) await stat(path.join(releaseDir, item));
  for (const item of obsolete) await assert.rejects(stat(path.join(releaseDir, item)), { code: 'ENOENT' });
});

test('Windows distribution is no longer an accepted release platform', async t => {
  const releaseDir = await mkdtemp(path.join(os.tmpdir(), 'mcp-agent-no-windows-'));
  t.after(() => rm(releaseDir, { recursive: true, force: true }));
  const keep = path.join(releaseDir, 'protected.bin');
  await writeFile(keep, 'user-owned');
  for (const platform of ['windows', 'linux', 'macos']) {
    await assert.rejects(pruneOldReleases({ releaseDir, platform, version: '0.2.2-preview' }), /Unknown release platform/);
    await assert.rejects(pruneDuplicateReleases({ releaseDir, platform }), /Only Android/);
  }
  await assert.rejects(pruneOldReleases({ releaseDir, platform: 'android', version: '../../' }), /Invalid release version/);
  assert.equal((await stat(keep)).size, 10);
});

test('Android canonical cleanup removes known duplicate APKs, retains unknown assets', async t => {
  const folder = await mkdtemp(path.join(os.tmpdir(), 'mcp-android-release-'));
  t.after(() => rm(folder, { recursive: true, force: true }));
  const releaseDir = path.join(folder, 'release');
  await mkdir(releaseDir);
  const apk = 'Mcp-Agent-Ai-Android.apk';
  await writeFile(path.join(releaseDir, apk), Buffer.alloc(2048));
  const obsolete = [
    'Mcp-Agent-Ai-0.2.2-preview-Android-debug.apk',
    'Mcp-Agent-Ai-0.2.2-preview-Tauri-Android-arm64-debug.apk',
    'Mcp-Agent-Ai-0.2.2-preview-Tauri-Android-arm64-debug.apk.sha256',
  ];
  for (const name of [...obsolete, 'personal-file.txt']) {
    await writeFile(path.join(releaseDir, name), 'temporary');
  }
  const removed = await pruneDuplicateReleases({ releaseDir, platform: 'android' });
  assert.deepEqual(removed.sort(), obsolete.sort());
  assert.deepEqual((await readdir(releaseDir)).sort(), [apk, 'personal-file.txt'].sort());
});

test('Android canonical cleanup fails closed before a real APK exists', async t => {
  const folder = await mkdtemp(path.join(os.tmpdir(), 'mcp-android-guard-'));
  t.after(() => rm(folder, { recursive: true, force: true }));
  const releaseDir = path.join(folder, 'release');
  await mkdir(releaseDir);
  const oldFile = 'Mcp-Agent-Ai-0.2.1-preview-Android-debug.apk';
  await writeFile(path.join(releaseDir, oldFile), 'historical');
  await assert.rejects(pruneDuplicateReleases({ releaseDir, platform: 'android' }));
  await stat(path.join(releaseDir, oldFile));
  await writeFile(path.join(releaseDir, 'Mcp-Agent-Ai-Android.apk'), Buffer.alloc(2048));
  const protectedFolder = 'Mcp-Agent-Ai-0.2.2-preview-Tauri-Android-arm64-debug.apk';
  await mkdir(path.join(releaseDir, protectedFolder));
  await writeFile(path.join(releaseDir, protectedFolder, 'notes.txt'), 'preserved');
  await pruneDuplicateReleases({ releaseDir, platform: 'android' });
  await stat(path.join(releaseDir, protectedFolder, 'notes.txt'));
});
