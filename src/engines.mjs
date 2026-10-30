import { spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { loadState, saveState } from './state.mjs';

const MAX_OUTPUT = 2 * 1024 * 1024;
const MAX_ANSWER = 100_000;
const MAX_EVENTS = 40;

/** Hermes owns its tool execution and authentication. COS must explicitly enable this capability. */
export async function runHermesProcess({
  prompt, model = '', workspaceRoot, resumeId = null, spawnFn = spawn, timeoutMs = 130_000,
}) {
  if (typeof prompt !== 'string' || !prompt.trim() || prompt.length > 50_000) throw new Error('Invalid Hermes prompt');
  if (typeof model !== 'string' || model.length > 120 || (model && !/^[a-zA-Z0-9][a-zA-Z0-9._:/-]*$/.test(model))) {
    throw new Error('Invalid Hermes model');
  }
  if (resumeId != null && (typeof resumeId !== 'string' || !/^[a-zA-Z0-9_-]{1,128}$/.test(resumeId))) {
    throw new Error('Invalid Hermes session');
  }
  const args = ['chat', '--oneshot', '--format=stream-json', '--source=tool', '--max-turns=12',
    '--run-budget=120', `--query=${prompt}`];
  if (model) args.push(`--model=${model}`);
  if (resumeId) args.push(`--resume=${resumeId}`);

  return new Promise((resolve, reject) => {
    let child;
    try {
      child = spawnFn('hermes', args, {
        cwd: workspaceRoot, shell: false, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'],
      });
    } catch {
      reject(new Error('Could not launch Hermes CLI'));
      return;
    }
    let stdout = '';
    let stdoutBytes = 0;
    let complete = false;
    let final = null;
    const actions = [];
    let failure = null;
    const timer = setTimeout(() => {
      failure = new Error('Hermes exceeded the 130-second execution limit');
      child.kill();
    }, timeoutMs);
    const fail = error => {
      if (complete) return;
      complete = true;
      clearTimeout(timer);
      child.kill();
      reject(error);
    };
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', chunk => {
      if (complete) return;
      stdoutBytes += Buffer.byteLength(chunk);
      if (stdoutBytes > MAX_OUTPUT) return fail(new Error('Hermes output exceeded the safety limit'));
      stdout += chunk;
      let newline;
      while ((newline = stdout.indexOf('\n')) !== -1) {
        const line = stdout.slice(0, newline).trim();
        stdout = stdout.slice(newline + 1);
        if (!line) continue;
        let event;
        try { event = JSON.parse(line); }
        catch { return fail(new Error('Hermes returned invalid JSON events')); }
        if (event.type === 'tool_use' && actions.length < MAX_EVENTS) {
          actions.push({ tool: String(event.name || 'unknown').slice(0, 120), ok: true });
        }
        if (event.type === 'result') final = event;
      }
    });
    child.stderr.on('data', () => {}); // Drain diagnostics without exposing tokens or profile paths.
    child.on('error', () => fail(new Error('Hermes CLI unavailable. Install and configure Hermes first.')));
    child.on('close', code => {
      if (complete) return;
      complete = true;
      clearTimeout(timer);
      if (failure) return reject(failure);
      if (stdout.trim()) {
        try { const event = JSON.parse(stdout.trim()); if (event.type === 'result') final = event; }
        catch { return reject(new Error('Hermes returned incomplete JSON events')); }
      }
      if (code !== 0 || !final || final.exit_code !== 0) {
        return reject(new Error('Hermes turn failed. Check Hermes setup and model access.'));
      }
      if (typeof final.text !== 'string' || final.text.length > MAX_ANSWER ||
          !/^[a-zA-Z0-9_-]{1,128}$/.test(final.session_id || '')) {
        return reject(new Error('Hermes returned an invalid final response'));
      }
      resolve({ answer: final.text, actions, engineSessionId: final.session_id });
    });
  });
}

export async function runHermesTurn({
  prompt, home, workspaceRoot, model = '', sessionId = null, runner = runHermesProcess,
}) {
  const state = await loadState(home);
  if (state.modules.external_agents !== true) throw new Error('Enable the External agents module in Customize first');
  const previous = sessionId ? state.sessions.find(session => session.id === sessionId) : null;
  if (sessionId && !previous) throw new Error('Conversation not found');
  if (previous && previous.provider !== 'hermes') throw new Error('Start a new chat to switch to Hermes');
  const output = await runner({ prompt, workspaceRoot, model, resumeId: previous?.engineSessionId ?? null });
  if (!output || typeof output.answer !== 'string' || !/^[a-zA-Z0-9_-]{1,128}$/.test(output.engineSessionId || '')) {
    throw new Error('Invalid Hermes engine output');
  }
  const id = sessionId ?? randomUUID();
  const latest = await loadState(home);
  if (latest.modules.external_agents !== true) throw new Error('External agents module was disabled during the request');
  latest.sessions = latest.sessions.filter(session => session.id !== id);
  latest.sessions.unshift({
    id, at: new Date().toISOString(), provider: 'hermes', model: model || null,
    engineSessionId: output.engineSessionId, prompt, answer: output.answer, actions: output.actions ?? [],
    messages: [...(previous?.messages ?? []), { role: 'user', text: prompt },
      { role: 'assistant', text: output.answer }].slice(-40),
  });
  latest.sessions = latest.sessions.slice(0, 25);
  await saveState(home, latest);
  return { sessionId: id, answer: output.answer, actions: output.actions ?? [] };
}
