#!/usr/bin/env node
import { startServer } from '../src/server.mjs';
import { defaultHome } from '../src/state.mjs';
import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';

const workspaceRoot = process.env.COS_FUSION_WORKSPACE || path.join(defaultHome(), 'workspace');
if (!process.env.COS_FUSION_WORKSPACE) {
  await mkdir(workspaceRoot, { recursive: true });
  try {
    await writeFile(path.join(workspaceRoot, 'WELCOME.txt'),
      'COS Fusion sample workspace. Put non-sensitive test files here to try the workspace tools.\n', { flag: 'wx' });
  } catch (error) {
    if (error.code !== 'EEXIST') throw error;
  }
}
const app = await startServer({ workspaceRoot, openBrowser: !process.argv.includes('--no-browser') });
console.log(`COS Fusion Preview is running on ${app.url}`);
console.log('Close this window or press Ctrl+C to stop the preview.');
process.on('SIGINT', () => { app.close().finally(() => process.exit(0)); });
process.on('SIGTERM', () => { app.close().finally(() => process.exit(0)); });
