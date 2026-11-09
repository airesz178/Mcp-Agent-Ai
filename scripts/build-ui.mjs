import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { copyFile } from 'node:fs/promises';

const root = fileURLToPath(new URL('..', import.meta.url));
await build({
  entryPoints: [path.join(root, 'ui', 'app.ts')],
  outfile: path.join(root, 'ui', 'app.js'),
  bundle: true,
  platform: 'browser',
  format: 'iife',
  target: ['chrome110', 'safari16'],
  sourcemap: false,
  minify: false,
  logLevel: 'info',
});
// Android-only standalone COS-like PTY client. The main Windows UI does not
// import, load or initialize this bundle.
await build({
  entryPoints: [path.join(root, 'ui', 'android-pty-client.ts')],
  outfile: path.join(root, 'ui', 'android-pty-client.js'),
  bundle: true,
  platform: 'browser',
  format: 'iife',
  target: ['chrome110'],
  sourcemap: false,
  minify: false,
  logLevel: 'info',
});
await copyFile(
  path.join(root, 'node_modules', '@xterm', 'xterm', 'css', 'xterm.css'),
  path.join(root, 'ui', 'android-xterm.css')
);
