import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, mkdir, readFile, rm } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import os from 'node:os';

if (process.platform !== 'win32') throw new Error('Windows release smoke requires Windows');
const root = fileURLToPath(new URL('..', import.meta.url));
const version = JSON.parse(await readFile(path.join(root, 'package.json'), 'utf8')).version;
const releaseDir = path.join(root, 'release', `COS-Fusion-${version}-Windows-x64`);
const exe = path.join(releaseDir, 'COS-Fusion-Preview.exe');
const checksum = (await readFile(path.join(releaseDir, 'SHA256SUMS.txt'), 'utf8')).split(' ')[0];
const executable = await readFile(exe);
assert.equal(createHash('sha256').update(executable).digest('hex').toUpperCase(), checksum, 'release checksum differs');
const temporary = await mkdtemp(path.join(os.tmpdir(), 'cos-fusion-release-smoke-'));
const workspace = path.join(temporary, 'workspace');
await mkdir(workspace);
let child;
try {
  child = spawn(exe, ['--no-browser'], {
    cwd: root, env: { ...process.env, COS_FUSION_HOME: path.join(temporary, 'state'), COS_FUSION_WORKSPACE: workspace },
    windowsHide: true, shell: false, stdio: ['ignore', 'pipe', 'pipe'],
  });
  const url = await new Promise((resolve, reject) => {
    let output = '';
    const timer = setTimeout(() => reject(new Error('Release EXE did not start within 15 seconds')), 15_000);
    const done = (error, value) => { clearTimeout(timer); error ? reject(error) : resolve(value); };
    child.on('error', error => done(error));
    child.on('close', code => done(new Error(`Release EXE exited before ready (${code})`)));
    child.stdout.on('data', chunk => {
      output += chunk.toString();
      const match = output.match(/COS Fusion Preview is running on (http:\/\/127\.0\.0\.1:\d+\/)/);
      if (match) done(null, match[1]);
      if (output.length > 8000) done(new Error('Unexpected release stdout'));
    });
    child.stderr.resume();
  });
  const opened = await fetch(url);
  assert.equal(opened.status, 200);
  const cookie = opened.headers.get('set-cookie')?.split(';')[0];
  assert.ok(cookie);
  const response = await fetch(new URL('/api/state', url), { headers: { cookie } });
  assert.equal(response.status, 200);
  const state = await response.json();
  assert.equal(state.version, version);
  assert.ok(state.providers.includes('hermes'));
  assert.equal(state.modules.external_agents, false);
  console.log(`Release smoke PASS: ${path.basename(exe)}, checksum verified, GUI and API started, Hermes disabled by default`);
} finally {
  if (child && child.exitCode === null) child.kill();
  await rm(temporary, { recursive: true, force: true });
}
