import { lstat, readdir, rm } from 'node:fs/promises';
import path from 'node:path';

const artifactPatterns = Object.freeze({
  android: /^(?<brand>COS-Fusion|Mcp-Agent-Ai)-(?<version>\d+\.\d+\.\d+(?:[-.][A-Za-z0-9]+)*)-Android-debug\.apk(?:\.sha256)?$/i,
});

function isSuperseded(candidateVersion, currentVersion, candidateBrand) {
  if (candidateVersion === currentVersion) return candidateBrand.toLowerCase() === 'cos-fusion';
  const number = version => version.split('-')[0].split('.').slice(0, 3).map(Number);
  const old = number(candidateVersion);
  const current = number(currentVersion);
  for (let i = 0; i < 3; i++) {
    if (old[i] !== current[i]) return old[i] < current[i];
  }
  // Different prerelease channels of the same numeric version are preserved.
  return false;
}

/**
 * Delete ONLY superseded, generated Mcp Agent Ai / COS Fusion release assets.
 * Never touch source, Android Gradle caches, user data, unknown files or links.
 * Call this only AFTER the new platform build and its release output succeed.
 */
export async function pruneOldReleases({ releaseDir, platform, version }) {
  const pattern = artifactPatterns[platform];
  if (!pattern) throw new Error('Unknown release platform');
  if (typeof version !== 'string' || !/^\d+\.\d+\.\d+(?:[-.][A-Za-z0-9]+)*$/.test(version)) {
    throw new Error('Invalid release version');
  }
  const root = path.resolve(releaseDir);
  const rootStat = await lstat(root);
  if (!rootStat.isDirectory() || rootStat.isSymbolicLink()) throw new Error('Release root must be a real directory');
  const current = `Mcp-Agent-Ai-${version}-Android-debug.apk`;
  const keep = new Set([current, `${current}.sha256`]);
  const removed = [];
  for (const item of await readdir(root, { withFileTypes: true })) {
    const match = item.name.match(pattern);
    if (!match || keep.has(item.name) ||
      !isSuperseded(match.groups.version, version, match.groups.brand)) continue;
    if (item.isSymbolicLink() || !item.isFile()) continue;
    const target = path.join(root, item.name);
    const actual = await lstat(target);
    if (actual.isSymbolicLink() || !actual.isFile()) continue;
    await rm(target);
    removed.push(item.name);
  }
  return removed;
}

// Canonical Android-only release: one APK. Never remove unrelated files.
const legacyArtifacts = {
  android: [
    /^Mcp-Agent-Ai-\d+\.\d+\.\d+(?:[-.][\w]+)*-Android-debug\.apk(?:\.sha256)?$/i,
    /^Mcp-Agent-Ai-\d+\.\d+\.\d+(?:[-.][\w]+)*-Tauri-Android-arm64-debug\.apk(?:\.sha256)?$/i,
    /^COS-Fusion-\d+\.\d+\.\d+(?:[-.][\w]+)*-Android-debug\.apk(?:\.sha256)?$/i,
  ],
};
const primaryArtifact = 'Mcp-Agent-Ai-Android.apk';

export async function pruneDuplicateReleases({ releaseDir, platform }) {
  if (platform !== 'android') throw new Error('Only Android release cleanup is supported');
  const root = path.resolve(releaseDir);
  if (path.basename(root).toLowerCase() !== 'release') throw new Error('Expected a release directory');
  const rootStat = await lstat(root);
  if (!rootStat.isDirectory() || rootStat.isSymbolicLink()) throw new Error('Release directory must be real');
  const canonicalPath = path.join(root, primaryArtifact);
  const canonical = await lstat(canonicalPath);
  if (!canonical.isFile() || canonical.isSymbolicLink() || canonical.size <= 1024) {
    throw new Error('Verified canonical release must exist before cleanup');
  }
  const removed = [];
  for (const entry of await readdir(root, { withFileTypes: true })) {
    if (!legacyArtifacts[platform].some(pattern => pattern.test(entry.name)) || entry.isSymbolicLink()) continue;
    const target = path.join(root, entry.name);
    const current = await lstat(target);
    if (current.isSymbolicLink()) continue;
    if (!current.isFile()) continue;
    await rm(target);
    removed.push(entry.name);
  }
  return removed;
}
