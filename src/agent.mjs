import { randomUUID } from 'node:crypto';
import { defaultHome, enabledSkillText, loadState, saveState } from './state.mjs';
import { callTool, visibleTools } from './tools.mjs';
import { makeProvider } from './providers.mjs';

/** Local execution is real. Cloud execution is a future adapter, never silently simulated. */
export async function runTurn({
  prompt, providerName = 'demo', model = '', home = defaultHome(), workspaceRoot = process.cwd(),
  execution = 'local', provider = null, maxSteps = 8, sessionId = null,
}) {
  if (execution !== 'local') throw new Error('Cloud runner is not configured; no remote work was started');
  if (typeof prompt !== 'string' || !prompt.trim()) throw new Error('Prompt is required');
  if (prompt.length > 50_000) throw new Error('Prompt exceeds 50,000 characters');
  if (!Number.isInteger(maxSteps) || maxSteps < 1 || maxSteps > 20) throw new Error('Invalid step limit');
  const state = await loadState(home);
  const activeProvider = provider ?? makeProvider(providerName);
  if (activeProvider.id !== 'demo' && !model) throw new Error('Model name is required');
  const skills = await enabledSkillText(home, state);
  const previous = sessionId ? state.sessions.find(session => session.id === sessionId) : null;
  if (sessionId && !previous) throw new Error('Conversation not found');
  const messages = [
    { role: 'system', text: `You are COS Fusion, a coding assistant. Use available tools only when needed. Workspace: ${workspaceRoot}. Never claim an action succeeded unless tool output proves it.${skills ? `\n\nEnabled skills:\n${skills}` : ''}` },
    ...(Array.isArray(previous?.messages) ? previous.messages.slice(-12) : []),
    { role: 'user', text: prompt },
  ];
  const actions = [];
  let answer = null;
  for (let step = 0; step < maxSteps; step++) {
    const latest = await loadState(home);
    const output = await activeProvider.generate({ model, messages, tools: visibleTools(latest) });
    if (!output || typeof output.text !== 'string' || !Array.isArray(output.toolCalls)) {
      throw new Error('Model response did not match the agent protocol');
    }
    if (output.toolCalls.length === 0) { answer = output.text; messages.push({ role: 'assistant', text: answer }); break; }
    if (output.toolCalls.length > 8) throw new Error('Too many tool calls in one model response');
    messages.push({ role: 'assistant', text: output.text, toolCalls: output.toolCalls });
    for (const call of output.toolCalls) {
      let text;
      try {
        const args = JSON.parse(call.arguments);
        const current = await loadState(home);
        const result = await callTool({ home, workspaceRoot, state: current, name: call.name, args });
        text = JSON.stringify({ ok: true, result });
        actions.push({ tool: call.name, ok: true });
      } catch (error) {
        text = JSON.stringify({ ok: false, error: error.message });
        actions.push({ tool: call.name, ok: false, error: error.message });
      }
      messages.push({ role: 'tool', callId: call.id, text });
    }
  }
  if (answer === null) throw new Error(`Agent step limit (${maxSteps}) reached without a final answer`);
  const nextSessionId = sessionId ?? randomUUID();
  const latest = await loadState(home);
  latest.sessions = latest.sessions.filter(session => session.id !== nextSessionId);
  latest.sessions.unshift({ id: nextSessionId, at: new Date().toISOString(), provider: activeProvider.id,
    model: model || null, prompt, answer, actions,
    messages: [...(previous?.messages ?? []), { role: 'user', text: prompt }, { role: 'assistant', text: answer }].slice(-40) });
  latest.sessions = latest.sessions.slice(0, 25);
  await saveState(home, latest);
  return { sessionId: nextSessionId, answer, actions };
}
