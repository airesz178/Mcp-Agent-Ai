import { build } from 'esbuild';
import yazl from 'yazl';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdir, readFile, writeFile, stat } from 'node:fs/promises';
import { createReadStream, createWriteStream } from 'node:fs';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { pipeline } from 'node:stream/promises';

const exec = promisify(execFile);
const root = fileURLToPath(new URL('..', import.meta.url));
const info = JSON.parse(await readFile(path.join(root, 'package.json'), 'utf8'));
const name = `COS-Fusion-${info.version}-Windows-x64`;
const dist = path.join(root, 'dist');
const release = path.join(root, 'release', name);
await mkdir(dist, { recursive: true });
await mkdir(release, { recursive: true });
const bundled = path.join(dist, 'preview.bundle.mjs');
await build({
  entryPoints: [path.join(root, 'bin', 'preview.mjs')],
  outfile: bundled,
  bundle: true,
  platform: 'node',
  format: 'esm',
  target: 'node26',
  minify: false,
  sourcemap: false,
  logLevel: 'info',
});

const executable = path.join(release, 'COS-Fusion-Preview.exe');
const config = {
  main: bundled,
  mainFormat: 'module',
  output: executable,
  disableExperimentalSEAWarning: true,
  useSnapshot: false,
  useCodeCache: false,
  assets: {
    'ui/index.html': path.join(root, 'ui', 'index.html'),
    'ui/app.css': path.join(root, 'ui', 'app.css'),
    'ui/app.js': path.join(root, 'ui', 'app.js'),
  },
};
const configFile = path.join(dist, 'sea-config.json');
await writeFile(configFile, JSON.stringify(config, null, 2));
const result = await exec(process.execPath, ['--build-sea', configFile], { cwd: root, maxBuffer: 4 * 1024 * 1024 });
if (result.stdout.trim()) process.stdout.write(result.stdout);
if (result.stderr.trim()) process.stderr.write(result.stderr);

const digest = createHash('sha256');
for await (const chunk of createReadStream(executable)) digest.update(chunk);
const sha256 = digest.digest('hex').toUpperCase();
const readme = path.join(root, 'docs', 'BETA-README.txt');
const manifest = path.join(release, 'SHA256SUMS.txt');
await writeFile(manifest, `${sha256} *COS-Fusion-Preview.exe\n`);
const zipPath = path.join(root, 'release', `${name}.zip`);
const zip = new yazl.ZipFile();
zip.addFile(executable, 'COS-Fusion-Preview.exe');
zip.addFile(readme, 'README-FIRST.txt');
zip.addFile(manifest, 'SHA256SUMS.txt');
zip.end();
await pipeline(zip.outputStream, createWriteStream(zipPath));
const exeStat = await stat(executable), zipStat = await stat(zipPath);
console.log(JSON.stringify({ executable, exeBytes: exeStat.size, sha256, zipPath, zipBytes: zipStat.size }, null, 2));
