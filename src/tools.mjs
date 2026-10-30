import fs from 'node:fs/promises';
import path from 'node:path';
import { loadState, saveState } from './state.mjs';

const definitions = {
  workspace_list: {
    module: 'workspace', description: 'List up to 40 entries in a workspace folder',
    parameters: { type: 'object', properties: { directory: { type: 'string' } }, additionalProperties: false },
  },
  workspace_read: {
    module: 'workspace', description: 'Read up to 64 KiB of a text file in the workspace',
    parameters: { type: 'object', properties: { file: { type: 'string' } }, required: ['file'], additionalProperties: false },
  },
  notes_save: {
    module: 'notes', description: 'Save a short local project note by key',
    parameters: { type: 'object', properties: { key: { type: 'string' }, text: { type: 'string' } }, required: ['key','text'], additionalProperties: false },
  },
  notes_read: {
    module: 'notes', description: 'Read a local project note by key',
    parameters: { type: 'object', properties: { key: { type: 'string' } }, required: ['key'], additionalProperties: false },
  },
};

export function visibleTools(state) {
  return Object.entries(definitions).filter(([, definition]) => state.modules[definition.module] === true)
    .map(([name, definition]) => ({ name, description: definition.description, parameters: definition.parameters }));
}

async function safePath(root, relativePath = '.') {
  if (typeof relativePath !== 'string' || path.isAbsolute(relativePath) ||
      relativePath.split(/[\\/]/).includes('..') || relativePath.includes('\0')) {
    throw new Error('Path must stay inside the workspace');
  }
  const actualRoot = await fs.realpath(root);
  const actual = await fs.realpath(path.resolve(actualRoot, relativePath));
  const rel = path.relative(actualRoot, actual);
  if (rel === '..' || rel.startsWith(`..${path.sep}`) || path.isAbsolute(rel)) {
    throw new Error('Path escapes the workspace');
  }
  return actual;
}

function validNoteKey(key) {
  if (typeof key !== 'string' || !/^[a-zA-Z0-9][a-zA-Z0-9_-]{0,39}$/.test(key)) {
    throw new Error('Note key must be 1-40 simple characters');
  }
}

export async function callTool({ home, workspaceRoot, state, name, args = {} }) {
  const definition = definitions[name];
  if (!definition || state.modules[definition.module] !== true) throw new Error(`Tool is not enabled: ${name}`);
  if (!args || typeof args !== 'object' || Array.isArray(args)) throw new Error('Tool arguments must be an object');
  if (name === 'workspace_list') {
    const target = await safePath(workspaceRoot, args.directory ?? '.');
    const items = (await fs.readdir(target, { withFileTypes: true })).slice(0, 40);
    return items.map(item => ({ name: item.name, type: item.isSymbolicLink() ? 'link' : item.isDirectory() ? 'directory' : 'file' }));
  }
  if (name === 'workspace_read') {
    const target = await safePath(workspaceRoot, args.file);
    const stat = await fs.stat(target);
    if (!stat.isFile() || stat.size > 64 * 1024) throw new Error('Only text files up to 64 KiB can be read');
    return await fs.readFile(target, 'utf8');
  }
  if (name === 'notes_save') {
    validNoteKey(args.key);
    if (typeof args.text !== 'string' || Buffer.byteLength(args.text) > 4096) throw new Error('Note exceeds 4 KiB');
    // Reload to avoid overwriting unrelated updates made outside the current turn.
    const current = await loadState(home);
    if (current.modules.notes !== true) throw new Error('Notes module disabled');
    current.notes[args.key] = args.text;
    await saveState(home, current);
    return { saved: args.key };
  }
  validNoteKey(args.key);
  const current = await loadState(home);
  return { key: args.key, text: current.notes[args.key] ?? null };
}
