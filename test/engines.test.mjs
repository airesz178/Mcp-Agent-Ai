import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import os from 'node:os';
import path from 'node:path';
import { runHermesProcess, runHermesTurn } from '../src/engines.mjs';
import { loadState, setModule } from '../src/state.mjs';

function simulatedSpawn({ output, exitCode = 0 }) {
  let invoked;
  const spawnFn = (program, args, options) => {
    invoked = { program, args, options };
    const child = new EventEmitter();
    child.stdout = new PassThrough();
    child.stderr = new PassThrough();
    child.kill = () => { child.emit('close', 1); return true; };
    queueMicrotask(() => {
      child.stdout.end(output);
      child.stderr.end();
      child.emit('close', exitCode);
    });
    return child;
  };
  return { spawnFn, invoked: () => invoked };
}

test('Hermes CLI runs without shell interpolation, parses result, and resumes safely', async () => {
  const fake = simulatedSpawn({ output: [
    JSON.stringify({ type: 'system', subtype: 'init', session_id: 'hermes-123' }),
    JSON.stringify({ type: 'tool_use', name: 'web_search', input: { query: 'synthetic' } }),
    JSON.stringify({ type: 'result', text: 'Finished safely', exit_code: 0, session_id: 'hermes-123' }),
  ].join('\n') + '\n' });
  const result = await runHermesProcess({
    prompt: 'Show shell text $(unsafe) && del file', model: 'provider/model',
    workspaceRoot: 'C:\\fake-workspace', resumeId: 'hermes-123', spawnFn: fake.spawnFn,
  });
  assert.equal(result.answer, 'Finished safely');
  assert.deepEqual(result.actions, [{ tool: 'web_search', ok: true }]);
  const invocation = fake.invoked();
  assert.equal(invocation.program, 'hermes');
  assert.equal(invocation.options.shell, false);
  assert.equal(invocation.options.cwd, 'C:\\fake-workspace');
  assert.ok(invocation.args.includes('--resume=hermes-123'));
  assert.ok(invocation.args.includes('--model=provider/model'));
  assert.ok(invocation.args.includes('--query=Show shell text $(unsafe) && del file'));
});

test('Hermes rejects malformed results, unsafe session IDs and failed runs', async () => {
  const fake = simulatedSpawn({ output: '{bad-json}\n' });
  await assert.rejects(runHermesProcess({ prompt: 'test', workspaceRoot: '.', spawnFn: fake.spawnFn }), /invalid JSON/);
  await assert.rejects(runHermesProcess({ prompt: 'test', workspaceRoot: '.', resumeId: '../outside', spawnFn: fake.spawnFn }), /Invalid Hermes session/);
  const failed = simulatedSpawn({ output: JSON.stringify({ type: 'result', exit_code: 1, text: 'partial', session_id: 'x' }) + '\n', exitCode: 1 });
  await assert.rejects(runHermesProcess({ prompt: 'test', workspaceRoot: '.', spawnFn: failed.spawnFn }), /turn failed/);
});

test('External agents must be enabled and Hermes continuation keeps a local thread', async t => {
  const home = await mkdtemp(path.join(os.tmpdir(), 'cos-fusion-hermes-'));
  t.after(() => rm(home, { recursive: true, force: true }));
  const calls = [];
  const runner = async args => {
    calls.push(args);
    return { answer: 'Synthetic reply', actions: [], engineSessionId: 'session-one' };
  };
  await assert.rejects(runHermesTurn({ prompt: 'hi', home, workspaceRoot: home, runner }), /Enable the External agents/);
  assert.equal(calls.length, 0);
  await setModule(home, 'external_agents', true);
  const first = await runHermesTurn({ prompt: 'hi', home, workspaceRoot: home, runner });
  const second = await runHermesTurn({ prompt: 'next', home, workspaceRoot: home, sessionId: first.sessionId, runner });
  assert.equal(first.sessionId, second.sessionId);
  assert.equal(calls[1].resumeId, 'session-one');
  const state = await loadState(home);
  assert.equal(state.sessions.length, 1);
  assert.equal(state.sessions[0].messages.length, 4);
  assert.equal(state.sessions[0].provider, 'hermes');
  await setModule(home, 'external_agents', false);
  await assert.rejects(runHermesTurn({ prompt: 'again', home, workspaceRoot: home, runner }), /Enable the External agents/);
  assert.equal(calls.length, 2);
});
