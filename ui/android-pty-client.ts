/**
 * Android-only interactive COS-style terminal frontend.
 * Separate bundle: never loaded by the Windows renderer. No AI/Goal/MCP path
 * imports this module. Opening the Terminal workspace is the user's explicit
 * action to start the first shell; closing the last tab replaces it once.
 */
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

type PtyEvent = {
  id: string;
  kind: 'output' | 'exit';
  data?: string;
  bytes?: number;
  exitCode?: number;
};
type Tab = {
  id: string;
  projectId: string | null;
  node: HTMLDivElement;
  term: Terminal;
  fit: FitAddon;
  title: string;
  started: boolean;
  exited: boolean;
  writes: Promise<void>;
};
type PtyStatus = {
  supported: boolean; kind: string; shell: string;
  active: number; maxSessions: number;
  userInitiatedRequired: boolean; automatedCommandsAllowed: boolean;
  sandboxed: boolean; termuxRequired: boolean;
};

const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T;
const tabs = new Map<string, Tab>();
const layoutStorageKey = 'mcp-agent-ai-android-terminal-tab-layout-v1';
let active: string | null = null;
let booted = false;
let ready = false;
let projectId: string | null = null;
let listenerReady = false;
let pendingUserOpen = false;
const idPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const status = (message: string) => { $('androidPtyStatus').textContent = message; };
const current = () => active ? tabs.get(active) : undefined;
// Store only layout metadata. Terminal output/commands can be sensitive and
// must never be copied into localStorage to retain the tab names.
function saveLayout(): void {
  try {
    const names = [...tabs.values()].map(tab => ({
      title: tab.title, projectId: tab.projectId
    }));
    localStorage.setItem(layoutStorageKey, JSON.stringify({
      version: 1, tabs: names,
      selected: Math.max(0, [...tabs.keys()].indexOf(active ?? ''))
    }));
  } catch { /* Private-mode storage can be unavailable; in-memory tabs remain. */ }
}
function nextTitle(): string {
  const used = new Set([...tabs.values()].map(tab => tab.title));
  for (let index = 1; index <= 4; index++)
    if (!used.has('sh ' + index)) return 'sh ' + index;
  return 'sh';
}
function revealFocusedCommand(): void {
  const command = $<HTMLInputElement>('androidPtyCommand');
  // Only the user's tap on this visible input should open Android's keyboard.
  // Reveal it after the WebView resizes, not while switching shell tabs.
  const reveal = () => {
    if (current()?.started && !current()?.exited &&
        document.activeElement === command &&
        document.visibilityState === 'visible' &&
        !$('toolTerminal').classList.contains('hidden')) {
      command.scrollIntoView({behavior:'auto',block:'center'});
    }
  };
  requestAnimationFrame(reveal);
  window.setTimeout(reveal, 140);
  window.setTimeout(reveal, 360);
}
const controls = () => {
  $('androidPtyNew',).toggleAttribute('disabled', !ready || tabs.size >= 4);
  const hasRunning = !!current()?.started && !current()?.exited;
  const restart = $<HTMLButtonElement>('androidPtyRestart');
  restart.hidden = !current()?.exited;
  restart.disabled = !ready || !current()?.exited;
  restart.setAttribute('aria-label', 'Restart terminal ' + (current()?.title || 'selected'));
  $('androidPtyCommand').toggleAttribute('disabled', !hasRunning);
  $('androidPtyMobileForm').querySelector('button')?.toggleAttribute('disabled', !hasRunning);
  $('androidPtyCopy').toggleAttribute('disabled', !current()?.term.hasSelection());
  $('androidPtyToChat').toggleAttribute('disabled', !current()?.term.hasSelection());
  document.querySelectorAll<HTMLButtonElement>('#androidPtyWorkspace [data-pty-key]')
    .forEach(button => { button.disabled = !hasRunning; });
};
function fitActive(): void {
  const tab = current();
  if (!tab || !tab.node.isConnected || $('toolTerminal').classList.contains('hidden')) return;
  if (tab.node.getBoundingClientRect().width < 20) return;
  try {
    tab.fit.fit();
    if (tab.started && !tab.exited) void invoke('android_pty_resize', {
      id:tab.id, cols:Math.min(300,Math.max(2,tab.term.cols)),
      rows:Math.min(150,Math.max(1,tab.term.rows))
    }).catch(() => status('Terminal resize could not be confirmed.'));
  } catch { /* Dimensions may be zero before the workspace becomes visible. */ }
}
function paint(): void {
  const bar = $('androidPtyTabs');
  bar.replaceChildren();
  for (const tab of tabs.values()) {
    tab.node.hidden = tab.id !== active;
    const wrapper = document.createElement('div');
    wrapper.className = 'android-pty-tab';
    wrapper.setAttribute('role','tab');
    wrapper.setAttribute('aria-selected', String(tab.id === active));
    wrapper.dataset.terminalState = tab.exited ? 'stopped' : 'running';
    const select = document.createElement('button');
    select.type = 'button';
    select.textContent = tab.title;
    select.setAttribute('aria-label','Select terminal ' + tab.title);
    select.title = tab.exited ? 'Session stopped; select this tab to see Restart' : tab.title;
    select.addEventListener('click', () => {
      // Tab selection only changes the displayed shell. If the command field
      // was focused, dismiss the IME; never focus the next tab automatically.
      const focused = document.activeElement;
      if (focused instanceof HTMLInputElement ||
          focused instanceof HTMLTextAreaElement ||
          (focused instanceof HTMLElement && focused.isContentEditable)) {
        focused.blur();
      }
      active = tab.id; paint(); requestAnimationFrame(fitActive);
      if (tab.exited)
        status(tab.title + ' stopped. Select Restart to create a new local shell.');
      else
        status(tab.title + ' · local shell running');
    });
    const close = document.createElement('button');
    close.type = 'button'; close.textContent = '×';
    close.setAttribute('aria-label','Close terminal ' + tab.title);
    close.disabled = !tab.started && !tab.exited;
    close.addEventListener('click', () => void closeTab(tab));
    wrapper.append(select,close); bar.append(wrapper);
  }
  controls();
  saveLayout();
}
function createTab(id: string, title: string, folder: string | null): Tab {
  const tabNode = document.createElement('div');
  tabNode.className = 'android-pty-terminal';
  $('androidPtyScreen').append(tabNode);
  const term = new Terminal({
    cols:80, rows:24, cursorBlink:true, fontFamily:'monospace',
    fontSize:12, scrollback:1200, allowTransparency:false,
    theme:{background:'#0b111b',foreground:'#e3eaf7',cursor:'#dee9f9'},
    screenReaderMode:true
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(tabNode);
  const tab: Tab = {
    id, projectId:folder, node:tabNode, term, fit,
    title, started:false, exited:false, writes:Promise.resolve()
  };
  term.onData(data => queueWrite(tab,data));
  term.onSelectionChange(controls);
  return tab;
}
function restoreLayout(): void {
  if (tabs.size) return;
  try {
    const raw = localStorage.getItem(layoutStorageKey);
    if (!raw || raw.length > 1024) return;
    const saved = JSON.parse(raw) as {
      version?: number;
      selected?: number;
      tabs?: { title?: string; projectId?: string | null }[];
    };
    if (saved.version !== 1 || !Array.isArray(saved.tabs) ||
        saved.tabs.length > 4 || saved.tabs.length < 1) return;
    const titles = new Set<string>();
    for (const item of saved.tabs) {
      if (typeof item?.title !== 'string' || !/^sh [1-4]$/.test(item.title) ||
          titles.has(item.title) ||
          (item.projectId != null && !idPattern.test(item.projectId))) return;
      titles.add(item.title);
    }
    for (const item of saved.tabs) {
      const id = crypto.randomUUID().toLowerCase();
      const tab = createTab(id, item.title!, item.projectId ?? null);
      tab.exited = true;
      tab.term.write('[Terminal tab restored. Previous shell is no longer connected. Tap Restart to start a new shell.]\r\n');
      tabs.set(id, tab);
    }
    const index = Number.isInteger(saved.selected) ? Math.max(0, Math.min(tabs.size-1, saved.selected!)) : 0;
    active = [...tabs.keys()][index];
    paint();
    status('Previous terminal tabs restored as stopped. Tap Restart to open a new local shell.');
  } catch { /* Reject malformed/unavailable storage, do not start shells. */ }
}
function stopTab(tab: Tab, message: string): void {
  if (tab.exited || tabs.get(tab.id) !== tab) return;
  tab.exited = true;
  tab.term.write('\r\n[' + message + ']\r\n');
  if (tab.id === active) status('Shell stopped. Select Restart to create a new session in this tab.');
  paint();
}
async function reconcileStoppedTabs(): Promise<void> {
  if (!ready || !tabs.size || document.hidden) return;
  const candidates = [...tabs.values()].filter(tab => tab.started && !tab.exited);
  if (!candidates.length) return;
  try {
    const receipt = await invoke<PtyStatus>('android_pty_status');
    if (receipt.kind !== 'android-in-app-pty') return;
    if (receipt.active === 0) {
      for (const tab of candidates)
        stopTab(tab, 'Shell stopped when Android left the foreground. Tab and scrollback retained.');
    }
  } catch { status('Shell state could not be verified; no commands were started.'); }
}
function queueWrite(tab: Tab, data: string): void {
  if (!tab.started || tab.exited || !data) return;
  const bytes = new TextEncoder().encode(data);
  if (bytes.length > 65_536) {
    status('Input exceeds 64 KiB. Nothing was sent.');
    return;
  }
  // Keep input ordered and within the Rust bridge's per-call 4096-byte bound.
  const chunks: string[] = [];
  let part = '';
  let size = 0;
  for (const symbol of data) {
    const length = new TextEncoder().encode(symbol).length;
    if (size + length > 4096) { chunks.push(part); part = ''; size = 0; }
    part += symbol; size += length;
  }
  if (part) chunks.push(part);
  for (const chunk of chunks) {
    tab.writes = tab.writes.then(async () => {
      if (tab.exited || !tabs.has(tab.id)) return;
      await invoke('android_pty_write',{id:tab.id,data:chunk,userInitiated:true});
    }).catch((error) => { status('Terminal input failed: ' + String(error).slice(0,130)); });
  }
}
function closeLocal(tab: Tab): void {
  if (!tabs.has(tab.id)) return;
  tabs.delete(tab.id);
  tab.exited = true;
  tab.term.dispose();
  tab.node.remove();
  if (active === tab.id) active = [...tabs.keys()].at(-1) ?? null;
  paint(); requestAnimationFrame(fitActive);
  if (!tabs.size) status('No active shell. Select + New to open one.');
}
async function restartTab(tab: Tab): Promise<void> {
  if (!ready || !tab.exited || tabs.get(tab.id) !== tab ||
      document.hidden || $('toolTerminal').classList.contains('hidden')) return;
  // Recreate the shell only from this explicit button. A previously running
  // command cannot be recovered; preserve its visible scrollback, not its PID.
  const oldId = tab.id;
  const id = crypto.randomUUID().toLowerCase();
  const reordered = [...tabs.entries()].map(([key, value]) =>
    [key === oldId ? id : key, value] as const);
  tabs.clear();
  for (const [key,value] of reordered) tabs.set(key,value);
  tab.id = id;
  tab.started = false;
  tab.exited = false;
  tab.writes = Promise.resolve();
  if (active === oldId) active = id;
  tab.term.write('\r\n[Starting a new local shell session]\r\n');
  paint();
  status('Restarting ' + tab.title + '…');
  try {
    const receipt = await invoke<{id:string;started:boolean;cwd:string;shell:string}>(
      'android_pty_create', {
        id, projectId:tab.projectId,
        cols:Math.min(300,Math.max(2,tab.term.cols)),
        rows:Math.min(150,Math.max(1,tab.term.rows)),
        userInitiated:true
      });
    if (receipt.id !== id || receipt.started !== true || receipt.shell !== 'sh')
      throw Error('Native Android shell returned an invalid restart receipt.');
    tab.started = true;
    status('Local sh · ' + receipt.cwd + ' · manually controlled');
    paint();
    requestAnimationFrame(fitActive);
  } catch (error) {
    stopTab(tab, 'Unable to restart shell');
    status('Restart failed: ' + String(error).slice(0,160));
  }
}
async function closeTab(tab: Tab): Promise<void> {
  if (!tabs.has(tab.id) || (!tab.started && !tab.exited)) return;
  if (tab.started && !tab.exited &&
      !window.confirm('Close this shell and stop its running processes?')) return;
  if (tab.started && !tab.exited) {
    try { await invoke('android_pty_close',{id:tab.id,userInitiated:true}); }
    catch (error) { status('Terminal close could not be confirmed: ' + String(error).slice(0,100)); return; }
  }
  const wasLastTab = tabs.size === 1;
  closeLocal(tab);
  // A person's explicit close (and stop confirmation when running) keeps the
  // COS-style terminal workspace ready. Lifecycle/child exits do NOT restart
  // shells: only this user action can replace the last tab.
  if (wasLastTab && ready && document.visibilityState === 'visible' &&
      !$('toolTerminal').classList.contains('hidden')) {
    await openNew();
  }
}
function onNativeEvent(event: PtyEvent): void {
  if (!event || typeof event.id !== 'string' || !idPattern.test(event.id)) return;
  const tab = tabs.get(event.id);
  if (!tab) return;
  if (event.kind === 'output') {
    if (typeof event.data !== 'string' || event.data.length > 5500 ||
        !Number.isInteger(event.bytes) || event.bytes! < 1 || event.bytes! > 4096) return;
    let bytes: Uint8Array;
    try {
      const decoded = atob(event.data);
      if (decoded.length !== event.bytes) return;
      bytes = Uint8Array.from(decoded, char => char.charCodeAt(0));
    } catch { return; }
    tab.term.write(bytes, () => {
      void invoke('android_pty_ack',{id:tab.id,count:bytes.length}).catch(() => {});
      if (tab.id === active) controls();
    });
  } else if (event.kind === 'exit') {
    stopTab(tab, 'Shell exited · ' + String(event.exitCode ?? '?'));
  }
}
async function openNew(): Promise<void> {
  if (!ready || tabs.size >= 4 || document.visibilityState !== 'visible' ||
      $('toolTerminal').classList.contains('hidden')) return;
  // Creating the first shell, pressing + New, or replacing the last closed
  // tab must NOT open the Android soft keyboard. In particular, never focus
  // xterm's hidden textarea during shell creation. A user tap on an existing
  // sh tab (or on the command input itself) is the separate typing gesture.
  const focused = document.activeElement;
  if (focused instanceof HTMLElement) focused.blur();
  const id = crypto.randomUUID().toLowerCase();
  const tab = createTab(id,nextTitle(),projectId);
  tabs.set(id,tab);
  active = id;
  paint(); requestAnimationFrame(fitActive);
  status('Starting a local /system/bin/sh session…');
  try {
    const receipt = await invoke<{
      id:string;started:boolean;cwd:string;shell:string;
    }>('android_pty_create',{
      id,projectId:tab.projectId,
      cols:Math.min(300,Math.max(2,tab.term.cols)),
      rows:Math.min(150,Math.max(1,tab.term.rows)),
      userInitiated:true
    });
    if (receipt.id !== id || receipt.started !== true || receipt.shell !== 'sh')
      throw Error('Native Android shell did not return a matching receipt.');
    tab.started = true;
    status('Local sh · ' + receipt.cwd + ' · manually controlled');
    // The tab's close button is intentionally disabled while spawn is
    // pending. Render again when the native receipt makes it safe to close.
    paint(); requestAnimationFrame(fitActive);
  } catch (error) {
    closeLocal(tab);
    // Do not retry automatically on a failed spawn or stale Android focus.
    status('Shell did not start: ' + String(error).slice(0,180));
  }
}
async function copySelection(): Promise<void> {
  const text = current()?.term.getSelection() || '';
  if (!text) return;
  try { await navigator.clipboard.writeText(text.slice(0,4096)); status('Selected text copied.'); }
  catch { status('Clipboard unavailable.'); }
}
function selectedToChat(): void {
  const text = current()?.term.getSelection() || '';
  const composer = document.getElementById('prompt') as HTMLTextAreaElement | null;
  if (!composer || !text) return;
  const excerpt = text.slice(0,4096);
  if (!window.confirm('Insert the selected terminal output in the chat draft? It could contain private information. Nothing will be sent automatically.')) return;
  const joined = [composer.value,excerpt].filter(Boolean).join('\n');
  if (joined.length > composer.maxLength) { status('Chat draft is full. Nothing was inserted.'); return; }
  composer.value = joined;
  composer.dispatchEvent(new Event('input',{bubbles:true}));
  status('Selected terminal output added to chat draft. Not sent.');
}
async function boot(): Promise<void> {
  if (booted) { requestAnimationFrame(fitActive); return; }
  booted = true;
  const stylesheet = document.createElement('link');
  stylesheet.rel = 'stylesheet'; stylesheet.href = './android-xterm.css';
  document.head.append(stylesheet);
  $('androidPtyNew').addEventListener('click', () => void openNew());
  $('androidPtyRestart').addEventListener('click', () => {
    const tab = current();
    if (tab?.exited) void restartTab(tab);
  });
  $<HTMLInputElement>('androidPtyCommand').addEventListener('focus', revealFocusedCommand);
  $('androidPtyCopy').addEventListener('click', () => void copySelection());
  $('androidPtyToChat').addEventListener('click', selectedToChat);
  $('androidPtyMobileForm').addEventListener('submit', event => {
    event.preventDefault();
    const tab = current();
    const input = $<HTMLInputElement>('androidPtyCommand');
    if (!tab || !tab.started || tab.exited || !input.value) return;
    queueWrite(tab,input.value+'\r');
    input.value = '';
  });
  const commands: Record<string,string> = {
    escape:'\x1b', tab:'\t', up:'\x1b[A', down:'\x1b[B',
    left:'\x1b[D', right:'\x1b[C', interrupt:'\x03'
  };
  document.querySelectorAll<HTMLButtonElement>('#androidPtyWorkspace [data-pty-key]').forEach(button => {
    button.addEventListener('click', () => {
      const tab = current(); if (!tab) return;
      queueWrite(tab,commands[button.dataset.ptyKey || ''] || '');
    });
  });
  new ResizeObserver(() => requestAnimationFrame(fitActive)).observe($('androidPtyScreen'));
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) {
      // Android native may stop PTYs when the Activity loses foreground
      // focus. Do not DELETE tabs and scrollback merely because the UI was
      // hidden; reconcile them as stopped when the user comes back.
      const focused = document.activeElement;
      if (focused instanceof HTMLElement) focused.blur();
    } else {
      void reconcileStoppedTabs();
    }
  });
  window.addEventListener('focus', () => void reconcileStoppedTabs());
  controls();
  try {
    const receipt = await invoke<PtyStatus>('android_pty_status');
    if (receipt.supported !== true || receipt.kind !== 'android-in-app-pty' ||
        receipt.shell !== '/system/bin/sh' || receipt.termuxRequired !== false ||
        receipt.automatedCommandsAllowed !== false || receipt.sandboxed !== false ||
        receipt.userInitiatedRequired !== true || receipt.maxSessions !== 4)
      throw Error('Installed Android native PTY backend not available.');
    await listen<PtyEvent>('mcp-android-pty', message => onNativeEvent(message.payload));
    listenerReady = true;
    ready = true;
    restoreLayout();
    if (!tabs.size) status(receipt.active > 0
      ? 'Previous shells may still be exiting. Close the app and reopen before starting a new session.'
      : 'Android local sh ready.');
    if (tabs.size) pendingUserOpen = false;
    if (pendingUserOpen && receipt.active === 0 && tabs.size === 0 &&
        document.visibilityState === 'visible' &&
        !$('toolTerminal').classList.contains('hidden')) {
      pendingUserOpen = false;
      await openNew();
    }
  } catch (error) { status('Interactive Android shell unavailable: ' + String(error).slice(0,145)); }
  controls();
}
function show(nextProjectId: string | null, userOpenedTerminal: boolean): void {
  projectId = nextProjectId && idPattern.test(nextProjectId) ? nextProjectId : null;
  if (userOpenedTerminal) pendingUserOpen = true;
  if (booted) {
    // Switching between Workspace views is not a terminal close. Keep all
    // tabs/scrollback and check native liveness without starting anything.
    void reconcileStoppedTabs().then(() => {
      if (userOpenedTerminal && ready && tabs.size === 0 &&
          document.visibilityState === 'visible' &&
          !$('toolTerminal').classList.contains('hidden')) {
        pendingUserOpen = false;
        void openNew();
      } else if (tabs.size) {
        pendingUserOpen = false;
      }
    });
    requestAnimationFrame(fitActive);
  }
}
// The renderer passes a trusted Terminal-opening gesture; no background start.
(window as any).__mcpAndroidPty = {boot,show};
