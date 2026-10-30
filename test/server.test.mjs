import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { request as httpRequest } from 'node:http';
import { startServer } from '../src/server.mjs';

async function preview(t) {
  const root = await mkdtemp(path.join(os.tmpdir(), 'cos-fusion-server-'));
  const home = path.join(root, 'state');
  const workspaceRoot = path.join(root, 'workspace');
  await mkdir(workspaceRoot);
  await writeFile(path.join(workspaceRoot, 'readme.txt'), 'sample workspace');
  const app = await startServer({ home, workspaceRoot, openBrowser: false });
  t.after(async () => { await app.close(); await rm(root, { recursive: true, force: true }); });
  const opened = await fetch(app.url);
  const cookie = opened.headers.get('set-cookie').split(';')[0];
  const origin = new URL(app.url).origin;
  return {
    ...app, cookie, origin,
    async get(route, authorized = true) {
      return fetch(app.url + route.replace(/^\//, ''), { headers: authorized ? { cookie } : {} });
    },
    async post(route, payload, headers = {}) {
      return fetch(app.url + route.replace(/^\//, ''), {
        method: 'POST', headers: { cookie, origin, 'content-type': 'application/json', ...headers },
        body: JSON.stringify(payload),
      });
    },
  };
}

test('local preview serves static interface with privacy headers', async t => {
  const app = await preview(t);
  const page = await app.get('/');
  const html = await page.text();
  assert.equal(page.status, 200);
  assert.match(html, /COS Fusion/);
  assert.match(page.headers.get('content-security-policy'), /frame-ancestors 'none'/);
  assert.equal((await app.get('/app.js')).status, 200);
  assert.equal((await app.get('/app.css')).status, 200);
});

test('cannot call control API without same-origin session', async t => {
  const app = await preview(t);
  assert.equal((await app.get('/api/state', false)).status, 403);
  assert.equal((await app.post('/api/module', { id: 'workspace', enabled: true }, { origin: 'https://evil.example' })).status, 403);
  assert.equal((await app.post('/api/module', { id: 'workspace', enabled: true }, { 'content-type': 'text/plain' })).status, 400);
  // Node fetch may rewrite Host to the URL authority. Use a raw HTTP client instead.
  const hostileStatus = await new Promise((resolve, reject) => {
    const request = httpRequest(app.url + 'api/state', {
      headers: { cookie: app.cookie, Host: 'attacker.example' },
    }, response => { response.resume(); resolve(response.statusCode); });
    request.on('error', reject);
    request.end();
  });
  assert.equal(hostileStatus, 403);
});

test('UI API switches modules, imports skills, and runs multi-turn demo agent', async t => {
  const app = await preview(t);
  let state = await (await app.get('/api/state')).json();
  assert.equal(state.modules.workspace, false);
  assert.equal(state.sessions.length, 0);
  assert.equal((await app.post('/api/module', { id: 'workspace', enabled: true })).status, 200);
  const imported = await (await app.post('/api/skill', { action: 'add', name: 'review', text: '# Review first' })).json();
  assert.equal(imported.id, 'review');
  assert.equal((await app.post('/api/skill', { action: 'enable', id: 'review' })).status, 200);
  const first = await (await app.post('/api/chat', { prompt: '/list', provider: 'demo', model: '' })).json();
  assert.match(first.answer, /readme.txt/);
  const second = await (await app.post('/api/chat', { prompt: 'hello', provider: 'demo', model: '', sessionId: first.sessionId })).json();
  assert.equal(second.sessionId, first.sessionId);
  state = await (await app.get('/api/state')).json();
  assert.equal(state.sessions.length, 1);
  assert.equal(state.sessions[0].messages.length, 4);
  assert.equal(state.skills[0].enabled, true);
  assert.equal((await app.post('/api/skill', { action: 'remove', id: 'review' })).status, 200);
});

test('API keys stay in memory and are not included in returned state', async t => {
  const app = await preview(t);
  assert.equal((await app.post('/api/key', { provider: 'openai', key: 'secret-test-key' })).status, 200);
  const state = await (await app.get('/api/state')).text();
  assert.ok(!state.includes('secret-test-key'));
  assert.ok(state.includes('"hasKeys":["openai"]'));
  await app.post('/api/key', { provider: 'openai', key: '' });
  const cleared = await (await app.get('/api/state')).json();
  assert.deepEqual(cleared.hasKeys, []);
});

test('Hermes is selectable but cannot launch without External agents permission', async t => {
  const app = await preview(t);
  const state = await (await app.get('/api/state')).json();
  assert.ok(state.providers.includes('hermes'));
  assert.equal(state.modules.external_agents, false);
  const response = await app.post('/api/chat', { provider: 'hermes', model: '', prompt: 'hello' });
  assert.equal(response.status, 400);
  assert.match((await response.json()).error, /Enable the External agents/);
});
