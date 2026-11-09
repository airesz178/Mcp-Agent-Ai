import { createServer } from 'node:http';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { readFile, realpath } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { isSea, getAsset } from 'node:sea';
import { defaultHome, loadState, setModule, importSkillText, setSkill, removeSkill } from './state.mjs';
import { runTurn } from './agent.mjs';
import { makeProvider, providerNames } from './providers.mjs';
import { runHermesTurn } from './engines.mjs';

const MIME = { '/': 'text/html; charset=utf-8', '/app.css': 'text/css; charset=utf-8', '/app.js': 'text/javascript; charset=utf-8', '/android-boot.js': 'text/javascript; charset=utf-8', '/app-icon.png': 'image/png', '/android-pty-client.js': 'text/javascript; charset=utf-8', '/android-xterm.css': 'text/css; charset=utf-8' };
const FILES = { '/': 'index.html', '/app.css': 'app.css', '/app.js': 'app.js', '/android-boot.js': 'android-boot.js', '/app-icon.png': 'app-icon.png', '/android-pty-client.js': 'android-pty-client.js', '/android-xterm.css': 'android-xterm.css' };
const MAX_BODY = 64 * 1024;

function json(res, code, body) {
  res.writeHead(code, { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store' });
  res.end(JSON.stringify(body));
}

async function inputJSON(request) {
  if (!String(request.headers['content-type'] ?? '').startsWith('application/json')) {
    throw new Error('Only JSON requests are accepted');
  }
  const chunks = [];
  let bytes = 0;
  for await (const chunk of request) {
    bytes += chunk.length;
    if (bytes > MAX_BODY) throw new Error('Request too large');
    chunks.push(chunk);
  }
  const value = JSON.parse(Buffer.concat(chunks).toString('utf8'));
  if (!value || Array.isArray(value) || typeof value !== 'object') throw new Error('Request must be an object');
  return value;
}

async function uiFile(filename) {
  if (isSea()) return Buffer.from(getAsset(`ui/${filename}`));
  return readFile(new URL(`../ui/${filename}`, import.meta.url));
}

function equalsToken(actual, expected) {
  if (typeof actual !== 'string') return false;
  const a = Buffer.from(actual), b = Buffer.from(expected);
  return a.length === b.length && timingSafeEqual(a, b);
}

/** Private loopback GUI: no public listener, no credential persistence, no CORS. */
export async function startServer({ home = defaultHome(), workspaceRoot = process.cwd(), port = 0, openBrowser = false } = {}) {
  const token = randomBytes(32).toString('hex');
  const secretByProvider = new Map();
  let inFlight = false;
  const startingWorkspace = await realpath(workspaceRoot);
  const server = createServer(async (req, res) => {
    const hostname = `127.0.0.1:${server.address()?.port}`;
    const origin = `http://${hostname}`;
    res.setHeader('x-content-type-options', 'nosniff');
    res.setHeader('referrer-policy', 'no-referrer');
    res.setHeader('x-frame-options', 'DENY');
    // Bounded in-memory Android SAF image previews need data: only for img-src.
    // Keep script, connect, frames and every other source policy unchanged.
    res.setHeader('content-security-policy', "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'");
    if (req.headers.host !== hostname) { json(res, 403, { error: 'Invalid host' }); return; }
    let pathname;
    try { pathname = new URL(req.url, origin).pathname; }
    catch { json(res, 400, { error: 'Invalid address' }); return; }
    if (req.method === 'GET' && Object.hasOwn(FILES, pathname)) {
      try {
        if (pathname === '/') res.setHeader('set-cookie', `cos_fusion=${token}; HttpOnly; SameSite=Strict; Path=/`);
        const buffer = await uiFile(FILES[pathname]);
        res.writeHead(200, { 'content-type': MIME[pathname], 'cache-control': 'no-store' });
        res.end(buffer);
      } catch { json(res, 500, { error: 'UI unavailable' }); }
      return;
    }
    const cookie = String(req.headers.cookie ?? '').split(';').map(s => s.trim()).find(s => s.startsWith('cos_fusion='))?.slice(11);
    if (!equalsToken(cookie, token)) { json(res, 403, { error: 'Session not authorized' }); return; }
    if (req.method === 'POST' && req.headers.origin !== origin) {
      json(res, 403, { error: 'Origin mismatch' }); return;
    }
    if (req.method === 'GET' && pathname === '/api/state') {
      try {
        const state = await loadState(home);
        json(res, 200, { version: '0.2.2-preview', modules: state.modules, skills: state.skills,
          sessions: state.sessions.filter(session => providerNames().includes(session.provider)).map(({ id, at, provider, model, messages, prompt, answer, actions }) => ({
            id, at, provider, model, messages: Array.isArray(messages) ? messages : [{ role: 'user', text: prompt }, { role: 'assistant', text: answer }], actions,
          })),
          providers: providerNames(), workspace: startingWorkspace, cloud: 'not available',
          hasKeys: [...secretByProvider.keys()] });
      } catch { json(res, 500, { error: 'Cannot load local state' }); }
      return;
    }
    if (req.method !== 'POST') { json(res, 404, { error: 'Not found' }); return; }
    if (inFlight) { json(res, 409, { error: 'A request is already running' }); return; }
    inFlight = true;
    try {
      const body = await inputJSON(req);
      if (pathname === '/api/key') {
        if (typeof body.provider !== 'string' || !['openai', 'openrouter', 'anthropic'].includes(body.provider)) throw new Error('Unsupported key provider');
        if (typeof body.key !== 'string' || body.key.length > 4096) throw new Error('Invalid API key');
        if (body.key.trim()) secretByProvider.set(body.provider, body.key.trim());
        else secretByProvider.delete(body.provider);
        json(res, 200, { savedInMemory: secretByProvider.has(body.provider) });
      } else if (pathname === '/api/module') {
        if (typeof body.id !== 'string' || typeof body.enabled !== 'boolean') throw new Error('Invalid module request');
        const state = await setModule(home, body.id, body.enabled);
        json(res, 200, { modules: state.modules });
      } else if (pathname === '/api/skill') {
        if (body.action === 'add') {
          // No arbitrary files selected by a remote caller: import Markdown text from the UI.
          if (typeof body.name !== 'string' || !/^[a-zA-Z0-9-]{1,50}$/.test(body.name)) throw new Error('Invalid skill name');
          if (typeof body.text !== 'string' || Buffer.byteLength(body.text) > 32 * 1024) throw new Error('Skill exceeds 32 KiB');
          // Imported skills are instructions, not executable plugins.
          const id = await importSkillText(home, body.name, body.text);
          json(res, 200, { id });
        } else if (body.action === 'remove') {
          await removeSkill(home, body.id);
          json(res, 200, { removed: body.id });
        } else if (body.action === 'enable' || body.action === 'disable') {
          await setSkill(home, body.id, body.action === 'enable');
          json(res, 200, { id: body.id, enabled: body.action === 'enable' });
        } else throw new Error('Unknown skill action');
      } else if (pathname === '/api/chat') {
        const providerName = body.provider;
        if (!providerNames().includes(providerName)) throw new Error('Unsupported provider');
        if (typeof body.model !== 'string' || body.model.length > 120) throw new Error('Invalid model');
        if (typeof body.prompt !== 'string' || body.prompt.length > 50_000) throw new Error('Invalid prompt');
        if (body.sessionId != null && (typeof body.sessionId !== 'string' || !/^[0-9a-f-]{36}$/.test(body.sessionId))) throw new Error('Invalid conversation');
        // Public beta is intentionally restricted to the workspace fixed when launched.
        const provider = providerName === 'hermes' ? null : makeProvider(providerName, {
          env: {
            ...process.env,
            ...(secretByProvider.has('openai') ? { OPENAI_API_KEY: secretByProvider.get('openai') } : {}),
            ...(secretByProvider.has('openrouter') ? { OPENROUTER_API_KEY: secretByProvider.get('openrouter') } : {}),
            ...(secretByProvider.has('anthropic') ? { ANTHROPIC_API_KEY: secretByProvider.get('anthropic') } : {}),
          },
        });
        const result = providerName === 'hermes'
          ? await runHermesTurn({ prompt: body.prompt, model: body.model, home,
            workspaceRoot: startingWorkspace, sessionId: body.sessionId ?? null })
          : await runTurn({ prompt: body.prompt, provider, providerName, model: body.model, home,
            workspaceRoot: startingWorkspace, sessionId: body.sessionId ?? null });
        json(res, 200, result);
      } else json(res, 404, { error: 'Unknown command' });
    } catch (error) {
      json(res, 400, { error: error instanceof Error ? error.message : 'Unexpected request error' });
    } finally { inFlight = false; }
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', resolve);
  });
  const url = `http://127.0.0.1:${server.address().port}/`;
  if (openBrowser && process.platform === 'win32') {
    const child = spawn('rundll32.exe', ['url.dll,FileProtocolHandler', url], { detached: true, stdio: 'ignore', windowsHide: true });
    child.on('error', () => {});
    child.unref();
  }
  return { server, url, close: () => new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve())) };
}
