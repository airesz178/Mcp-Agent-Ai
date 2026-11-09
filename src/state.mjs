import { readFile, writeFile, mkdir, rename, unlink } from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { randomUUID } from 'node:crypto';

export const MODULES = Object.freeze({
  workspace: 'Read files and list folders within the selected workspace',
  notes: 'Create and retrieve small local notes',
  external_agents: 'Allow locally installed agents to run their configured tools',
});

export function defaultHome() {
  return process.env.COS_FUSION_HOME || path.join(os.homedir(), '.cos-fusion');
}

export function freshState() {
  return { version: 1, modules: { workspace: false, notes: false, external_agents: false }, skills: [], notes: {}, sessions: [] };
}

export async function loadState(home = defaultHome()) {
  let data;
  try { data = await readFile(path.join(home, 'state.json'), 'utf8'); }
  catch (error) { if (error.code === 'ENOENT') return freshState(); throw error; }
  const parsed = JSON.parse(data);
  if (!parsed || parsed.version !== 1 || !parsed.modules || !Array.isArray(parsed.skills) ||
      !parsed.notes || !Array.isArray(parsed.sessions)) {
    throw new Error('Invalid Mcp Agent Ai state. Existing data has not been overwritten.');
  }
  return { ...parsed, modules: { ...freshState().modules, ...parsed.modules } };
}

export async function saveState(home, state) {
  await mkdir(home, { recursive: true });
  const temporary = path.join(home, `.state-${randomUUID()}.tmp`);
  await writeFile(temporary, JSON.stringify(state, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
  await rename(temporary, path.join(home, 'state.json'));
}

export async function setModule(home, moduleId, enabled) {
  if (!Object.hasOwn(MODULES, moduleId)) throw new Error(`Unknown module: ${moduleId}`);
  const state = await loadState(home);
  state.modules[moduleId] = enabled;
  await saveState(home, state);
  return state;
}

function skillId(filename) {
  const id = path.basename(filename, path.extname(filename)).toLowerCase().replace(/[^a-z0-9-]/g, '-');
  if (!/^[a-z0-9][a-z0-9-]{0,49}$/.test(id)) throw new Error('Skill filename needs an alphanumeric name');
  return id;
}

export async function addSkill(home, sourceFile) {
  if (path.extname(sourceFile).toLowerCase() !== '.md') throw new Error('Skills must be Markdown files');
  const content = await readFile(sourceFile);
  if (content.length > 64 * 1024) throw new Error('Skill exceeds the 64 KiB limit');
  const id = skillId(sourceFile);
  const state = await loadState(home);
  if (state.skills.some(skill => skill.id === id)) throw new Error(`Skill ${id} already installed`);
  const skillDir = path.join(home, 'skills');
  await mkdir(skillDir, { recursive: true });
  const destination = path.join(skillDir, `${id}.md`);
  await writeFile(destination, content, { flag: 'wx', mode: 0o600 });
  try {
    state.skills.push({ id, enabled: false });
    await saveState(home, state);
  } catch (error) {
    await unlink(destination).catch(() => {});
    throw error;
  }
  return id;
}

export async function importSkillText(home, name, text) {
  if (!/^[a-zA-Z0-9-]{1,50}$/.test(name) || typeof text !== 'string' || Buffer.byteLength(text) > 32 * 1024) {
    throw new Error('Invalid skill content or filename');
  }
  const id = name.toLowerCase();
  const state = await loadState(home);
  if (state.skills.some(skill => skill.id === id)) throw new Error(`Skill ${id} already installed`);
  const skillDir = path.join(home, 'skills');
  await mkdir(skillDir, { recursive: true });
  const destination = path.join(skillDir, `${id}.md`);
  await writeFile(destination, text, { flag: 'wx', mode: 0o600 });
  try {
    state.skills.push({ id, enabled: false });
    await saveState(home, state);
  } catch (error) {
    await unlink(destination).catch(() => {});
    throw error;
  }
  return id;
}

export async function setSkill(home, id, enabled) {
  const state = await loadState(home);
  const skill = state.skills.find(value => value.id === id);
  if (!skill) throw new Error(`Skill ${id} not installed`);
  skill.enabled = enabled;
  await saveState(home, state);
}

export async function removeSkill(home, id) {
  const state = await loadState(home);
  const skill = state.skills.find(value => value.id === id);
  if (!skill) throw new Error(`Skill ${id} not installed`);
  // The skill ID is derived from an installed record, never an arbitrary path.
  if (!/^[a-z0-9][a-z0-9-]{0,49}$/.test(id)) throw new Error('Invalid skill ID');
  const filename = path.join(home, 'skills', `${id}.md`);
  const quarantine = path.join(home, 'skills', `.${id}-${randomUUID()}.removed`);
  let moved = false;
  try { await rename(filename, quarantine); moved = true; }
  catch (error) { if (error.code !== 'ENOENT') throw error; }
  state.skills = state.skills.filter(value => value.id !== id);
  try { await saveState(home, state); }
  catch (error) {
    if (moved) await rename(quarantine, filename);
    throw error;
  }
  if (moved) await unlink(quarantine);
}

export async function enabledSkillText(home, state) {
  const parts = [];
  for (const skill of state.skills.filter(value => value.enabled)) {
    if (!/^[a-z0-9][a-z0-9-]{0,49}$/.test(skill.id)) throw new Error('Invalid installed skill ID');
    const text = await readFile(path.join(home, 'skills', `${skill.id}.md`), 'utf8');
    if (Buffer.byteLength(text) > 64 * 1024) throw new Error('Installed skill exceeds size limit');
    parts.push(`## Skill ${skill.id}\n${text}`);
  }
  return parts.join('\n\n');
}
