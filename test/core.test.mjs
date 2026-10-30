import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { createServer } from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { freshState, loadState, saveState, setModule, addSkill, setSkill, removeSkill, enabledSkillText } from '../src/state.mjs';
import { callTool, visibleTools } from '../src/tools.mjs';
import { runTurn } from '../src/agent.mjs';
import { makeProvider } from '../src/providers.mjs';

async function fixture(t) {
  const root = await mkdtemp(path.join(os.tmpdir(), 'cos-fusion-test-'));
  const home = path.join(root, 'cos-data');
  const workspaceRoot = path.join(root, 'workspace');
  await mkdir(workspaceRoot);
  await writeFile(path.join(workspaceRoot, 'hello.txt'), 'hello from workspace');
  t.after(async () => { await rm(root, { recursive: true, force: true }); });
  return { root, home, workspaceRoot };
}

test('modules persist, disabled notes are not exposed, and saved notes can be read', async t => {
  const { home, workspaceRoot } = await fixture(t);
  assert.equal((await loadState(home)).modules.notes, false);
  assert.deepEqual(visibleTools(await loadState(home)).map(tool => tool.name), []);
  await assert.rejects(callTool({ home, workspaceRoot, state: await loadState(home), name: 'notes_save', args: { key: 'a', text: 'b' } }), /not enabled/);
  await setModule(home, 'notes', true);
  const state = await loadState(home);
  assert.equal(state.modules.notes, true);
  assert.equal((await callTool({ home, workspaceRoot, state, name: 'notes_save', args: { key: 'idea', text: 'fusion' } })).saved, 'idea');
  assert.equal((await callTool({ home, workspaceRoot, state, name: 'notes_read', args: { key: 'idea' } })).text, 'fusion');
  await setModule(home, 'notes', false);
  await assert.rejects(callTool({ home, workspaceRoot, state: await loadState(home), name: 'notes_read', args: { key: 'idea' } }), /not enabled/);
});

test('workspace path traversal is denied and a small in-root file can be read', async t => {
  const { root, home, workspaceRoot } = await fixture(t);
  await writeFile(path.join(root, 'private.txt'), 'outside');
  await setModule(home, 'workspace', true);
  const state = await loadState(home);
  assert.equal(await callTool({ home, workspaceRoot, state, name: 'workspace_read', args: { file: 'hello.txt' } }), 'hello from workspace');
  await assert.rejects(callTool({ home, workspaceRoot, state, name: 'workspace_read', args: { file: '../private.txt' } }), /inside the workspace/);
  await assert.rejects(callTool({ home, workspaceRoot, state, name: 'workspace_read', args: { file: path.join(root, 'private.txt') } }), /inside the workspace/);
});

test('Markdown skills require explicit activation and are removable', async t => {
  const { root, home } = await fixture(t);
  const skillPath = path.join(root, 'check-design.md');
  await writeFile(skillPath, 'Review architecture before editing');
  assert.equal(await addSkill(home, skillPath), 'check-design');
  assert.equal(await enabledSkillText(home, await loadState(home)), '');
  await setSkill(home, 'check-design', true);
  assert.match(await enabledSkillText(home, await loadState(home)), /Review architecture/);
  await removeSkill(home, 'check-design');
  assert.deepEqual((await loadState(home)).skills, []);
});

test('demo agent calls workspace tool and records a real result; cloud fails closed', async t => {
  const { home, workspaceRoot } = await fixture(t);
  await setModule(home, 'workspace', true);
  await assert.rejects(runTurn({ home, workspaceRoot, prompt: '/list', execution: 'cloud' }), /not configured/);
  const result = await runTurn({ home, workspaceRoot, prompt: '/list' });
  assert.match(result.answer, /hello.txt/);
  assert.deepEqual(result.actions, [{ tool: 'workspace_list', ok: true }]);
  assert.equal((await loadState(home)).sessions[0].id, result.sessionId);
});

test('a revoked module cannot execute a stale model tool call', async t => {
  const { home, workspaceRoot } = await fixture(t);
  await setModule(home, 'workspace', true);
  const provider = {
    id: 'demo', calls: 0,
    async generate({ messages }) {
      this.calls++;
      if (this.calls === 1) {
        await setModule(home, 'workspace', false);
        return { text: '', toolCalls: [{ id: 'call-revoked', name: 'workspace_read', arguments: '{"file":"hello.txt"}' }] };
      }
      return { text: messages.at(-1).text, toolCalls: [] };
    },
  };
  const result = await runTurn({ home, workspaceRoot, prompt: 'read', provider });
  assert.equal(result.actions[0].ok, false);
  assert.match(result.answer, /Tool is not enabled/);
});

test('OpenAI-compatible adapter sends tool schemas and resumes with a tool result', async t => {
  const { home, workspaceRoot } = await fixture(t);
  await setModule(home, 'workspace', true);
  const seen = [];
  const server = createServer(async (req, res) => {
    let json = '';
    for await (const part of req) json += part;
    seen.push({ path: req.url, auth: req.headers.authorization, payload: JSON.parse(json) });
    res.setHeader('content-type', 'application/json');
    if (seen.length === 1) res.end(JSON.stringify({ choices: [{ message: { content: null, tool_calls: [{ id: 'call-1', type: 'function', function: { name: 'workspace_read', arguments: '{"file":"hello.txt"}' } }] } }] }));
    else res.end(JSON.stringify({ choices: [{ message: { content: 'Confirmed by tool output' } }] }));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => server.close(resolve)));
  const provider = makeProvider('openai', { env: {
    OPENAI_API_KEY: 'test-key', COS_FUSION_ENDPOINT: `http://127.0.0.1:${server.address().port}/v1`,
  } });
  const result = await runTurn({ home, workspaceRoot, prompt: 'read file', model: 'test-model', provider });
  assert.equal(result.answer, 'Confirmed by tool output');
  assert.equal(seen.length, 2);
  assert.equal(seen[0].path, '/v1/chat/completions');
  assert.equal(seen[0].auth, 'Bearer test-key');
  assert.equal(seen[0].payload.tools[0].function.name, 'workspace_list');
  assert.match(seen[1].payload.messages.at(-1).content, /hello from workspace/);
});

test('Anthropic adapter sends Messages protocol; arbitrary public HTTP is rejected', async t => {
  const { home, workspaceRoot } = await fixture(t);
  await setModule(home, 'workspace', true);
  assert.throws(() => makeProvider('openai', { env: { OPENAI_API_KEY: 'test', COS_FUSION_ENDPOINT: 'http://example.com/v1' } }), /requires HTTPS/);
  let received;
  const server = createServer(async (req, res) => {
    let body = '';
    for await (const chunk of req) body += chunk;
    received = { path: req.url, key: req.headers['x-api-key'], body: JSON.parse(body) };
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ content: [{ type: 'text', text: 'Claude-format response' }] }));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => server.close(resolve)));
  const provider = makeProvider('anthropic', { env: {
    ANTHROPIC_API_KEY: 'test-claude-key', COS_FUSION_ENDPOINT: `http://127.0.0.1:${server.address().port}/v1`,
  } });
  const result = await runTurn({ home, workspaceRoot, prompt: 'hello', model: 'claude-test', provider });
  assert.equal(result.answer, 'Claude-format response');
  assert.equal(received.path, '/v1/messages');
  assert.equal(received.key, 'test-claude-key');
  assert.equal(received.body.model, 'claude-test');
});
