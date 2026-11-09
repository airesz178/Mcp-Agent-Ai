#!/usr/bin/env node
import process from 'node:process';
import { defaultHome, loadState, MODULES, setModule, addSkill, setSkill, removeSkill } from '../src/state.mjs';
import { providerNames } from '../src/providers.mjs';
import { runTurn } from '../src/agent.mjs';
import { runHermesTurn } from '../src/engines.mjs';

const home = defaultHome();
const [command, ...args] = process.argv.slice(2);

function usage() {
  console.log(`Mcp Agent Ai Core 0.1.0 (prototype)\n
  node bin/cos.mjs status
  node bin/cos.mjs modules
  node bin/cos.mjs module enable|disable workspace|notes
  node bin/cos.mjs skill add path/to/SKILL.md
  node bin/cos.mjs skill enable|disable|remove skill-id
  node bin/cos.mjs ask openai|anthropic|openrouter|ollama|hermes MODEL "your task"

State: COS_FUSION_HOME (default ~/.cos-fusion)
Workspace: COS_FUSION_WORKSPACE (default current folder)
Optional endpoint override: COS_FUSION_ENDPOINT
Cloud execution: planned, not connected. No background server runs.`);
}

try {
  if (command === 'status') {
    const state = await loadState(home);
    console.log(JSON.stringify({ version: '0.1.0', status: 'prototype', execution: 'local',
      cloud: 'not configured', providers: providerNames(), modules: state.modules,
      skills: state.skills, sessionsSaved: state.sessions.length, home }, null, 2));
  } else if (command === 'modules') {
    const state = await loadState(home);
    for (const [id, description] of Object.entries(MODULES)) console.log(`${state.modules[id] ? 'ON ' : 'OFF'} ${id}: ${description}`);
  } else if (command === 'module' && ['enable', 'disable'].includes(args[0]) && args[1]) {
    await setModule(home, args[1], args[0] === 'enable');
    console.log(`${args[1]} ${args[0]}d`);
  } else if (command === 'skill' && args[0] === 'add' && args[1]) {
    console.log(`Installed disabled skill: ${await addSkill(home, args[1])}`);
  } else if (command === 'skill' && ['enable','disable'].includes(args[0]) && args[1]) {
    await setSkill(home, args[1], args[0] === 'enable');
    console.log(`Skill ${args[1]} ${args[0]}d`);
  } else if (command === 'skill' && args[0] === 'remove' && args[1]) {
    await removeSkill(home, args[1]);
    console.log(`Removed skill ${args[1]}`);
  } else if (command === 'ask' && args.length >= 3) {
    const [providerName, model, ...prompt] = args;
    const options = { prompt: prompt.join(' '), model: model === '-' ? '' : model, home,
      workspaceRoot: process.env.COS_FUSION_WORKSPACE || process.cwd() };
    const output = providerName === 'hermes' ? await runHermesTurn(options) : await runTurn({ ...options, providerName });
    console.log(output.answer);
    console.log(JSON.stringify({ sessionId: output.sessionId, actions: output.actions }));
  } else { usage(); if (command && command !== 'help') process.exitCode = 2; }
} catch (error) {
  console.error(`Mcp Agent Ai: ${error.message}`);
  process.exitCode = 1;
}
