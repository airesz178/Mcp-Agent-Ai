import { invoke, isTauri } from '@tauri-apps/api/core';
type Message = { role: 'user' | 'assistant'; text: string };
type Session = { id: string; provider: string; model?: string; messages: Message[]; actions?: unknown[] };
type AppState = {
  platform?: 'windows' | 'android';
  modules: Record<string, boolean>;
  availableModules?: string[];
  skillsAvailable?: boolean;
  skills: { id: string; name?: string; enabled: boolean }[];
  sessions: Session[];
  providers: string[];
  hasKeys: string[];
  workspace: string;
  toolPanels?: { review: boolean; terminal: boolean; files: boolean; subagents: boolean };
};
const $ = (id: string): any => document.getElementById(id)!;
// Native bridge discovery must happen before the first asynchronous state
// request. Otherwise a cold Android launch briefly shows the oversized
// browser/desktop layout while the Rust API starts.
// Set the compact layout from the Android user agent immediately. Tauri's
// injected bridge can appear *after* our frontend runs on some cold launches;
// waiting for it causes a flash of the much larger desktop welcome screen.
const nativeAndroidRuntime = /Android/i.test(navigator.userAgent);
if (nativeAndroidRuntime) document.documentElement.classList.add('native-android');
// ChatGPT OAuth starts as the default. Explicit API-key providers can be
// selected separately for real model function calls and Android local tools.
if (nativeAndroidRuntime) {
  const providerSelect = document.querySelector<HTMLSelectElement>('#provider');
  if (providerSelect) {
    for (const option of [...providerSelect.options])
      if (!['chatgpt-browser','openai','anthropic','openrouter'].includes(option.value)) option.remove();
    providerSelect.value = 'chatgpt-browser';
  }
  const modelField = document.querySelector<HTMLInputElement>('#model');
  if (modelField) modelField.value = 'chatgpt.com';
  // Neither legacy Setup card is part of Android Settings any more. Retain
  // the nodes for state bindings and the unchanged Windows interface.
  for (const id of ['connectionSection', 'backgroundChatgptSettings']) {
    const section = document.getElementById(id);
    if (section) {
      section.hidden = true;
      section.inert = true;
      section.setAttribute('aria-hidden', 'true');
    }
  }
}
if (nativeAndroidRuntime) {
  // Some Android WebViews keep CSS 100dvh at the pre-IME size even when the
  // visual viewport shrinks. Bound the single app grid to the actual visible
  // area; never translate the composer (which would desync hit targets).
  const syncAndroidViewport = () => {
    const viewport = window.visualViewport;
    const visible = viewport && viewport.scale === 1 ?
      Math.min(window.innerHeight, viewport.height + viewport.offsetTop) : window.innerHeight;
    if (!(visible > 160) || !Number.isFinite(visible)) return;
    document.documentElement.style.setProperty('--android-live-viewport',`${Math.round(visible)}px`);
    const editable = document.activeElement instanceof HTMLInputElement ||
      document.activeElement instanceof HTMLTextAreaElement;
    document.documentElement.classList.toggle('android-ime-visible',
      editable && window.innerHeight - visible > 100);
  };
  window.visualViewport?.addEventListener('resize',syncAndroidViewport);
  window.visualViewport?.addEventListener('scroll',syncAndroidViewport);
  window.addEventListener('resize',syncAndroidViewport);
  document.addEventListener('focusin', () => {
    syncAndroidViewport();
    window.setTimeout(syncAndroidViewport, 120);
    window.setTimeout(syncAndroidViewport, 360);
  });
  document.addEventListener('focusout', () => window.setTimeout(syncAndroidViewport, 120));
  syncAndroidViewport();
  // The COS renderer owns one full-width topbar above the sidebar/chat grid.
  // Reuse the existing header on Android only; don't clone its two controls.
  const shell = document.querySelector<HTMLElement>('.shell')!;
  const topbar = shell.querySelector<HTMLElement>('.main > .topbar')!;
  shell.insertBefore(topbar, shell.querySelector('.sidebar'));
  (shell.querySelector('#sidebar') as HTMLElement).inert = true;
  document.addEventListener('pointerdown', () => {
    document.documentElement.dataset.inputMode = 'pointer';
  }, { capture: true });
  document.addEventListener('keydown', (event) => {
    // Retain visible focus for real hardware-keyboard navigation.
    if (['Tab', 'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight'].includes(event.key))
      document.documentElement.dataset.inputMode = 'keyboard';
  }, { capture: true });
}
let state: AppState = { modules: {}, skills: [], sessions: [], providers: [], hasKeys: [], workspace: '' };
let selectedSession: string | null = null;
// General is a device-local UI preference, separate from API credentials and
// the Setup/Workspace native permission receipts. Nothing sensitive is saved.
type AndroidGeneralPreferences = {
  followOutput:boolean;
  notifyOnFinish:boolean;
  notificationDetail:'task_finished'|'full_message';
  notificationPriority:'foreground'|'background'|'both';
  messageDuringTurn:'steer'|'queue';
};
const androidGeneralStorageKey = 'mcp-agent-ai-android-general-v1';
const androidGeneralDefaults:AndroidGeneralPreferences = {
  followOutput:true, notifyOnFinish:false,
  notificationDetail:'task_finished', notificationPriority:'both', messageDuringTurn:'queue'
};
function parseAndroidGeneralPreferences(raw:unknown):AndroidGeneralPreferences {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) throw Error('Invalid backup');
  const data = raw as Record<string,unknown>;
  if (data.version === 1 &&
      typeof data.followOutput === 'boolean' &&
      (data.sendShortcut === 'modifier' || data.sendShortcut === 'enter') &&
      typeof data.soundCues === 'boolean' &&
      Object.keys(data).every(key => ['version','followOutput','sendShortcut','soundCues'].includes(key)))
    return {...androidGeneralDefaults,followOutput:data.followOutput};
  if ((data.version !== 2 && data.version !== 3) || typeof data.followOutput !== 'boolean' ||
      typeof data.notifyOnFinish !== 'boolean' ||
      (data.notificationDetail !== 'task_finished' && data.notificationDetail !== 'full_message') ||
      (data.messageDuringTurn !== 'steer' && data.messageDuringTurn !== 'queue') ||
      Object.keys(data).some(key =>
        !['version','followOutput','notifyOnFinish','notificationDetail','messageDuringTurn',
          ...(data.version === 3 ? ['notificationPriority'] : [])].includes(key)))
    throw Error('Backup must contain only supported General preferences (version 2 or 3)');
  const notificationPriority = data.version === 2 ? 'both' : data.notificationPriority;
  if (notificationPriority !== 'foreground' && notificationPriority !== 'background' &&
      notificationPriority !== 'both')
    throw Error('Invalid notification priority');
  return {
    followOutput:data.followOutput,
    notifyOnFinish:data.notifyOnFinish,
    notificationDetail:data.notificationDetail,
    notificationPriority,
    messageDuringTurn:data.messageDuringTurn
  };
}
function loadAndroidGeneralPreferences():AndroidGeneralPreferences {
  if (!nativeAndroidRuntime) return {...androidGeneralDefaults};
  try {
    const value = localStorage.getItem(androidGeneralStorageKey);
    return value ? parseAndroidGeneralPreferences(JSON.parse(value)) : {...androidGeneralDefaults};
  } catch { return {...androidGeneralDefaults}; }
}
let androidGeneralPreferences = loadAndroidGeneralPreferences();
let androidGeneralChatScrolledUp = false;
let androidGeneralChatSessionId:string|null = null;
let androidGeneralProgrammaticScroll = false;
function androidGeneralBackupJson():string {
  return JSON.stringify({version:3,...androidGeneralPreferences},null,2);
}
function saveAndroidGeneralPreferences(next:AndroidGeneralPreferences):boolean {
  const hadFollowOutput = androidGeneralPreferences.followOutput;
  try {
    localStorage.setItem(androidGeneralStorageKey,JSON.stringify({version:3,...next}));
    androidGeneralPreferences = {...next};
    if (!hadFollowOutput && androidGeneralPreferences.followOutput) {
      androidGeneralChatScrolledUp = false;
      const transcript = $('messages');
      androidGeneralProgrammaticScroll = true;
      transcript.scrollTop = transcript.scrollHeight;
      requestAnimationFrame(() => {androidGeneralProgrammaticScroll = false;});
    }
    renderAndroidGeneralSettings();
    return true;
  } catch {
    renderAndroidGeneralSettings();
    return false;
  }
}
function androidGeneralFollowOutput():boolean {
  return !nativeAndroidRuntime || androidGeneralPreferences.followOutput;
}
const androidTurnNotificationClaims = new Set<string>();
async function sendAndroidFinishedNotification(answer:string,requestId:string):Promise<void> {
  if (!nativeAndroidRuntime || !isTauri() ||
      !androidGeneralPreferences.notifyOnFinish || !requestId ||
      androidTurnNotificationClaims.has(requestId)) return;
  // Claim before the OS call, including permission errors or uncertain replies,
  // so a transient UI refresh never posts a duplicate notification.
  androidTurnNotificationClaims.add(requestId);
  if (androidTurnNotificationClaims.size > 256) {
    const first = androidTurnNotificationClaims.values().next().value;
    if (first) androidTurnNotificationClaims.delete(first);
  }
  const mode = androidGeneralPreferences.notificationDetail;
  try {
    const result = await invoke<{posted:boolean;reason?:string}>('android_turn_notification',{
      mode,
      message:mode === 'full_message' ? answer : '',
      notificationPriority:androidGeneralPreferences.notificationPriority,
      userInitiated:true
    });
    if (result?.posted !== true && nativeAndroidRuntime &&
        ['permission_denied','channel_disabled'].includes(result?.reason || '')) {
      $('androidGeneralBackupStatus').textContent =
        'Android notifications are disabled. Open General → Notifications → Setup to allow them.';
    }
  } catch { /* A notification never changes the confirmed chat result. */ }
}
let speechContextRevision = 0; // Discard dictation if user switches chat while Android picker is open.
let speechDictationBusy = false;
let working = false;
let activeChatRequestId: string | null = null;
let streamProgressTimer: ReturnType<typeof setTimeout> | null = null;
function stopChatStreamPolling() {
  if (streamProgressTimer !== null) clearTimeout(streamProgressTimer);
  streamProgressTimer = null;
}
// Poll a bounded, request-scoped Rust buffer. In-flight polls cannot paint a
// later chat after cancellation, navigation or a new request.
async function pollChatStream(requestId: string) {
  if (!working || activeChatRequestId !== requestId) return;
  try {
    const result = await api('/api/chat-progress', {requestId});
    if (!working || activeChatRequestId !== requestId) return;
    if (result.active && typeof result.text === 'string' && result.text.length) {
      const bubble = document.querySelector('.message.pending .bubble');
      if (bubble) {
        bubble.querySelector('.typing')?.remove();
        const body = bubble.querySelector('.body') ?? el('div', 'body');
        if (!body.isConnected) bubble.append(body);
        body.textContent = result.text;
        if (androidGeneralFollowOutput())
          $('messages').scrollTop = $('messages').scrollHeight;
      }
    }
  } catch { /* Chat response remains authoritative; polling may be unavailable. */ }
  if (working && activeChatRequestId === requestId)
    streamProgressTimer = setTimeout(() => void pollChatStream(requestId), 240);
}
function createChatRequestId(): string {
  const id = new Uint8Array(16);
  crypto.getRandomValues(id);
  id[6] = (id[6] & 0x0f) | 0x40;
  id[8] = (id[8] & 0x3f) | 0x80;
  const hex = Array.from(id, byte => byte.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0,8)}-${hex.slice(8,12)}-${hex.slice(12,16)}-${hex.slice(16,20)}-${hex.slice(20)}`;
}
let filesCurrentPath = '';
let workspacePanel: string | null = null;
let extraView: string | null = null;
// This is last-request verification, not a claim of an always-open network link.
// Credentials existing in memory alone never mean the provider is connected.
let verifiedAiConnection: { provider: string; model: string } | null = null;
// Official Android Sign in with ChatGPT plan inference is NOT authenticated
// using the separate chatgpt.com WebView profile.
let verifiedSiwcChat = false;
function updateSidebarConnection() {
  const badge = $('sidebarConnectionStatus');
  const provider = $('provider').value;
  const model = $('model').value.trim();
  const credentialsReady = ['ollama', 'hermes', 'chatgpt-browser'].includes(provider) ||
    !!state.hasKeys?.includes(provider);
  const connected = nativeAndroidRuntime
    ? provider === 'chatgpt-browser' && verifiedSiwcChat && androidCosOAuthLinked() &&
      verifiedAiConnection?.provider === 'chatgpt-browser' &&
      verifiedAiConnection?.model === 'chatgpt.com'
    : credentialsReady && !!verifiedAiConnection &&
      verifiedAiConnection.provider === provider && verifiedAiConnection.model === model;
  badge.dataset.status = connected ? 'connected' : 'disconnected';
  badge.querySelector('.sidebar-connection-label')!.textContent = connected ? 'Connected' : 'Disconnected';
  if (nativeAndroidRuntime) {
    $('androidConnectionPopoverTitle').textContent = connected ? 'ChatGPT reply verified' : 'No verified reply';
    $('androidConnectionPopoverDot').dataset.status = connected ? 'connected' : 'disconnected';
  }
  badge.title = connected
    ? 'Last ChatGPT website reply verified in this app session'
    : 'No successful ChatGPT website reply verified in this app session';
  if (nativeAndroidRuntime) $('androidConnectionStatusDetail').textContent = connected
    ? 'ChatGPT OAuth plan reply verified through Responses API. This is not a continuously monitored connection.'
    : 'No ChatGPT assistant reply verified yet. ChatGPT app authorization is in Step 3; the unrelated MCP tunnel is in Step 8.';
  if (nativeAndroidRuntime) renderAndroidCoreConnectionPopover();
  if (nativeAndroidRuntime) renderAndroidCosSetup();
}
function renderAndroidCoreConnectionPopover():void {
  if(!nativeAndroidRuntime)return;
  const report=androidOpenAiTunnelSnapshot;
  const verified=androidCorePluginVerified();
  const active=!!report&&['connecting','polling','tool_verified'].includes(report.state);
  $('androidConnectionCoreStatus').textContent=androidOpenAiTunnelUnavailable
    ? 'Native Core status unavailable'
    : !report?'No current Core receipt'
    : verified?'Core linked · inbound MCP tool acknowledgement recorded'
    : active?'Tunnel '+report.state+' · waiting for a verified MCP call'
    : report.state==='error'?'Tunnel error · check Setup':'Core disconnected';
  const toggle=$('androidConnectionCoreToggle') as HTMLButtonElement;
  toggle.textContent=active?'Disconnect Core':
    ($('androidOpenAiTunnelConnect') as HTMLButtonElement).disabled?'Configure Core':'Connect Core';
  toggle.disabled=androidOpenAiTunnelBusy;
  toggle.dataset.active=String(active);
}
$('sidebarConnectionStatus').addEventListener('click', () => {
  if (!nativeAndroidRuntime) return;
  updateSidebarConnection();
  const open = $('androidConnectionStatusPopover').classList.contains('hidden');
  $('androidConnectionStatusPopover').classList.toggle('hidden', !open);
  $('sidebarConnectionStatus').setAttribute('aria-expanded', String(open));
  if(open){
    renderAndroidCoreConnectionPopover();
    void Promise.allSettled([refreshAndroidOpenAiTunnel(true),
      refreshAndroidTunnelSavedId(),refreshAndroidTunnelCredentials()]).then(()=>
      renderAndroidCoreConnectionPopover());
  }
});
$('androidConnectionCoreToggle').addEventListener('click',()=>{
  if(!nativeAndroidRuntime||!isTauri()||androidOpenAiTunnelBusy)return;
  const active=$('androidConnectionCoreToggle').dataset.active==='true';
  if(active){
    ($('androidOpenAiTunnelDisconnect') as HTMLButtonElement).click();
  }else if(!($('androidOpenAiTunnelConnect') as HTMLButtonElement).disabled){
    ($('androidOpenAiTunnelConnect') as HTMLButtonElement).click();
  }else{
    closeAndroidConnectionPopover();
    setSettings(true);setAndroidSettingsView('setup');
    chooseAndroidCosSetupStep('connect');
  }
  renderAndroidCoreConnectionPopover();
});
function closeAndroidConnectionPopover() {
  $('androidConnectionStatusPopover').classList.add('hidden');
  $('sidebarConnectionStatus').setAttribute('aria-expanded', 'false');
}
$('androidConnectionClose').addEventListener('click', closeAndroidConnectionPopover);
$('androidConnectionOpenSetup').addEventListener('click', () => {
  closeAndroidConnectionPopover();
  setSettings(true);
  setAndroidSettingsView('setup');
});
if (nativeAndroidRuntime) $('androidConnectionOpenSetup').textContent = 'Open Setup';

async function api(route: string, body?: any): Promise<any> {
  if (isTauri()) {
    const result = await invoke<any>('api_request', { route, body: body ?? null });
    if (result?.error) throw new Error(result.error);
    return result;
  }
  if ((window as any).McpAndroidBridge?.dispatch) {
    const id = String(++nativeRequestNumber);
    return new Promise((resolve, reject) => {
      const watchdog = setTimeout(() => {
        nativePending.delete(id);
        reject(new Error('Native request timed out'));
      }, 65_000);
      nativePending.set(id, { resolve, reject, watchdog });
      try { (window as any).McpAndroidBridge.dispatch(id, JSON.stringify({ route, body: body ?? null })); }
      catch (error) {
        clearTimeout(watchdog);
        nativePending.delete(id);
        reject(new Error('Native connection unavailable'));
      }
    });
  }
  const response = await fetch(route, {
    method: body === undefined ? 'GET' : 'POST',
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    credentials: 'same-origin',
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  const data = await response.json();
  if (!response.ok) throw new Error(data.error || `HTTP ${response.status}`);
  return data;
}
let nativeRequestNumber = 0;
const nativePending = new Map<string, {resolve: (data: any) => void; reject: (error: Error) => void; watchdog: ReturnType<typeof setTimeout>}>();
(window as any).mcpNativeResult = (id: string, json: string) => {
  const pending = nativePending.get(String(id));
  if (!pending) return;
  nativePending.delete(String(id));
  clearTimeout(pending.watchdog);
  try {
    const data = JSON.parse(json);
    if (data?.error) pending.reject(new Error(data.error));
    else pending.resolve(data);
  } catch {
    pending.reject(new Error('Invalid native response'));
  }
};

function announce(message, good = false) {
  $('notice').textContent = message;
  $('notice').className = good ? 'notice good' : 'notice';
}

function el(tag: string, className = '', text?: any): any {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = String(text);
  return node;
}
// Small, fixed stroke-icon set. No injected markup or external icon runtime.
function uiIcon(name: 'folder' | 'file' | 'message' | 'chevron' | 'check' | 'plus' | 'close') {
  const paths = {
    folder: 'M3 7h7l2 2h9v10H3zM3 7V5h7l2 2',
    file: 'M6 3h8l4 4v14H6zM14 3v5h4M9 12h6M9 16h6',
    message: 'M4 5h16v12H8l-4 3zM8 9h8M8 13h5',
    chevron: 'm9 18 6-6-6-6',
    check: 'm5 12 5 5L19 7',
    plus: 'M12 5v14M5 12h14',
    close: 'M18 6 6 18M6 6l12 12',
  };
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('class', 'android-ui-icon');
  const shape = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  shape.setAttribute('d', paths[name]);
  svg.append(shape);
  return svg;
}

// Android's conversation displays work as a COS-style timeline, not as
// decorative assistant cards. Everything below is sourced from a real turn
// clock or a completed, checked tool receipt. ChatGPT's own private tool
// activity and raw shell arguments are NOT visible to this Android renderer.
type AndroidChatToolFact = {
  name:string; title:string; family:'android'|'mcp'|'tool'; outcome:'confirmed'|'failed';
  detail:string;
};
type AndroidChatTurnClock = {
  requestId:string; prompt:string; startedAt:number; step:string;
};
type AndroidChatTurnRecord = {
  sessionId:string; messageCount:number; prompt:string; answer:string;
  durationMs:number; tools:AndroidChatToolFact[];
};
let androidChatWorkingTurn:AndroidChatTurnClock|null = null;
let androidChatTick:number|null = null;
const androidChatTurnRecords:AndroidChatTurnRecord[] = [];

function androidChatElapsed(ms:number):string {
  const seconds=Math.max(0,Math.floor(Math.max(0,ms)/1000));
  const hours=Math.floor(seconds/3600),minutes=Math.floor(seconds%3600/60);
  return `${hours ? `${hours}h ` : ''}${hours || minutes ? `${minutes}m ` : ''}${seconds%60}s`;
}
function updateAndroidChatClock():void {
  if (!androidChatWorkingTurn) return;
  const caption=document.querySelector<HTMLElement>(
    `.android-chat-worked[data-request-id="${androidChatWorkingTurn.requestId}"] .android-chat-worked-text`);
  if (caption) caption.textContent=`Working for ${androidChatElapsed(performance.now()-androidChatWorkingTurn.startedAt)}`;
}
function startAndroidChatWork(requestId:string,prompt:string,step:string):void {
  if (!nativeAndroidRuntime || !requestId || !prompt.trim()) return;
  endAndroidChatWork();
  androidChatWorkingTurn={requestId,prompt,startedAt:performance.now(),step};
  androidChatTick=window.setInterval(updateAndroidChatClock,1000);
}
function endAndroidChatWork():void {
  if (androidChatTick!==null) window.clearInterval(androidChatTick);
  androidChatTick=null;
  androidChatWorkingTurn=null;
}
// Only restricted, non-secret success/failure metadata is retained by the
// Android chat store. Never reconstruct tool arguments or invent shell logs.
function androidChatVerifiedTools(actions:unknown):AndroidChatToolFact[] {
  if (!Array.isArray(actions) || actions.length>16) return [];
  const result:AndroidChatToolFact[]=[];
  for (const raw of actions) {
    if (!raw || typeof raw!=='object' || Array.isArray(raw)) continue;
    const action=raw as Record<string,unknown>;
    const name=typeof action.name==='string' ? action.name :
      typeof action.tool==='string' ? action.tool : '';
    if (!/^[A-Za-z0-9_.:-]{1,64}$/.test(name)) continue;
    let family:AndroidChatToolFact['family'];
    let confirmed=false;
    let failed=false;
    if (action.type==='android_local_direct_action' && action.nativeConfirmed===true &&
        action.userIntentAuthorized===true && action.resultIsError===false &&
        action.resultStored===false) {
      family='android'; confirmed=true;
    } else if (action.type==='android_local_tool_call' &&
        action.userIntentAuthorized===true && action.resultIsError===false &&
        action.resultStored===false) {
      family='android'; confirmed=true;
    } else if (action.type==='mcp_tool_call' && action.approvedByUser===true &&
        action.resultIsError===false && action.resultStored===false) {
      family='mcp'; confirmed=true;
    } else if (action.type===undefined && typeof action.tool==='string' &&
        typeof action.ok==='boolean') {
      // Legacy local/desktop agent history has only `{tool,ok}` receipts.
      family='tool'; confirmed=action.ok===true; failed=action.ok===false;
    } else continue;
    const titles:Record<string,string>={
      android_browser_start:'Opened browser session',android_browser_stop:'Stopped browser session',
      android_app_start:'Opened Android app',android_app_stop:'Stopped Android app control',
      android_browser_status:'Checked browser status',android_app_status:'Checked Android app status',
      android_browser_snapshot:'Inspected webpage',android_browser_click:'Clicked webpage element',
      android_browser_fill:'Filled webpage field',
    };
    const detail=confirmed
      ? 'Native receipt confirmed. Detailed arguments and result content were not saved in chat history.'
      : 'Local tool reported failure. Raw error and arguments are not displayed in the conversation.';
    result.push({name,title:titles[name] || `Called ${name}`,family,
      outcome:failed?'failed':'confirmed',detail});
  }
  return result;
}
function finishAndroidChatWork(session:Session|undefined,actions:unknown):void {
  if (!nativeAndroidRuntime || !androidChatWorkingTurn) return;
  const current=androidChatWorkingTurn;
  endAndroidChatWork();
  if (!session?.id || !Array.isArray(session.messages) || session.messages.length<2) return;
  const messages=session.messages;
  if (messages[messages.length-2]?.role!=='user' ||
      messages[messages.length-2]?.text!==current.prompt ||
      messages[messages.length-1]?.role!=='assistant') return;
  androidChatTurnRecords.push({sessionId:session.id,messageCount:messages.length,
    prompt:current.prompt,answer:messages[messages.length-1].text,
    durationMs:Math.max(0,performance.now()-current.startedAt),
    tools:androidChatVerifiedTools(actions)});
  if (androidChatTurnRecords.length>64) androidChatTurnRecords.splice(0,androidChatTurnRecords.length-64);
}
function androidChatRecord(session:Session,index:number):AndroidChatTurnRecord|undefined {
  const messages=session.messages || [];
  const prompt=messages[index-1]?.text,answer=messages[index]?.text;
  for (let at=androidChatTurnRecords.length-1;at>=0;at--) {
    const recent=androidChatTurnRecords[at];
    if (recent.sessionId===session.id && recent.messageCount===index+1 &&
        recent.prompt===prompt && recent.answer===answer) return recent;
  }
  // Older native-direct sessions record exactly one confirmed action per turn.
  // This is the only history shape that can be mapped to exchanges without
  // a stored per-action turn index; other uncorrelated actions are not shown.
  if (session.provider!=='local-automation' || index%2!==1 ||
      !Array.isArray(session.actions) || !session.actions.length ||
      !session.actions.every(action=>action && typeof action==='object' &&
        (action as Record<string,unknown>).type==='android_local_direct_action')) return undefined;
  const turnCount=Math.floor(messages.length/2);
  if (session.actions.length<turnCount) return undefined;
  const action=session.actions[session.actions.length-turnCount+Math.floor(index/2)];
  const tools=androidChatVerifiedTools([action]);
  if (!tools.length || typeof prompt!=='string' || typeof answer!=='string') return undefined;
  return {sessionId:session.id,messageCount:index+1,prompt,answer,durationMs:-1,tools};
}
function androidChatWorkedLine(label:string,requestId?:string):HTMLElement {
  const line=el('div','android-chat-worked');
  if (requestId) line.dataset.requestId=requestId;
  if (requestId && nativeAndroidRuntime) line.append(el('span','android-prime-running-spinner'));
  line.append(el('span','android-chat-worked-text',label));
  return line;
}
function androidChatToolActivity(tools:AndroidChatToolFact[]):HTMLElement|null {
  if (!tools.length) return null;
  const group=el('details','android-chat-tool-group');
  const summary=el('summary','android-chat-tool-group-summary');
  const sameTitle=tools.every(tool=>tool.title===tools[0].title);
  const headline=sameTitle ? tools[0].title : `${tools.length} tool calls`;
  summary.append(el('span','android-chat-tool-flash','ϟ'),
    el('span','',headline));
  if (tools.length>1) summary.append(el('span','android-chat-tool-count',String(tools.length)));
  summary.append(
    el('span','android-chat-tool-expand','⌄'));
  group.append(summary);
  for (const tool of tools) {
    const call=el('details','android-chat-tool');
    call.dataset.outcome=tool.outcome;
    const heading=el('summary','android-chat-tool-summary');
    heading.append(el('span','android-chat-tool-icon',tool.family==='android'?'▣':'⚡'),
      el('span','android-chat-tool-kind',tool.family==='android'?'native':tool.family),
      el('span','android-chat-tool-title',tool.title),
      el('span','android-chat-tool-result',tool.outcome==='confirmed'?'✓ Confirmed':'Failed'));
    const detail=el('div','android-chat-tool-detail');
    detail.append(el('div','',`Tool: ${tool.name}`),el('div','',tool.detail));
    call.append(heading,detail);
    group.append(call);
  }
  return group;
}
// Bundled, text-node-only formatting for assistant prose. COS ChatGPT displays
// emphasis and code inline; never set innerHTML or trust HTML/JS from a model.
// Large/unusual Markdown stays readable as escaped plaintext rather than
// building unbounded DOM or accidentally rendering active web content.
function androidChatProse(text:string):HTMLElement {
  const body=el('div','body android-chat-prose');
  const lines=text.split(/\r?\n/);
  if (lines.length>400 || text.length>25_000) {
    body.textContent=text;
    return body;
  }
  const inline=(node:HTMLElement,value:string) => {
    const parts=/(\*\*[^*\n]{1,220}\*\*|`[^`\n]{1,220}`|\*[^*\n]{1,220}\*)/g;
    let last=0;
    for (const match of value.matchAll(parts)) {
      const index=match.index ?? last;
      if (index>last) node.append(document.createTextNode(value.slice(last,index)));
      const token=match[0];
      const bold=token.startsWith('**');
      const code=token.startsWith('`');
      const tag=bold?'strong':code?'code':'em';
      const cut=bold?2:1;
      node.append(el(tag,'',token.slice(cut,-cut)));
      last=index+token.length;
    }
    if (last<value.length) node.append(document.createTextNode(value.slice(last)));
  };
  let paragraph:HTMLElement|null=null;
  let list:HTMLElement|null=null;
  let code:HTMLElement|null=null;
  let codeLines:string[]=[];
  const flushCode=() => {
    if (!code) return;
    code.textContent=codeLines.join('\n').slice(0,16_384);
    code=null;codeLines=[];
  };
  for (const line of lines) {
    if (/^\s*```/.test(line)) {
      paragraph=null;list=null;
      if (code) flushCode();
      else {const pre=el('pre','android-chat-code');code=el('code');pre.append(code);body.append(pre);}
      continue;
    }
    if (code) {codeLines.push(line);continue;}
    if (!line.trim()) {paragraph=null;list=null;continue;}
    const bullet=/^\s*(?:[-*]|\d+\.)\s+(.+)$/.exec(line);
    if (bullet) {
      paragraph=null;
      if (!list) {list=el('ul','android-chat-list');body.append(list);}
      const item=el('li');inline(item,bullet[1]);list!.append(item);
      continue;
    }
    list=null;
    const heading=/^\s*#{1,4}\s+(.+)$/.exec(line);
    if (heading) {
      paragraph=null;
      const title=el('p','android-chat-heading');inline(title,heading[1]);body.append(title);
      continue;
    }
    if (!paragraph) {paragraph=el('p');body.append(paragraph);}
    else paragraph.append(document.createTextNode('\n'));
    inline(paragraph!,line);
  }
  flushCode();
  return body;
}

function displayMessages(session, pending = false) {
  syncStandingRulesSession();
  const wrapper = $('messages');
  const nextSessionId = session?.id ?? selectedSession;
  if (nativeAndroidRuntime && androidGeneralChatSessionId !== nextSessionId) {
    androidGeneralChatSessionId = nextSessionId;
    androidGeneralChatScrolledUp = false;
  }
  const previousScrollTop = wrapper.scrollTop;
  // Follow a running reply, but allow reading earlier messages after it ends.
  // A later refresh must retain the reader's place instead of snapping down.
  const preserveScroll = nativeAndroidRuntime &&
    (!androidGeneralFollowOutput() || (!working && androidGeneralChatScrolledUp));
  wrapper.replaceChildren();
  const messages = session?.messages || [];
  const firstUser = messages.find(message => message.role === 'user');
  $('threadTitle').textContent = firstUser?.text?.slice(0, 34) || 'New chat';
  if (!messages.length) {
    const welcome = document.createElement('div');
    welcome.className = 'welcome';
    // The initial HTML is trusted bundled content; preserve the exact initial welcome layout.
    welcome.innerHTML = welcomeTemplate;
    wrapper.append(welcome);
    updateWelcomeConnection();
    return;
  }
  for (let index=0;index<messages.length;index++) {
    const message=messages[index];
    const prior=messages[index-1];
    const recorded=nativeAndroidRuntime && message.role==='assistant' && prior?.role==='user' &&
      session?.id ? androidChatRecord(session,index) : undefined;
    if (recorded && recorded.durationMs>=0)
      wrapper.append(androidChatWorkedLine(`Worked for ${androidChatElapsed(recorded.durationMs)}`));
    const node = el('article', `message ${message.role}`);
    const avatar = el('span', 'avatar', message.role === 'user' ? 'U' : '✳');
    const bubble = el('div', 'bubble');
    bubble.append(el('div', 'who', message.role === 'user' ? 'You' : 'Mcp Agent Ai'));
    bubble.append(nativeAndroidRuntime && message.role==='assistant'
      ? androidChatProse(message.text) : el('div', 'body', message.text));
    if (recorded?.tools?.length) {
      const activity=el('section','android-chat-activity');
      activity.setAttribute('aria-label','Recorded tool activity');
      const group=androidChatToolActivity(recorded.tools);
      if (group) activity.append(group);
      bubble.append(activity);
    }
    node.append(avatar, bubble);
    wrapper.append(node);
  }
  if (pending) {
    if (nativeAndroidRuntime && androidChatWorkingTurn &&
        messages[messages.length-1]?.role==='user' &&
        messages[messages.length-1]?.text===androidChatWorkingTurn.prompt)
      wrapper.append(androidChatWorkedLine(
        `Working for ${androidChatElapsed(performance.now()-androidChatWorkingTurn.startedAt)}`,
        androidChatWorkingTurn.requestId));
    const node = el('article', 'message pending');
    const avatar = el('span', 'avatar', '✳');
    const bubble = el('div', 'bubble');
    bubble.append(el('div', 'who', 'Mcp Agent Ai'));
    const dots = el('div', 'typing');
    dots.setAttribute('aria-label', 'Thinking');
    for (let i = 0; i < 3; i++) dots.append(el('i'));
    bubble.append(dots);
    if (nativeAndroidRuntime && androidChatWorkingTurn) {
      const running=el('div','android-chat-running');
      running.append(el('span','android-chat-running-dot'),
        el('span','android-chat-running-text',androidChatWorkingTurn.step));
      bubble.append(running);
    }
    node.append(avatar, bubble);
    wrapper.append(node);
  }
  if (nativeAndroidRuntime) androidGeneralProgrammaticScroll = true;
  wrapper.scrollTop = preserveScroll ? previousScrollTop : wrapper.scrollHeight;
  if (nativeAndroidRuntime)
    requestAnimationFrame(() => {androidGeneralProgrammaticScroll = false;});
}

const welcomeTemplate = $('welcome').innerHTML;

function updateWelcomeConnection() {
  const provider = $('provider').value;
  const hasSessionKey = !!state.hasKeys?.includes(provider);
  for (const title of document.querySelectorAll('[data-connection-title]'))
    title.textContent = nativeAndroidRuntime ? 'Connect ChatGPT' :
      hasSessionKey || provider === 'chatgpt-browser' ? 'Manage AI connection' : 'Connect your AI';
  for (const detail of document.querySelectorAll('[data-connection-detail]'))
    detail.textContent = nativeAndroidRuntime
      ? 'Sign in and connect your private ChatGPT session in Setup'
      : provider === 'chatgpt-browser'
      ? 'ChatGPT website · requires owned browser and explicit Send confirmation'
      : hasSessionKey
      ? 'Key in memory · Check your provider and model'
      : 'Choose your provider in Connections & tools';
}

$('messages').addEventListener('click', (event: MouseEvent) => {
  const target = event.target as Element;
  if (target.closest('[data-open-connection]')) {
    setSettings(true);
    if (nativeAndroidRuntime) {
      setAndroidSettingsView('setup');
      chooseAndroidCosSetupStep('connect');
    } else $('provider').focus();
    return;
  }
  const starter = target.closest<HTMLButtonElement>('[data-starter]');
  if (starter && !working) {
    $('prompt').value = starter.dataset.starter || '';
    updateCharCount();
    $('prompt').focus();
  }
});

// Projects are local labels for existing sessions. They never modify chat
// history and remain separate from the Windows/desktop workspace model.
type MobileProject = { id: string; name: string };
const projectStorageKey = 'mcp-agent-mobile-projects-v1';
const nativeProjectsFlagKey = 'mcp-agent-native-projects-active-v1';
let mobileProjects: MobileProject[] = [];
let projectAssignments: Record<string, string> = {};
let selectedProjectId: string | null = null;
let editingProjectId: string | null = null;
let pendingProjectFolderAfterCreate = false;
let nativeProjectMode = false;
let pendingProjectImport: string | null = null;
let pendingProjectImportSnapshot: string | null = null;
let nativeProjectRefreshTicket = 0;
if (nativeAndroidRuntime) {
  try {
    nativeProjectMode = localStorage.getItem(nativeProjectsFlagKey) === 'on';
    const stored = JSON.parse(localStorage.getItem(projectStorageKey) || '{}');
    if (!nativeProjectMode && Array.isArray(stored.projects)) {
      mobileProjects = stored.projects.slice(0, 30).filter((p: any) =>
        typeof p?.id === 'string' && p.id.length <= 90 &&
        typeof p?.name === 'string' && p.name.length > 0 && p.name.length <= 40);
    }
    if (!nativeProjectMode && stored.assignments && typeof stored.assignments === 'object' && !Array.isArray(stored.assignments)) {
      for (const [id, project] of Object.entries(stored.assignments)) {
        if (id.length < 120 && mobileProjects.some(p => p.id === project)) projectAssignments[id] = project as string;
      }
    }
  } catch { /* Invalid local labels cannot modify or erase conversations. */ }
}
function applyNativeProjectCatalog(catalog: {
  projects?: MobileProject[]; assignments?: Record<string,string>;
}) {
  if (!catalog || !Array.isArray(catalog.projects) ||
      !catalog.assignments || typeof catalog.assignments !== 'object') {
    throw Error('Invalid native project catalog');
  }
  mobileProjects = catalog.projects;
  projectAssignments = catalog.assignments;
  if (selectedProjectId && !mobileProjects.some(p => p.id === selectedProjectId))
    selectedProjectId = null;
  renderThreads();
}
async function refreshNativeProjectCatalog() {
  if (!nativeAndroidRuntime || !isTauri()) {
    $('projectNativeEnable').disabled = true;
    $('projectNativeStatus').textContent = 'Native Project Storage is available in the installed Android app.';
    return;
  }
  $('projectNativeEnable').disabled = false;
  $('projectNativeEnable').classList.toggle('hidden', nativeProjectMode);
  $('projectNativeDisable').classList.toggle('hidden', !nativeProjectMode);
  if (!nativeProjectMode) {
    nativeProjectRefreshTicket++;
    $('projectNativeStatus').textContent = 'Using original local project labels. Native storage is optional; nothing is imported automatically.';
    return;
  }
  const ticket = ++nativeProjectRefreshTicket;
  try {
    const result = await api('/api/mobile-projects', {action:'status'});
    if (ticket !== nativeProjectRefreshTicket || !nativeProjectMode) return;
    applyNativeProjectCatalog(result.catalog);
    $('projectNativeStatus').textContent = 'Using native private project storage. Original local labels remain preserved.';
  } catch (error) {
    if (ticket !== nativeProjectRefreshTicket || !nativeProjectMode) return;
    $('projectNativeStatus').textContent = `Native catalog unavailable: ${error.message}`;
  }
}
async function updateNativeProjects(
  action: string, options: Record<string, unknown> = {}
) {
  const ticket = ++nativeProjectRefreshTicket;
  const result = await api('/api/mobile-projects', { action, ...options });
  if (ticket === nativeProjectRefreshTicket && nativeProjectMode)
    applyNativeProjectCatalog(result.catalog);
  return result.catalog as {projects:MobileProject[];assignments:Record<string,string>};
}
$('projectNativeEnable').addEventListener('click', async () => {
  if (!nativeAndroidRuntime || !isTauri()) return;
  const source = localStorage.getItem(projectStorageKey);
  if (!source) {
    try {
      const result = await api('/api/mobile-projects', {action:'status'});
      applyNativeProjectCatalog(result.catalog);
      localStorage.setItem(nativeProjectsFlagKey, 'on');
      nativeProjectMode = true;
      await refreshNativeProjectCatalog();
    } catch (error) { $('projectNativeStatus').textContent = String(error.message); }
    return;
  }
  $('projectNativeStatus').textContent = 'Reviewing existing localStorage project labels. No data is modified.';
  try {
    const result = await api('/api/mobile-projects', {action:'review-import', legacy:source});
    pendingProjectImport = result.reviewId;
    pendingProjectImportSnapshot = source;
    $('projectImportPreview').classList.remove('hidden');
    $('projectImportDetails').textContent =
      `${result.preview.projects_to_add} project(s), ${result.preview.assignments_to_add} assignments to import; `+
      `${result.preview.conflicts.length} conflict(s), ${result.preview.unknown_chats.length} unknown chats.`;
    const list = $('projectImportNames');
    list.replaceChildren();
    for (const project of result.preview.incoming_projects)
      list.append(el('div', '', `• ${project.name}`));
    const valid = result.preview.conflicts.length === 0 && result.preview.unknown_chats.length === 0;
    $('projectImportApprove').disabled = !valid;
    if (!valid) $('projectNativeStatus').textContent =
      'Legacy labels need reconciliation; Rust import has been blocked. Original labels are untouched.';
  } catch (error) {
    pendingProjectImport = null;
    pendingProjectImportSnapshot = null;
    $('projectNativeStatus').textContent = String(error.message);
  }
});
$('projectImportApprove').addEventListener('click', async () => {
  const reviewId = pendingProjectImport;
  // The Rust review validates its supplied snapshot. The WebView must also
  // ensure that localStorage has not changed since the user saw the preview.
  if (reviewId && localStorage.getItem(projectStorageKey) !== pendingProjectImportSnapshot) {
    pendingProjectImport = null;
    pendingProjectImportSnapshot = null;
    $('projectImportApprove').disabled = true;
    $('projectNativeStatus').textContent = 'Local project labels changed during review. Review the current labels again before approving; nothing was imported.';
    return;
  }
  if (!reviewId || !window.confirm(
    'Import the project groups exactly as reviewed into the Rust catalog? Your original local labels will be retained.')) return;
  $('projectImportApprove').disabled = true;
  try {
    const result = await api('/api/mobile-projects', {action:'approve-import', reviewId});
    if (localStorage.getItem(projectStorageKey) !== pendingProjectImportSnapshot)
      throw Error('Local labels changed during import. Original mode remains active; review current labels again.');
    applyNativeProjectCatalog(result.catalog);
    localStorage.setItem(nativeProjectsFlagKey, 'on');
    nativeProjectMode = true;
    pendingProjectImport = null;
    pendingProjectImportSnapshot = null;
    $('projectImportPreview').classList.add('hidden');
    await refreshNativeProjectCatalog();
  } catch (error) { $('projectNativeStatus').textContent = String(error.message); }
  finally { $('projectImportApprove').disabled = false; }
});
$('projectImportCancel').addEventListener('click', () => {
  pendingProjectImport = null;
  pendingProjectImportSnapshot = null;
  $('projectImportPreview').classList.add('hidden');
  $('projectNativeStatus').textContent = 'Import cancelled. Original local labels preserved.';
});
$('projectNativeDisable').addEventListener('click', () => {
  if (!window.confirm('Show your original local project labels again? The Rust catalog will remain saved.')) return;
  try {
    nativeProjectRefreshTicket++;
    pendingProjectImport = null;
    pendingProjectImportSnapshot = null;
    localStorage.removeItem(nativeProjectsFlagKey);
    nativeProjectMode = false;
    const stored = JSON.parse(localStorage.getItem(projectStorageKey) || '{}');
    mobileProjects = Array.isArray(stored.projects) ? stored.projects : [];
    projectAssignments = stored.assignments && typeof stored.assignments === 'object' ?
      stored.assignments : {};
    selectedProjectId = null;
    renderThreads();
    void refreshNativeProjectCatalog();
  } catch (error) { $('projectNativeStatus').textContent = String(error.message); }
});
function saveProjectLabels() {
  try {
    localStorage.setItem(projectStorageKey, JSON.stringify({ projects: mobileProjects, assignments: projectAssignments }));
    return true;
  } catch {
    announce('Unable to save project labels on this device');
    return false;
  }
}
function renderProjects() {
  const list = $('projectsList');
  list.replaceChildren();
  if (!mobileProjects.length) {
    list.append(el('div', 'project-empty', 'No projects yet. Add a folder to get started.'));
  } else {
    const all = el('button', !selectedProjectId ? 'selected' : '');
    all.append(uiIcon('message'), el('span', '', 'All chats'),
      el('span', 'project-chat-count', String(state.sessions.length)));
    all.type = 'button';
    all.addEventListener('click', () => {
      selectedProjectId = null;
      renderThreads();
      $('projectSelectedFolderStatus').textContent = '';
    });
    list.append(all);
    for (const project of mobileProjects) {
      const button = el('button', selectedProjectId === project.id ? 'selected' : '');
      button.append(uiIcon('folder'), el('span', '', project.name),
        el('span', 'project-chat-count',
          String(state.sessions.filter(session => projectAssignments[session.id] === project.id).length)));
      button.type = 'button';
      button.title = project.name;
      button.addEventListener('click', () => {
        selectedProjectId = project.id;
        renderThreads();
        $('projectUnlinkPhoneFolder').classList.add('hidden');
        $('projectSelectedFolderStatus').textContent = 'Checking phone folder access…';
        void refreshSelectedProjectFolderStatus();
      });
      list.append(button);
    }
  }
  const assign = $('projectAssign');
  assign.classList.toggle('hidden', !selectedProjectId || !selectedSession);
  $('projectManage').classList.toggle('hidden', !selectedProjectId);
  const assigned = !!(selectedSession && selectedProjectId && projectAssignments[selectedSession] === selectedProjectId);
  assign.replaceChildren(uiIcon(assigned ? 'check' : 'plus'),
    document.createTextNode(assigned ? 'Remove current chat from project' : 'Add current chat to project'));
}
$('sidebarSearch').addEventListener('input', renderThreads);
$('projectsToggle').addEventListener('click', () => {
  const opened = $('projectsToggle').getAttribute('aria-expanded') !== 'true';
  $('projectsToggle').setAttribute('aria-expanded', String(opened));
  $('projectsContent').classList.toggle('hidden', !opened);
});
$('chatsToggle').addEventListener('click', () => {
  const opened = $('chatsToggle').getAttribute('aria-expanded') !== 'true';
  $('chatsToggle').setAttribute('aria-expanded', String(opened));
  $('threads').classList.toggle('hidden', !opened);
});
function showProjectCreateForm() {
  editingProjectId = null;
  $('projectForm').querySelector('button[type="submit"]').setAttribute('aria-label', 'Save project');
  $('projectsToggle').setAttribute('aria-expanded', 'true');
  $('projectsContent').classList.remove('hidden');
  $('projectForm').classList.remove('hidden');
  $('projectName').focus();
}
function closeAndroidProjectAddMenu() {
  $('androidProjectAddMenu').classList.add('hidden');
  $('projectAdd').setAttribute('aria-expanded','false');
}
async function refreshSelectedProjectFolderStatus() {
  const id = selectedProjectId;
  if (!id || !nativeAndroidRuntime || !isTauri()) return;
  try {
    const status = await invoke<{linked:boolean;granted:boolean;displayName?:string;readOnly?:boolean}>(
      'document_tree_project_folder_status',{projectId:id});
    if (selectedProjectId !== id) return;
    $('projectSelectedFolderStatus').textContent = status.linked
      ? `Phone folder: ${status.displayName || 'selected folder'} · ${status.granted ? 'Read access verified' : 'Access expired'}`
      : 'No phone folder linked to this project.';
    $('projectUnlinkPhoneFolder').classList.toggle('hidden',!status.linked);
  } catch {
    if (selectedProjectId === id) {
      $('projectUnlinkPhoneFolder').classList.add('hidden');
      $('projectSelectedFolderStatus').textContent = 'Phone folder status unavailable.';
    }
  }
}
// Removing a label without revoking its independent SAF grant would leave an
// orphaned persisted folder permission that the user can no longer unlink.
async function projectFolderUnlinkedBeforeRemoval(id: string): Promise<boolean> {
  if (!nativeAndroidRuntime || !isTauri()) return true;
  try {
    const receipt = await invoke<{projectId:string;linked:boolean;granted:boolean}>(
      'document_tree_project_folder_status',{projectId:id});
    if (selectedProjectId !== id) return false;
    if (receipt.projectId !== id || receipt.linked !== false || receipt.granted !== false) {
      announce('Remove this project’s phone folder access before removing its group. No files have been deleted.');
      return false;
    }
    return true;
  } catch {
    announce('Project folder access could not be verified. Group removal is blocked to protect the existing Android grant.');
    return false;
  }
}
async function requestProjectPhoneFolder(id: string) {
  if (!nativeAndroidRuntime || !isTauri() || !mobileProjects.some(project => project.id === id))
    throw Error('Select an existing project in the installed Android app first');
  const receipt = await invoke<{selected:boolean;granted?:boolean;persisted?:boolean;displayName?:string;readOnly?:boolean}>(
    'document_tree_project_folder_request',{projectId:id,userInitiated:true});
  if (receipt.selected !== true || receipt.granted !== true || receipt.persisted !== true ||
      receipt.readOnly !== true) {
    if (receipt.selected === false) {
      announce('Android folder picker closed without linking a folder.');
      return;
    }
    throw Error('Android did not verify a persistent read-only project folder grant');
  }
  $('projectPhoneFolderStatus').textContent = `Linked ${receipt.displayName || 'selected phone folder'} (read-only).`;
  await refreshSelectedProjectFolderStatus();
  announce('Phone folder linked to selected project with read-only Android grant.',true);
}
$('projectAdd').addEventListener('click', () => {
  if (!nativeAndroidRuntime) { showProjectCreateForm(); return; }
  const open = $('androidProjectAddMenu').classList.contains('hidden');
  $('androidProjectAddMenu').classList.toggle('hidden', !open);
  $('projectAdd').setAttribute('aria-expanded', String(open));
  $('projectsToggle').setAttribute('aria-expanded', 'true');
  $('projectsContent').classList.remove('hidden');
});
$('projectAddGroup').addEventListener('click', () => {
  closeAndroidProjectAddMenu();
  showProjectCreateForm();
});
$('projectChoosePhoneFolder').addEventListener('click', async () => {
  closeAndroidProjectAddMenu();
  if (!nativeAndroidRuntime || !isTauri()) {
    $('projectPhoneFolderStatus').textContent = 'Phone folder picker requires the installed Android app.';
    return;
  }
  if (!selectedProjectId) {
    pendingProjectFolderAfterCreate = true;
    showProjectCreateForm();
    announce('Name and save a project group first; then Android will open the phone folder picker.');
    return;
  }
  const button = $('projectChoosePhoneFolder') as HTMLButtonElement;
  button.disabled = true;
  $('projectPhoneFolderStatus').textContent = 'Opening Android folder picker. Select a folder yourself.';
  try {
    await requestProjectPhoneFolder(selectedProjectId);
  } catch (error) {
    $('projectPhoneFolderStatus').textContent = `Folder not linked: ${String(error).slice(0,160)}`;
    announce('Could not verify a persistent Android Project folder grant. No new folder binding was accepted.');
  } finally {button.disabled = false;}
});
const androidSshDialog=$('androidSshDialog') as HTMLDialogElement;
let androidSshBusy=false;
const androidSshLinksKey='mcp-agent-ai-android-project-ssh-links-v1';
type AndroidSshLink={host:string;port:number;username:string;remoteDir:string;verifiedAtMs:number};
function androidSshSavedLink(projectId:string):AndroidSshLink|null {
  try{
    const links=JSON.parse(localStorage.getItem(androidSshLinksKey)||'{}');
    const item=links?.[projectId];
    if(!item||typeof item!=='object'||typeof item.host!=='string'||
      typeof item.username!=='string'||typeof item.remoteDir!=='string'||
      !Number.isInteger(item.port)||item.port<1||item.port>65535||
      !Number.isFinite(item.verifiedAtMs)||item.verifiedAtMs<0)return null;
    return item as AndroidSshLink;
  }catch{return null;}
}
function saveAndroidSshLink(projectId:string,link:AndroidSshLink):boolean{
  try{
    const raw=JSON.parse(localStorage.getItem(androidSshLinksKey)||'{}');
    const saved=raw&&typeof raw==='object'&&!Array.isArray(raw)?raw:{};
    saved[projectId]=link;
    // A localStorage entry is only a convenience label. Every future SSH
    // operation must repeat native pinned-key authentication and project checks.
    if(Object.keys(saved).length>30)throw Error('SSH link capacity exceeded');
    localStorage.setItem(androidSshLinksKey,JSON.stringify(saved));
    return true;
  }catch{return false;}
}
function androidSshCurrentProjectId():string|null {
  const value=($('androidSshProject') as HTMLSelectElement).value;
  return nativeProjectMode&&mobileProjects.some(item=>item.id===value)?value:null;
}
function androidSshParseAddress():{username:string;host:string;port:number}{
  const address=($('androidSshAddress') as HTMLInputElement).value.trim();
  const match=/^([a-zA-Z][a-zA-Z0-9_.-]{0,31})@([a-zA-Z0-9][a-zA-Z0-9.-]{0,252})(?::([0-9]{1,5}))?$/.exec(address);
  if(!match)throw Error('Use user@server or user@server:port (key authentication only).');
  const port=match[3]?Number(match[3]):22;
  if(port<1||port>65535||match[2].includes('..')||match[2].endsWith('.')||
      match[2].startsWith('-'))throw Error('Invalid SSH host or port.');
  return {username:match[1],host:match[2].toLowerCase(),port};
}
function androidSshRemoteFolder():string {
  const remote=($('androidSshFolder') as HTMLInputElement).value.trim();
  if(!remote.startsWith('/')||remote.length>256||remote.includes('//')||
      remote.split('/').some((part,index)=>index>0&&(part==='..'||part==='.'))||
      !/^\/[a-zA-Z0-9._/-]*$/.test(remote))
    throw Error('Use an absolute server folder such as /srv/app. Relative paths and shell characters are blocked.');
  return remote;
}
function androidSshLoadLink(projectId:string|null):void {
  const saved=projectId?androidSshSavedLink(projectId):null;
  ($('androidSshAddress') as HTMLInputElement).value=saved
    ? `${saved.username}@${saved.host}${saved.port===22?'':':'+saved.port}` : '';
  ($('androidSshFolder') as HTMLInputElement).value=saved?.remoteDir||'';
  if(saved)$('androidSshStatus').textContent=
    'Saved SSH address loaded. The prior test is historical; connection must be verified again.';
}
function androidSshSetBusy(busy:boolean):void{
  androidSshBusy=busy;
  for(const id of ['androidSshGenerateKey','androidSshPinHost','androidSshClose',
    'androidSshCancel','androidSshManageProjects'])
    ($(`${id}`) as HTMLButtonElement).disabled=busy;
  ($('androidSshSubmit') as HTMLButtonElement).disabled=busy||!androidSshCurrentProjectId();
}
function androidSshError(error:unknown):string{
  return String(error instanceof Error?error.message:error).slice(0,230);
}
async function androidSshHostFingerprint(publicKey:string):Promise<string>{
  const parts=publicKey.trim().split(/\s+/);
  if(parts.length!==2||parts[0]!=='ssh-ed25519'||
      !/^[a-zA-Z0-9+/]{68}$/.test(parts[1]))throw Error('Enter one Ed25519 host public key without a trailing comment.');
  const decoded=atob(parts[1]);
  if(decoded.length!==51)throw Error('Invalid SSH host key bytes.');
  const wire=Uint8Array.from(decoded,c=>c.charCodeAt(0));
  const digest=new Uint8Array(await crypto.subtle.digest('SHA-256',wire));
  return 'SHA256:'+btoa(String.fromCharCode(...digest)).replace(/=+$/,'');
}
function resetAndroidSshHostReview():void{
  $('androidSshHostKeyReview').classList.add('hidden');
  ($('androidSshTrustHost') as HTMLInputElement).checked=false;
}
function restoreAndroidSshStorageSettings():void {
  const original=$('androidWorkspaceLegacyBindings');
  const settings=$('projectStorageSettings');
  if(settings.parentElement!==original)original.append(settings);
  settings.style.removeProperty('display');
  $('androidSshStorageSlot').classList.add('hidden');
}
function renderAndroidSshProjects():void {
  const select=$('androidSshProject') as HTMLSelectElement;
  const previous=select.value;
  select.replaceChildren();
  for(const project of nativeProjectMode ? mobileProjects : []){
    const option=document.createElement('option');
    option.value=project.id;
    option.textContent=project.name+(androidSshSavedLink(project.id)?' · SSH':'');
    select.append(option);
  }
  if(previous&&mobileProjects.some(item=>item.id===previous))select.value=previous;
  else if(selectedProjectId&&mobileProjects.some(item=>item.id===selectedProjectId))
    select.value=selectedProjectId;
  ($('androidSshSubmit') as HTMLButtonElement).disabled=androidSshBusy||!select.value;
  if(!nativeProjectMode){
    $('androidSshStatus').textContent=
      'Native project storage is required for an SSH workspace. Your existing local project labels are preserved until you approve migration.';
  }else if(!mobileProjects.length){
    $('androidSshStatus').textContent='Create a native project group first, then add its remote folder.';
  }
}
$('projectChooseServerSsh').addEventListener('click',async()=>{
  closeAndroidProjectAddMenu();
  if(!nativeAndroidRuntime||!isTauri()){
    $('projectPhoneFolderStatus').textContent='SSH projects require the installed Android app.';
    return;
  }
  $('androidSshStatus').textContent='No connection attempted. Server identity must be verified.';
  resetAndroidSshHostReview();
  if(nativeProjectMode)await refreshNativeProjectCatalog();
  renderAndroidSshProjects();
  androidSshLoadLink(androidSshCurrentProjectId());
  androidSshDialog.showModal();
  if(($('androidSshProject') as HTMLSelectElement).value)
    ($('androidSshAddress') as HTMLInputElement).focus();
});
function closeAndroidSshDialog():void {
  if(androidSshBusy)return;
  androidSshDialog.close();
  resetAndroidSshHostReview();
}
androidSshDialog.addEventListener('close',restoreAndroidSshStorageSettings);
$('androidSshClose').addEventListener('click',closeAndroidSshDialog);
$('androidSshCancel').addEventListener('click',closeAndroidSshDialog);
androidSshDialog.addEventListener('cancel',event=>{
  if(androidSshBusy)event.preventDefault();
});
$('androidSshProject').addEventListener('change',()=>{
  const projectId=androidSshCurrentProjectId();
  renderAndroidSshProjects();
  resetAndroidSshHostReview();
  androidSshLoadLink(projectId);
});
for(const id of ['androidSshAddress','androidSshHostPublicKey'])
  $(id).addEventListener('input',resetAndroidSshHostReview);
$('androidSshManageProjects').addEventListener('click',()=>{
  if(nativeProjectMode){
    closeAndroidSshDialog();
    showProjectCreateForm();
    return;
  }
  const slot=$('androidSshStorageSlot');
  const source=$('projectStorageSettings') as HTMLElement;
  slot.append(source);
  source.style.display='block';
  slot.classList.remove('hidden');
  void refreshNativeProjectCatalog();
  $('projectNativeEnable').focus({preventScroll:true});
});
new MutationObserver(()=>{
  if(androidSshDialog.open&&nativeProjectMode){
    renderAndroidSshProjects();
    if(androidSshCurrentProjectId()&&!($('androidSshAddress') as HTMLInputElement).value)
      androidSshLoadLink(androidSshCurrentProjectId());
  }
}).observe($('projectNativeStatus'),{childList:true,characterData:true,subtree:true});
$('androidSshGenerateKey').addEventListener('click',async()=>{
  const projectId=androidSshCurrentProjectId();
  if(!projectId||!nativeAndroidRuntime||!isTauri())return;
  if(!($('androidSshKeyConsent') as HTMLInputElement).checked){
    $('androidSshStatus').textContent='Confirm unencrypted app-private SSH key storage before generating a key.';
    return;
  }
  androidSshSetBusy(true);
  $('androidSshStatus').textContent='Requesting project SSH identity from Android…';
  try{
    const reply=await invoke<{projectId:string;publicKey:string;privateKeyExported:boolean;
      privateKeyPassphraseProtected:boolean;remoteAuthorizationVerified:boolean}>(
      'android_project_ssh_identity',{projectId,confirmUnencryptedKeyStorage:true,userInitiated:true});
    if(reply.projectId!==projectId||reply.privateKeyExported!==false||
        reply.privateKeyPassphraseProtected!==false||reply.remoteAuthorizationVerified!==false||
        !/^ssh-ed25519 [A-Za-z0-9+/]{68}(?: .*)?$/.test(reply.publicKey))
      throw Error('SSH identity receipt did not match a verified project key.');
    ($('androidSshClientPublicKey') as HTMLTextAreaElement).value=reply.publicKey;
    $('androidSshStatus').textContent='Project public key generated or loaded. Add it to the server authorized_keys yourself; server login has not been tested.';
  }catch(error){$('androidSshStatus').textContent='SSH identity unavailable: '+androidSshError(error);}
  finally{androidSshSetBusy(false);}
});
$('androidSshCopyKey').addEventListener('click',async()=>{
  const key=($('androidSshClientPublicKey') as HTMLTextAreaElement).value.trim();
  if(!key)return;
  try{await navigator.clipboard.writeText(key);
    $('androidSshStatus').textContent='Project public key copied. Nothing has been sent to your server.';
  }catch{($('androidSshClientPublicKey') as HTMLTextAreaElement).select();
    $('androidSshStatus').textContent='Select and copy the public key manually.';}
});
$('androidSshPinHost').addEventListener('click',async()=>{
  const projectId=androidSshCurrentProjectId();
  if(!projectId||!nativeAndroidRuntime||!isTauri())return;
  let address:{username:string;host:string;port:number};
  const key=($('androidSshHostPublicKey') as HTMLTextAreaElement).value.trim();
  try{address=androidSshParseAddress();
    const fingerprint=await androidSshHostFingerprint(key);
    $('androidSshFingerprint').textContent=fingerprint;
  }catch(error){$('androidSshStatus').textContent=androidSshError(error);return;}
  if($('androidSshHostKeyReview').classList.contains('hidden')){
    $('androidSshHostKeyReview').classList.remove('hidden');
    $('androidSshStatus').textContent=
      'Compare the fingerprint against a trusted server record, tick the confirmation, then press Save again.';
    return;
  }
  if(!($('androidSshTrustHost') as HTMLInputElement).checked){
    $('androidSshStatus').textContent='Independent host fingerprint verification is required.';
    return;
  }
  androidSshSetBusy(true);
  try{
    const reply=await invoke<{projectId:string;host:string;port:number;pinned:boolean;
      replacedExistingPin:boolean;remoteHostVerifiedByApp:boolean}>(
      'android_project_ssh_pin',{projectId,host:address.host,port:address.port,
        publicKey:key,confirmedTrustedHostKey:true,userInitiated:true});
    if(reply.projectId!==projectId||reply.host!==address.host||reply.port!==address.port||
        reply.pinned!==true||reply.replacedExistingPin!==false||
        reply.remoteHostVerifiedByApp!==false)
      throw Error('SSH host pin was not verified by Android.');
    $('androidSshStatus').textContent='Trusted host key pinned in Android app-private known_hosts. Authentication has not been tested.';
  }catch(error){$('androidSshStatus').textContent='Host not pinned: '+androidSshError(error);}
  finally{androidSshSetBusy(false);}
});
$('androidSshForm').addEventListener('submit',async event=>{
  event.preventDefault();
  const projectId=androidSshCurrentProjectId();
  if(!projectId||androidSshBusy||!nativeAndroidRuntime||!isTauri())return;
  let address:{username:string;host:string;port:number};
  let remoteDir:string;
  try{address=androidSshParseAddress();remoteDir=androidSshRemoteFolder();}
  catch(error){$('androidSshStatus').textContent=androidSshError(error);return;}
  androidSshSetBusy(true);
  $('androidSshStatus').textContent='Testing an actual SSH connection and remote folder with strict host-key checking…';
  try{
    const reply=await invoke<{projectId:string;host:string;port:number;
      hostKeyPolicy:string;authenticationMethod:string;remoteDir?:string;
      processStarted:boolean;completed:boolean;success:boolean;stdout?:string;stderr?:string}>(
      'android_project_ssh_run',{projectId,host:address.host,port:address.port,
        username:address.username,action:'pwd',remoteDir,userInitiated:true});
    if(reply.projectId!==projectId||reply.host!==address.host||reply.port!==address.port||
        reply.hostKeyPolicy!=='strict-pinned'||reply.authenticationMethod!=='publickey-only'||
        reply.remoteDir!==remoteDir||reply.processStarted!==true||
        reply.completed!==true||reply.success!==true)
      throw Error((reply.stderr||'SSH command did not verify the server folder').slice(0,180));
    const stored=saveAndroidSshLink(projectId,{...address,remoteDir,verifiedAtMs:Date.now()});
    if(!stored)throw Error('SSH connected but local project link could not be saved');
    selectedProjectId=projectId;
    renderThreads();
    $('androidSshStatus').textContent='SSH verified and project folder saved. Future access will recheck host and key.';
    androidSshDialog.close();
    announce('Remote SSH project verified and linked.',true);
  }catch(error){
    $('androidSshStatus').textContent='SSH project not linked: '+androidSshError(error);
  }finally{androidSshSetBusy(false);}
});
$('projectCancel').addEventListener('click', () => {
  pendingProjectFolderAfterCreate = false;
  editingProjectId = null;
  $('projectName').value = '';
  $('projectForm').classList.add('hidden');
});
$('projectForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  const name = $('projectName').value.trim();
  if (!name || name.length > 40 || [...name].some(c => c.charCodeAt(0) < 32)) return;
  if (nativeProjectMode && nativeAndroidRuntime && isTauri()) {
    const submit = $('projectForm').querySelector('button[type="submit"]');
    submit.disabled = true;
    try {
      const oldIds = new Set(mobileProjects.map(p => p.id));
      const catalog = editingProjectId ?
        await updateNativeProjects('rename', { id: editingProjectId, name }) :
        await updateNativeProjects('create', { name });
      if (!editingProjectId)
        selectedProjectId = catalog.projects.find(p => !oldIds.has(p.id))?.id || selectedProjectId;
      editingProjectId = null;
      $('projectName').value = '';
      $('projectForm').classList.add('hidden');
      renderThreads();
      if (pendingProjectFolderAfterCreate && selectedProjectId) {
        pendingProjectFolderAfterCreate = false;
        try { await requestProjectPhoneFolder(selectedProjectId); }
        catch (error) { announce(`Project saved; phone folder not linked: ${String(error).slice(0,150)}`); }
      }
    } catch (error) { announce(`Native project error: ${error.message}`); }
    finally { submit.disabled = false; }
    return;
  }
  if (editingProjectId) {
    const project = mobileProjects.find(p => p.id === editingProjectId);
    if (!project) return;
    const previousName = project.name;
    project.name = name;
    if (!saveProjectLabels()) { project.name = previousName; return; }
    editingProjectId = null;
    $('projectName').value = '';
    $('projectForm').classList.add('hidden');
    renderThreads();
    return;
  }
  if (mobileProjects.length >= 30) { announce('Maximum 30 local projects'); return; }
  const id = 'project-' + Date.now() + '-' + Math.random().toString(36).slice(2, 10);
  mobileProjects.push({ id, name });
  if (!saveProjectLabels()) { mobileProjects.pop(); return; }
  selectedProjectId = id;
  $('projectName').value = '';
  $('projectForm').classList.add('hidden');
  renderThreads();
  if (pendingProjectFolderAfterCreate) {
    pendingProjectFolderAfterCreate = false;
    try { await requestProjectPhoneFolder(id); }
    catch (error) { announce(`Project saved; phone folder not linked: ${String(error).slice(0,150)}`); }
  }
});
$('projectUnlinkPhoneFolder').addEventListener('click', async () => {
  const id = selectedProjectId;
  if (!id || !nativeAndroidRuntime || !isTauri()) return;
  if (!window.confirm('Remove this project\'s Android folder grant? Files in the folder will remain untouched.')) return;
  try {
    const result = await invoke<{released:boolean;linked:boolean;filesDeleted:boolean}>(
      'document_tree_project_folder_release',{projectId:id,userInitiated:true});
    if (result.released !== true || result.linked !== false || result.filesDeleted !== false)
      throw Error('Folder grant release was not verified');
    await refreshSelectedProjectFolderStatus();
    announce('Project folder access removed. No files deleted.',true);
  } catch (error) { announce(`Folder release not verified: ${String(error).slice(0,160)}`); }
});
$('projectRename').addEventListener('click', () => {
  const project = mobileProjects.find(p => p.id === selectedProjectId);
  if (!project) return;
  editingProjectId = project.id;
  $('projectName').value = project.name;
  $('projectForm').querySelector('button[type="submit"]').setAttribute('aria-label', 'Save project name');
  $('projectForm').classList.remove('hidden');
  $('projectName').focus();
});
$('projectRemove').addEventListener('click', async () => {
  const project = mobileProjects.find(p => p.id === selectedProjectId);
  if (!project || !window.confirm(`Remove project group "${project.name}"? All chats will be kept.`)) return;
  if (!(await projectFolderUnlinkedBeforeRemoval(project.id))) return;
  if (nativeProjectMode && nativeAndroidRuntime && isTauri()) {
    try {
      await updateNativeProjects('remove', { id: project.id });
      selectedProjectId = null;
      editingProjectId = null;
      $('projectName').value = '';
      $('projectForm').classList.add('hidden');
      renderThreads();
    } catch (error) { announce(`Native project error: ${error.message}`); }
    return;
  }
  const projectsBefore = mobileProjects;
  const assignmentsBefore = projectAssignments;
  mobileProjects = mobileProjects.filter(p => p.id !== project.id);
  projectAssignments = Object.fromEntries(Object.entries(projectAssignments).filter(([, id]) => id !== project.id));
  if (!saveProjectLabels()) {
    mobileProjects = projectsBefore;
    projectAssignments = assignmentsBefore;
    return;
  }
  selectedProjectId = null;
  editingProjectId = null;
  $('projectName').value = '';
  $('projectForm').classList.add('hidden');
  renderThreads();
});
$('projectAssign').addEventListener('click', async () => {
  if (!selectedSession || !selectedProjectId) return;
  if (nativeProjectMode && nativeAndroidRuntime && isTauri()) {
    try {
      const target = projectAssignments[selectedSession] === selectedProjectId ?
        null : selectedProjectId;
      await updateNativeProjects('assign', { chatId: selectedSession, projectId: target });
    } catch (error) { announce(`Native project assignment failed: ${error.message}`); }
    return;
  }
  const previous = projectAssignments[selectedSession];
  if (previous === selectedProjectId) delete projectAssignments[selectedSession];
  else projectAssignments[selectedSession] = selectedProjectId;
  if (!saveProjectLabels()) {
    if (previous) projectAssignments[selectedSession] = previous;
    else delete projectAssignments[selectedSession];
  }
  renderThreads();
});
$('chatsRefresh').addEventListener('click', async () => {
  if (working) return;
  $('chatsRefresh').disabled = true;
  try { await refresh(); announce('Chats refreshed', true); }
  catch (error) { announce(String(error.message)); }
  finally { $('chatsRefresh').disabled = false; }
});
// Android View menu and right-hand owned browser are independent from the
// left conversations drawer. A browser mode preference never opens a page.
let androidViewZoom = 1;
function sizeAndroidViewMenu() {
  if (!nativeAndroidRuntime) return;
  // CSS zoom scales fixed-position popups too. Keep the visual popup inside
  // the *physical* viewport at 120%, even on narrow 320px devices.
  const menu = $('androidViewMenu');
  menu.style.left = `${51 / androidViewZoom}px`;
  menu.style.top = `${48 / androidViewZoom}px`;
  // CSS zoom changes innerWidth/innerHeight in Android WebView as well as
  // transforming fixed children; compensate for both effects.
  menu.style.width = `${Math.min(274, Math.max(160, (window.innerWidth / androidViewZoom - 61) / androidViewZoom))}px`;
  menu.style.maxHeight = `${Math.max(180, (window.innerHeight / androidViewZoom - 60) / androidViewZoom)}px`;
}
function closeAndroidViewMenu() {
  $('androidViewMenu').classList.add('hidden');
  $('androidViewBackdrop').classList.add('hidden');
  $('sidebarQuickSearch').setAttribute('aria-expanded', 'false');
}
function updateAndroidViewMenu() {
  $('viewPetsLabel').textContent = $('petBuddy').classList.contains('hidden') ? 'Show pets' : 'Hide pets';
  $('viewZoomPercent').textContent = `${Math.round(androidViewZoom * 100)}%`;
  const browser = document.getElementById('androidViewMenu')!.querySelector<HTMLButtonElement>('[data-view-action="browser"]')!;
  browser.disabled = !nativeAndroidRuntime || !isTauri();
  browser.setAttribute('aria-disabled', String(browser.disabled));
  const browserTools = document.getElementById('androidViewMenu')!.querySelector<HTMLButtonElement>('[data-view-action="browser-tools"]')!;
  browserTools.disabled = browser.disabled;
  browserTools.setAttribute('aria-disabled', String(browserTools.disabled));
  document.getElementById('androidViewMenu')!.querySelector<HTMLButtonElement>('[data-view-action="zoom-in"]')!.disabled = androidViewZoom >= 1.2;
  document.getElementById('androidViewMenu')!.querySelector<HTMLButtonElement>('[data-view-action="zoom-out"]')!.disabled = androidViewZoom <= .8;
}
function setAndroidViewZoom(factor: number) {
  if (!nativeAndroidRuntime) return;
  androidViewZoom = Math.max(.8, Math.min(1.2, Math.round(factor * 10) / 10));
  document.documentElement.style.zoom = `${Math.round(androidViewZoom * 100)}%`;
  sizeAndroidViewMenu();
  updateAndroidViewMenu();
}
function openAndroidChatSearch() {
  closeAndroidViewMenu();
  if (!$('sidebar').classList.contains('open')) {
    toggleSidebar();
  }
  $('sidebar').classList.add('is-searching');
  $('sidebarSearch').focus({ preventScroll: true });
}
$('sidebarSearchButton').addEventListener('click', openAndroidChatSearch);
function toggleAndroidViewMenu() {
  if (!nativeAndroidRuntime) return;
  const shouldOpen = $('androidViewMenu').classList.contains('hidden');
  closeAndroidViewMenu();
  if (!shouldOpen) return;
  closeSidebar();
  closeToolMenu();
  updateAndroidViewMenu();
  sizeAndroidViewMenu();
  $('androidViewMenu').classList.remove('hidden');
  $('androidViewBackdrop').classList.remove('hidden');
  $('sidebarQuickSearch').setAttribute('aria-expanded', 'true');
}
$('sidebarQuickSearch').addEventListener('click', toggleAndroidViewMenu);
$('sidebarQuickSearch').addEventListener('keydown', (event: KeyboardEvent) => {
  if (nativeAndroidRuntime && event.key === 'ArrowDown') {
    event.preventDefault();
    if ($('androidViewMenu').classList.contains('hidden')) toggleAndroidViewMenu();
    document.getElementById('androidViewMenu')!.querySelector<HTMLButtonElement>('[data-view-action="search"]')?.focus();
  }
});
$('androidViewBackdrop').addEventListener('click', closeAndroidViewMenu);
window.addEventListener('resize', sizeAndroidViewMenu);
$('androidViewMenu').addEventListener('click', (event: MouseEvent) => {
  const action = (event.target as Element).closest<HTMLButtonElement>('[data-view-action]');
  if (!action || action.disabled) return;
  switch (action.dataset.viewAction) {
    case 'search': openAndroidChatSearch(); break;
    case 'browser': void openAndroidBrowserDrawer(); break;
    case 'browser-tools': openAndroidBrowserControls(); break;
    case 'pets': showPet($('petBuddy').classList.contains('hidden'), true); closeAndroidViewMenu(); break;
    case 'zoom-in': setAndroidViewZoom(androidViewZoom + .1); break;
    case 'zoom-out': setAndroidViewZoom(androidViewZoom - .1); break;
    case 'zoom-reset': setAndroidViewZoom(1); break;
  }
});
type AndroidInAppTab = { tabId: number; title?: string; url?: string; active?: boolean };
type AndroidInAppStatus = { opened: boolean; activeTabId: number;
  automationAllowed: boolean; isolatedProfile: boolean; profileIsolationSupported: boolean;
  lastProfileCleanup?: string;
  tabs: AndroidInAppTab[] };
type AndroidDomSnapshot = { tabId: number; snapshotId: number;
  title?: string; text?: string; elements?: Array<{
    index: number; tag: string; role?: string; label?: string; fillable: boolean;
  }> };
let androidInAppStatus: AndroidInAppStatus | null = null;
let androidDomSnapshot: AndroidDomSnapshot | null = null;
let androidBrowserPreviewEpoch = 0;
let androidBrowserPreviewBusy = false;
const androidAgentProviderModels: Record<string,string> = {
  'chatgpt-browser':'chatgpt.com',openai:'',anthropic:'',openrouter:''
};
function renderAndroidAgentModel() {
  if (!nativeAndroidRuntime) return;
  const provider = $('provider').value;
  const apiProvider = ['openai','anthropic','openrouter'].includes(provider);
  const model = $('model').value.trim();
  const keyReady = !!state?.hasKeys?.includes(provider);
  ($('androidAgentProvider') as HTMLSelectElement).value = provider;
  $('androidAgentApiSettings').classList.toggle('hidden',!apiProvider);
  const agentModel = $('androidAgentModel') as HTMLInputElement;
  if (document.activeElement !== agentModel) agentModel.value = apiProvider ? model : '';
  const indicator = $('androidAgentModelStatus');
  indicator.dataset.state = !apiProvider ? 'idle' : keyReady && model ? 'active' : 'pending';
  indicator.textContent = !apiProvider
    ? 'ChatGPT account: text replies plus direct chat commands such as a pasted HTTPS link or “buka YouTube”. Complex agent tasks require an API model.'
    : !model ? 'Enter a model ID supported by your API account before sending.'
    : keyReady ? `${provider} model key ready · ${model}. Android agent tools are automatically available from chat requests.`
    : `Add a separate ${provider} API key for intelligent tool calling. Direct browser/app commands still work from chat without this key.`;
}
if (nativeAndroidRuntime) {
  $('androidAgentProvider').addEventListener('change',() => {
    const provider = ($('androidAgentProvider') as HTMLSelectElement).value;
    if (!['chatgpt-browser','openai','anthropic','openrouter'].includes(provider)) return;
    const previous = $('provider').value;
    if (previous !== 'chatgpt-browser') androidAgentProviderModels[previous] = $('model').value.trim();
    $('provider').value = provider;
    $('model').value = provider === 'chatgpt-browser' ? 'chatgpt.com' : androidAgentProviderModels[provider] || '';
    $('provider').dispatchEvent(new Event('change'));
    $('model').dispatchEvent(new Event('input'));
    renderAndroidAgentModel();
  });
  $('androidAgentModel').addEventListener('input',() => {
    const provider = $('provider').value;
    if (!['openai','anthropic','openrouter'].includes(provider)) return;
    const model = ($('androidAgentModel') as HTMLInputElement).value.trim();
    androidAgentProviderModels[provider] = model;
    $('model').value = model;
    $('model').dispatchEvent(new Event('input'));
    renderAndroidAgentModel();
  });
  $('androidAgentSaveKey').addEventListener('click',async () => {
    const provider = $('provider').value;
    const input = $('androidAgentApiKey') as HTMLInputElement;
    if (!['openai','anthropic','openrouter'].includes(provider) || !input.value.trim()) return;
    const button = $('androidAgentSaveKey') as HTMLButtonElement;
    button.disabled = true;
    try {
      await api('/api/key',{provider,key:input.value});
      input.value = '';
      verifiedAiConnection = null;
      await refresh();
      announce('Agent model key available in memory for this app session.',true);
    } catch {
      $('androidAgentModelStatus').textContent = 'Model API key was not accepted; review the provider and try again.';
    } finally { button.disabled = false; renderAndroidAgentModel(); }
  });
  $('androidAgentClearKey').addEventListener('click',async () => {
    const provider = $('provider').value;
    if (!['openai','anthropic','openrouter'].includes(provider)) return;
    const button = $('androidAgentClearKey') as HTMLButtonElement;
    button.disabled = true;
    try {
      await api('/api/key',{provider,key:''});
      ($('androidAgentApiKey') as HTMLInputElement).value = '';
      verifiedAiConnection = null;
      await refresh();
      announce('Model API key cleared from memory.',true);
    } catch {
      $('androidAgentModelStatus').textContent = 'Unable to confirm model API key removal.';
    } finally { button.disabled = false; renderAndroidAgentModel(); }
  });
}
function browserMessage(message: string) { $('androidBrowserStatus').textContent = message; }
let androidBrowserDrawerOpening = false;
async function openAndroidBrowserDrawer():Promise<void> {
  if (!nativeAndroidRuntime || !isTauri() || androidBrowserDrawerOpening) return;
  androidBrowserDrawerOpening = true;
  const trigger = $('androidBrowserDrawerToggle') as HTMLButtonElement;
  trigger.disabled = true;
  closeAndroidViewMenu();
  closeSidebar();
  closeToolMenu();
  // The native browser is the single visible browser surface. Do not stack
  // the separate DOM inspector above the right drawer.
  closeAndroidBrowserControls();
  try {
    const status = await invoke<AndroidInAppStatus>('in_app_browser_status');
    if (status.opened) {
      browserMessage('Private browser already running. Stop / Close ends the whole session.');
      return;
    }
    if (!status.profileIsolationSupported) {
      announce('Private browser unavailable: isolated Android WebView profile is unsupported.');
      return;
    }
    // Only an explicit click arrives here. The browser owns the address/search
    // controls; changing automation modes must never start navigation.
    const opened = await invoke<{opened:boolean;cancelled?:boolean}>('in_app_browser_open', {
      url:'https://www.google.com/',userInitiated:true,
    });
    browserMessage(opened.opened
      ? 'Browser drawer opened. Search, Refresh, Back and Close are in its native toolbar.'
      : 'Opening the browser drawer was cancelled.');
  } catch (error) {
    browserMessage('Browser drawer unavailable; no unsafe fallback was opened.');
    announce('Browser drawer unavailable: ' + String(error).slice(0,140));
  } finally {
    androidBrowserDrawerOpening = false;
    trigger.disabled = false;
  }
}
$('androidBrowserDrawerToggle').addEventListener('click',() => void openAndroidBrowserDrawer());
function clearAndroidBrowserPreview() {
  androidBrowserPreviewEpoch++;
  $('androidBrowserPreviewOutput').replaceChildren();
  $('androidBrowserPreviewOutput').classList.add('hidden');
}
function updateAndroidBrowserPreviewButton() {
  $('androidBrowserPreviewOnce').disabled = androidBrowserPreviewBusy ||
    !androidInAppStatus?.opened || !androidInAppStatus.activeTabId;
}
function openAndroidBrowserControls() {
  if (!nativeAndroidRuntime || !isTauri()) return;
  closeAndroidViewMenu();
  closeSidebar();
  $('androidBrowserBackdrop').classList.remove('hidden');
  $('androidBrowserUrl').focus({ preventScroll: true });
  void refreshAndroidInAppBrowser();
}
function closeAndroidBrowserControls() {
  clearAndroidBrowserPreview();
  $('androidBrowserBackdrop').classList.add('hidden');
  // The native browser has its own visible Close control; hiding this panel
  // must not silently dismiss pages or revoke consent.
}
function renderAndroidInAppTabs() {
  const list = $('androidBrowserTabs');
  list.replaceChildren();
  if (!androidInAppStatus?.tabs?.length) {
    list.append(el('p', 'tool-empty', 'No in-app browser tabs open.'));
    return;
  }
  for (const tab of androidInAppStatus.tabs) {
    const wrapper = el('div', 'android-browser-tab');
    const open = el('button', '', tab.title?.trim() || tab.url || `Tab ${tab.tabId}`);
    open.type = 'button';
    open.setAttribute('aria-pressed', String(tab.tabId === androidInAppStatus.activeTabId));
    open.addEventListener('click', async () => {
      clearAndroidBrowserPreview();
      try {
        await invoke('in_app_browser_switch_tab', { tabId: tab.tabId });
        androidDomSnapshot = null;
        await refreshAndroidInAppBrowser();
      } catch (error) { browserMessage(String(error.message)); }
    });
    const close = el('button', '', 'Close');
    close.type = 'button';
    close.setAttribute('aria-label', `Close in-app browser tab ${tab.tabId}`);
    close.addEventListener('click', async () => {
      clearAndroidBrowserPreview();
      try {
        await invoke('in_app_browser_close_tab', { tabId: tab.tabId });
        androidDomSnapshot = null;
        await refreshAndroidInAppBrowser();
      } catch (error) { browserMessage(String(error.message)); }
    });
    wrapper.append(open, close);
    list.append(wrapper);
  }
}
async function refreshAndroidInAppBrowser() {
  if (!nativeAndroidRuntime || !isTauri()) return;
  try {
    const status = await invoke<AndroidInAppStatus>('in_app_browser_status');
    if (androidInAppStatus?.activeTabId !== status.activeTabId ||
        (androidInAppStatus?.opened && !status.opened) ||
        !status.tabs?.some(tab => tab.tabId === status.activeTabId))
      clearAndroidBrowserPreview();
    androidInAppStatus = status;
    browserMessage(status.opened
      ? `${status.tabs.length} tab(s) · DOM tools ${status.automationAllowed ? 'allowed' : 'disabled'} · ${status.isolatedProfile ? 'separate WebView data profile' : 'profile isolation unverified'}`
      : status.profileIsolationSupported
        ? `Browser closed · separate profile supported · last cleanup: ${status.lastProfileCleanup || 'not attempted'}`
        : 'Browser unavailable: this Android WebView cannot provide a separate data profile. No unsafe fallback.');
    $('androidBrowserConsent').disabled = !status.opened;
    $('androidBrowserStop').disabled = !status.opened;
    $('androidBrowserSnapshot').disabled = !status.automationAllowed || !status.opened;
    updateAndroidBrowserPreviewButton();
    renderAndroidInAppTabs();
  } catch (error) { browserMessage(String(error.message)); }
}
$('androidBrowserClose').addEventListener('click', closeAndroidBrowserControls);
$('androidBrowserBackdrop').addEventListener('click', (event: MouseEvent) => {
  if (event.target === $('androidBrowserBackdrop')) closeAndroidBrowserControls();
});
$('androidBrowserOpenForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  if (!nativeAndroidRuntime || !isTauri()) return;
  const url = $('androidBrowserUrl').value.trim();
  const button = $('androidBrowserOpenForm').querySelector('button[type="submit"]');
  button.disabled = true;
  browserMessage('Android will ask you to confirm the new in-app WebView…');
  try {
    const status = await invoke<{ opened: boolean; cancelled?: boolean }>('in_app_browser_open', {
      url, userInitiated: true,
    });
    if (status.opened) {
      androidDomSnapshot = null;
      clearAndroidBrowserPreview();
      browserMessage('In-app tab opened. JavaScript/DOM tools still require separate consent.');
    } else browserMessage('Opening cancelled; no WebView opened.');
    await refreshAndroidInAppBrowser();
  } catch (error) { browserMessage(String(error.message)); }
  finally { button.disabled = false; }
});
$('androidBrowserConsent').addEventListener('click', async () => {
  $('androidBrowserConsent').disabled = true;
  browserMessage('Android will ask for visible-session DOM automation permission…');
  try {
    const result = await invoke<{ allowed: boolean }>('in_app_browser_automation', { userInitiated: true });
    androidDomSnapshot = null;
    browserMessage(result.allowed ? 'DOM tools consent granted for this visible browser session.' : 'DOM tools were not enabled.');
    await refreshAndroidInAppBrowser();
  } catch (error) { browserMessage(String(error.message)); }
  finally { $('androidBrowserConsent').disabled = false; }
});
$('androidBrowserRefresh').addEventListener('click', () => void refreshAndroidInAppBrowser());
$('androidBrowserStop').addEventListener('click', async () => {
  if (!window.confirm('Close all in-app browser tabs? No personal Chrome tabs will be affected.')) return;
  clearAndroidBrowserPreview();
  try {
    await invoke('in_app_browser_close_all');
    androidDomSnapshot = null;
    $('androidBrowserSnapshotOutput').replaceChildren();
    await refreshAndroidInAppBrowser();
  } catch (error) { browserMessage(String(error.message)); }
});
$('androidBrowserPreviewOnce').addEventListener('click', async () => {
  const tabId = androidInAppStatus?.activeTabId;
  if (!nativeAndroidRuntime || !isTauri() || androidBrowserPreviewBusy ||
      !androidInAppStatus?.opened || !tabId) return;
  if (!window.confirm('Preview the selected in-app browser tab ONCE? Android will request separate capture permission. This in-memory image will not be saved, uploaded or refreshed automatically.'))
    return;
  clearAndroidBrowserPreview();
  const epoch = androidBrowserPreviewEpoch;
  androidBrowserPreviewBusy = true;
  updateAndroidBrowserPreviewButton();
  browserMessage('Preparing a one-time preview; Android will ask for capture approval separately…');
  try {
    const prepared = await invoke<{tabId:number;previewToken:string;pageGeneration:number;expiresInMs:number}>(
      'in_app_browser_prepare_preview', {tabId,userInitiated:true});
    if (epoch !== androidBrowserPreviewEpoch || androidInAppStatus?.activeTabId !== tabId) return;
    if (prepared.tabId !== tabId || typeof prepared.previewToken !== 'string' ||
        !/^[A-Za-z0-9_-]{8,128}$/.test(prepared.previewToken) ||
        !Number.isSafeInteger(prepared.pageGeneration) || prepared.pageGeneration < 1 ||
        !Number.isSafeInteger(prepared.expiresInMs) || prepared.expiresInMs <= 0 ||
        prepared.expiresInMs > 60_000)
      throw Error('Invalid preview preparation receipt');
    const captured = await invoke<{captured:boolean;cancelled?:boolean;tabId:number;
      previewTokenConsumed:boolean;pageGeneration:number;mimeType:string;base64:string;
      width:number;height:number;pngBytes:number}>('in_app_browser_capture_preview',
      {tabId,previewToken:prepared.previewToken,userInitiated:true});
    if (epoch !== androidBrowserPreviewEpoch || androidInAppStatus?.activeTabId !== tabId ||
        !androidInAppStatus?.opened) return; // stale tab or closed panel, never display
    if (captured.captured === false && captured.cancelled === true) {
      browserMessage('One-time preview cancelled in Android; no image retained.');
      return;
    }
    const {width,height,pngBytes,base64} = captured;
    if (captured.captured !== true || captured.previewTokenConsumed !== true ||
        captured.tabId !== tabId || captured.pageGeneration !== prepared.pageGeneration ||
        captured.mimeType !== 'image/png' || !Number.isSafeInteger(width) ||
        !Number.isSafeInteger(height) || width < 1 || height < 1 ||
        width > 1024 || height > 1024 || width * height > 786432 ||
        !Number.isSafeInteger(pngBytes) || pngBytes < 57 || pngBytes > 1048576 ||
        typeof base64 !== 'string' || base64.length > 1398104 ||
        base64.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(base64))
      throw Error('Preview response exceeded safety limits');
    const binary = atob(base64);
    if (binary.length !== pngBytes || !binary.startsWith('\x89PNG\r\n\x1a\n'))
      throw Error('Preview was not a valid bounded PNG payload');
    const image = document.createElement('img');
    image.alt = 'One-time preview of selected in-app browser tab';
    image.width = width;
    image.height = height;
    image.style.maxWidth = '100%'; // Only necessary Android preview containment
    image.style.maxHeight = '240px';
    image.style.objectFit = 'contain';
    image.src = `data:image/png;base64,${base64}`;
    const output = $('androidBrowserPreviewOutput');
    output.replaceChildren(image);
    output.classList.remove('hidden');
    browserMessage('One-time preview shown in memory; not saved or refreshed.');
  } catch (error) {
    if (epoch === androidBrowserPreviewEpoch) browserMessage(`Preview unavailable: ${String(error.message).slice(0,180)}`);
  } finally {
    androidBrowserPreviewBusy = false;
    updateAndroidBrowserPreviewButton();
  }
});
$('androidBrowserSnapshot').addEventListener('click', async () => {
  if (!androidInAppStatus?.automationAllowed || !androidInAppStatus.activeTabId) return;
  browserMessage('Reading bounded DOM snapshot of the visible owned tab…');
  try {
    const snapshot = await invoke<AndroidDomSnapshot>('in_app_browser_snapshot',
      { tabId: androidInAppStatus.activeTabId });
    androidDomSnapshot = snapshot;
    const output = $('androidBrowserSnapshotOutput');
    output.replaceChildren(el('strong', '', snapshot.title || 'Current page'));
    const text = el('pre', '', (snapshot.text || '').slice(0, 6000));
    output.append(text);
    for (const item of snapshot.elements || []) {
      output.append(el('div', 'android-browser-node',
        `#${item.index} · ${item.tag} · ${item.label || '(unlabeled)'}${item.fillable ? ' · editable' : ''}`));
    }
    browserMessage(`${snapshot.elements?.length || 0} indexed elements captured. Snapshot invalidates after every action.`);
  } catch (error) { browserMessage(String(error.message)); }
});
$('androidBrowserActionForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  const snap = androidDomSnapshot;
  if (!snap) { browserMessage('Inspect the visible tab before using indexed actions.'); return; }
  const action = $('androidBrowserAction').value;
  const index = Number($('androidBrowserIndex').value);
  const raw = $('androidBrowserValue').value;
  if (!Number.isInteger(index) || index < (action === 'scroll' ? 0 : 1) || index > 64) {
    browserMessage('Element index must match the latest DOM snapshot.');
    return;
  }
  if (index > 0 && !snap.elements?.some(e => e.index === index)) {
    browserMessage('Indexed element is not in the latest snapshot.');
    return;
  }
  if (action === 'fill' && !snap.elements?.find(e => e.index === index)?.fillable) {
    browserMessage('Selected element is not an editable field.');
    return;
  }
  if (!window.confirm(`Run ${action} in the currently visible in-app browser tab? The website may change.`)) return;
  const command = ({click:'in_app_browser_click', fill:'in_app_browser_fill',
    scroll:'in_app_browser_scroll', inspect:'in_app_browser_inspect',
    hover:'in_app_browser_hover'} as Record<string,string>)[action];
  if (!command) return;
  const args: Record<string,unknown> = { tabId: snap.tabId, snapshotId: snap.snapshotId, index };
  if (action === 'fill') args.value = raw;
  if (action === 'scroll') {
    const delta = Number(raw);
    if (!Number.isInteger(delta) || Math.abs(delta) > 2000) {
      browserMessage('Scroll distance must be an integer between -2000 and 2000.');
      return;
    }
    args.deltaY = delta;
  }
  if (action !== 'inspect') androidDomSnapshot = null;
  try {
    const result = await invoke<{ok?:boolean;tabId?:number;snapshotId?:number;
      index?:number;tag?:string;role?:string;label?:string;
      attributes?:{type?:string;disabled?:boolean;readOnly?:boolean;required?:boolean};
      snapshotInvalidated?:boolean;syntheticEvents?:boolean;physicalPointerMoved?:boolean}>(command, args);
    if (action === 'inspect') {
      if (result.ok !== true || result.tabId !== snap.tabId ||
          result.snapshotId !== snap.snapshotId || result.index !== index ||
          result.snapshotInvalidated !== false ||
          !/^[a-z]{1,12}$/.test(result.tag || '') ||
          typeof result.label !== 'string' || result.label.length > 120 ||
          !result.attributes || typeof result.attributes !== 'object')
        throw Error('Indexed inspection receipt did not match the selected snapshot');
      const safeLabel = mobileAiDelegatePreview(result.label, 120);
      const summary = `#${index} · ${result.tag} · ${result.role || 'no role'} · ${safeLabel || '(no safe label)'} · type: ${result.attributes.type || 'none'} · disabled: ${result.attributes.disabled === true} · read only: ${result.attributes.readOnly === true}`;
      $('androidBrowserSnapshotOutput').append(el('div', 'android-browser-node', summary));
      browserMessage('One indexed node inspected (safe metadata only). Snapshot retained for the next action.');
      return;
    }
    if (action === 'hover' && (result.ok !== true || result.tabId !== snap.tabId ||
        result.snapshotInvalidated !== true || result.syntheticEvents !== true ||
        result.physicalPointerMoved !== false))
      throw Error('Hover was not confirmed as synthetic DOM events');
    $('androidBrowserSnapshotOutput').replaceChildren();
    browserMessage(action === 'hover'
      ? 'Synthetic DOM hover events dispatched. No physical pointer moved; inspect again before another action.'
      : 'Action executed. Inspect again before the next action.');
  } catch (error) { browserMessage(String(error.message)); }
});
$('androidViewMenu').addEventListener('keydown', (event: KeyboardEvent) => {
  if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
  event.preventDefault();
  const items = [...document.getElementById('androidViewMenu')!.querySelectorAll<HTMLButtonElement>('[role="menuitem"]:not(:disabled)')];
  const index = items.indexOf(document.activeElement as HTMLButtonElement);
  const next = event.key === 'Home' ? 0 : event.key === 'End' ? items.length - 1 :
    (index + (event.key === 'ArrowDown' ? 1 : -1) + items.length) % items.length;
  items[next]?.focus();
});
$('sidebar').addEventListener('click', (event: MouseEvent) => {
  const button = (event.target as Element).closest<HTMLButtonElement>('[data-sidebar-extra]');
  if (!button) return;
  if (nativeAndroidRuntime && button.dataset.sidebarExtra === 'plugins') {
    setSettings(true);
    setAndroidSettingsView('plugins');
  } else openExtraView(button.dataset.sidebarExtra || '');
});

function renderThreads() {
  const wrapper = $('threads');
  wrapper.replaceChildren();
  $('threadCount').textContent = String(state.sessions.length);
  const query = nativeAndroidRuntime || state.platform === 'android' ? $('sidebarSearch').value.trim().toLocaleLowerCase() : '';
  const sessions = state.sessions.filter(session => {
    if (selectedProjectId && projectAssignments[session.id] !== selectedProjectId) return false;
    if (!query) return true;
    // Search actual locally persisted messages, not only the first user
    // title. No provider request, server indexing or external upload.
    return (session.messages || []).some(message =>
      typeof message.text === 'string' && message.text.toLocaleLowerCase().includes(query));
  });
  if (!sessions.length) {
    const message = query ? 'No matching chats' :
      selectedProjectId ? 'No chats in this project' : 'No conversations yet';
    wrapper.append(el('span', 'thread-muted', message));
  }
  for (const session of sessions) {
    const title = session.messages?.find(message => message.role === 'user')?.text || 'Conversation';
    const button = el('button', session.id === selectedSession ? 'selected' : '');
    button.type = 'button';
    button.dataset.sessionId = session.id;
    button.title = title;
    const threadIcon = el('span', 'thread-icon');
    threadIcon.append(nativeAndroidRuntime ? uiIcon('message') : document.createTextNode('▤'));
    button.append(threadIcon, el('span', 'thread-label', title.slice(0, 65)));
    button.disabled = working;
    button.setAttribute('aria-current', session.id === selectedSession ? 'page' : 'false');
    button.addEventListener('click', () => {
      if (working) return;
      ++speechContextRevision;
      selectedSession = session.id;
      // Android has exactly one live chat engine. Historical direct-provider
      // conversations stay readable, but are not silently converted or
      // answered using a different engine.
      $('provider').value = nativeAndroidRuntime ? 'chatgpt-browser' : (session.provider || 'openai');
      $('provider').dispatchEvent(new Event('change'));
      $('model').value = nativeAndroidRuntime ? 'chatgpt.com' : (session.model || '');
      if (nativeAndroidRuntime && session.provider !== 'chatgpt-browser')
        announce('This earlier provider chat is read-only in ChatGPT-only mode. Start New chat to send with ChatGPT.');
      displayMessages(session);
      renderThreads();
      renderProjects();
      closeSidebar();
    });
    wrapper.append(button);
  }
  renderProjects();
}

function closeToolMenu() {
  $('toolPanelMenu').classList.add('hidden');
  $('toolPanelBackdrop').classList.add('hidden');
  $('toolPanelToggle').setAttribute('aria-expanded', 'false');
}
function closeExtrasPanel() {
  $('extrasPanel').classList.add('hidden');
  extraView = null;
}
function renderMobileSkills() {
  const list = $('mobileSkillsList');
  list.replaceChildren();
  const skills = state.skills || [];
  $('mobileSkillCount').textContent = String(skills.length);
  const filter = $('mobileSkillsSearch').value.trim().toLocaleLowerCase();
  const matching = filter ? skills.filter(skill =>
    (skill.name || skill.id).toLocaleLowerCase().includes(filter)) : skills;
  if (!skills.length) {
    list.append(el('div', 'tool-empty', 'No custom skills yet. Create one below; it starts disabled.'));
    return;
  }
  if (!matching.length) {
    list.append(el('div', 'tool-empty', 'No skills matching this search.'));
    return;
  }
  for (const skill of matching) {
    const row = el('div', 'mobile-skill-item');
    const copy = el('div', 'skill-copy');
    copy.append(el('strong', '', skill.name || skill.id),
      el('small', '', skill.enabled ? 'Enabled for new AI requests' : 'Disabled · stored on this device'));
    const label = el('label', 'skill-enable');
    const toggle = el('input');
    toggle.type = 'checkbox';
    toggle.checked = skill.enabled;
    toggle.setAttribute('aria-label', `Enable ${skill.name || skill.id}`);
    label.append(toggle);
    toggle.addEventListener('change', async () => {
      toggle.disabled = true;
      try {
        await api('/api/mobile-skills', { action: 'toggle', id: skill.id, enabled: toggle.checked });
        await refresh();
        $('mobileSkillsFeedback').textContent = 'Skill updated';
      } catch (error) {
        toggle.checked = !toggle.checked;
        $('mobileSkillsFeedback').textContent = String(error.message);
        $('mobileSkillsFeedback').classList.add('error');
      } finally { toggle.disabled = false; }
    });
    const remove = el('button', 'mobile-skill-remove');
    remove.append(uiIcon('close'));
    remove.type = 'button';
    remove.setAttribute('aria-label', `Remove ${skill.name || skill.id}`);
    remove.addEventListener('click', async () => {
      remove.disabled = true;
      try {
        await api('/api/mobile-skills', { action: 'delete', id: skill.id });
        await refresh();
        $('mobileSkillsFeedback').textContent = 'Skill removed';
      } catch (error) {
        remove.disabled = false;
        $('mobileSkillsFeedback').textContent = String(error.message);
        $('mobileSkillsFeedback').classList.add('error');
      }
    });
    row.append(copy, label, remove);
    list.append(row);
  }
}
$('mobileSkillsSearch').addEventListener('input', renderMobileSkills);
function showPet(enabled: boolean, save = false) {
  $('petBuddy').classList.toggle('hidden', !enabled);
  $('mobilePetEnabled').checked = enabled;
  if (save) {
    try { localStorage.setItem('mcp-mobile-pocket-bot-v1', enabled ? 'on' : 'off'); }
    catch { /* Pet preview still works for this app session. */ }
  }
}
const androidPetGlyphs:Record<string,string> = {
  bot:'🤖',cat:'🐱',dog:'🐶',fox:'🦊',rabbit:'🐰',
  panda:'🐼',dragon:'🐲',owl:'🦉'
};
function setPetSpecies(value:string,save=false):void {
  const species=Object.hasOwn(androidPetGlyphs,value)?value:'bot';
  $('mobilePetSpecies').value=species;
  $('petBuddy').dataset.pet=species;
  $('petBuddyGlyph').textContent=androidPetGlyphs[species];
  $('petBuddy').title=`Drag to move ${species}; tap to customize`;
  if(save)try{localStorage.setItem('mcp-mobile-pet-species-v1',species);}catch{}
}
$('mobilePetSpecies').addEventListener('change',()=>setPetSpecies($('mobilePetSpecies').value,true));
try{setPetSpecies(localStorage.getItem('mcp-mobile-pet-species-v1')||'bot');}
catch{setPetSpecies('bot');}
type PetScreenPosition={x:number;y:number};
function applyPetScreenPosition(value:PetScreenPosition):void {
  if(!Number.isFinite(value.x)||!Number.isFinite(value.y)||
      value.x<0||value.y<0||value.x>1||value.y>1)return;
  const pet=$('petBuddy') as HTMLElement;
  pet.style.position='fixed';
  pet.style.right='auto';pet.style.bottom='auto';
  pet.style.left=Math.round(value.x*Math.max(0,window.innerWidth-58))+'px';
  pet.style.top=Math.round(value.y*Math.max(0,window.innerHeight-68))+'px';
}
try{
  const saved=JSON.parse(localStorage.getItem('mcp-mobile-pet-point-v1')||'null');
  if(saved&&typeof saved==='object')applyPetScreenPosition(saved);
}catch{}
let petDrag:{pointerId:number;startX:number;startY:number;left:number;top:number;moved:boolean}|null=null;
let ignorePetClick=false;
const petNode=$('petBuddy') as HTMLButtonElement;
petNode.addEventListener('pointerdown',(event:PointerEvent)=>{
  if(!nativeAndroidRuntime||event.button!==0)return;
  const rect=petNode.getBoundingClientRect();
  petDrag={pointerId:event.pointerId,startX:event.clientX,startY:event.clientY,
    left:rect.left,top:rect.top,moved:false};
  petNode.setPointerCapture(event.pointerId);
});
petNode.addEventListener('pointermove',(event:PointerEvent)=>{
  const current=petDrag;
  if(!current||current.pointerId!==event.pointerId)return;
  const dx=event.clientX-current.startX,dy=event.clientY-current.startY;
  if(!current.moved&&Math.hypot(dx,dy)<7)return;
  current.moved=true;
  applyPetScreenPosition({
    x:Math.max(0,Math.min(1,(current.left+dx)/Math.max(1,window.innerWidth-58))),
    y:Math.max(0,Math.min(1,(current.top+dy)/Math.max(1,window.innerHeight-68)))
  });
});
function completePetDrag(event:PointerEvent):void {
  const current=petDrag;
  if(!current||current.pointerId!==event.pointerId)return;
  petDrag=null;
  if(current.moved){
    ignorePetClick=true;
    const rect=petNode.getBoundingClientRect();
    try{localStorage.setItem('mcp-mobile-pet-point-v1',JSON.stringify({
      x:rect.left/Math.max(1,window.innerWidth-58),
      y:rect.top/Math.max(1,window.innerHeight-68)
    }));}catch{}
  }
}
petNode.addEventListener('pointerup',completePetDrag);
petNode.addEventListener('pointercancel',completePetDrag);
petNode.addEventListener('click',event=>{
  if(!ignorePetClick)return;
  ignorePetClick=false;
  event.preventDefault();event.stopImmediatePropagation();
},{capture:true});
function setPetPosition(value: string, save = false) {
  const position = value === 'left' ? 'left' : 'right';
  $('mobilePetPosition').value = position;
  $('petBuddy').dataset.position = position;
  if (save) {
    const pet=$('petBuddy') as HTMLElement;
    for(const property of ['position','left','top','right','bottom'])
      pet.style.removeProperty(property);
    try{localStorage.removeItem('mcp-mobile-pet-point-v1');}catch{}
    try { localStorage.setItem('mcp-mobile-pocket-bot-position-v1', position); }
    catch { /* Pet still moves for the current app session. */ }
  }
}
$('mobilePetPosition').addEventListener('change', () => setPetPosition($('mobilePetPosition').value, true));
try { setPetPosition(localStorage.getItem('mcp-mobile-pocket-bot-position-v1') || 'right'); }
catch { setPetPosition('right'); }
type PetOverlayState = {
  supported:boolean; permissionGranted:boolean; visible:boolean; foreground:boolean;
  pendingConsent:boolean; sizeDp:number; mode:string;
};
let petOverlayBusy = false;
function validPetOverlayState(value: any): value is PetOverlayState {
  return value && typeof value === 'object' && !Array.isArray(value) &&
    ['supported','permissionGranted','visible','foreground','pendingConsent']
      .every(key => typeof value[key] === 'boolean') &&
    value.sizeDp === 56 && value.mode === 'app-scoped' &&
    (!value.visible || (value.permissionGranted && value.foreground &&
      value.supported && !value.pendingConsent));
}
function petOverlayButtons(snapshot: PetOverlayState | null) {
  $('petOverlayShow').disabled = petOverlayBusy || !snapshot?.supported ||
    !snapshot.permissionGranted || !snapshot.foreground || snapshot.visible ||
    snapshot.pendingConsent;
  $('petOverlayHide').disabled = petOverlayBusy || !snapshot?.visible;
  $('petOverlayRefresh').disabled = petOverlayBusy;
}
async function refreshPetOverlay() {
  const enabled = nativeAndroidRuntime && isTauri();
  $('petOverlayControls').classList.toggle('hidden', !enabled);
  if (!enabled || extraView !== 'pets') return;
  petOverlayButtons(null);
  const feedback = $('petOverlayStatus');
  try {
    const result = await invoke<PetOverlayState>('pet_overlay_status');
    if (extraView !== 'pets') return;
    if (!validPetOverlayState(result)) throw Error('Invalid Android overlay status');
    petOverlayButtons(result);
    feedback.textContent = !result.supported ? 'This Android version does not support the overlay.' :
      !result.permissionGranted ? 'Overlay permission not granted. Use Permissions Center to request it explicitly.' :
      result.visible ? 'Pocket Bot is attached now. Android removes it when this app leaves foreground.' :
      'Permission granted; overlay is off. Nothing starts automatically.';
  } catch (error) {
    if (extraView === 'pets') {
      petOverlayButtons(null);
      feedback.textContent = `Unable to verify overlay status: ${String((error as Error).message).slice(0,160)}`;
    }
  }
}
$('petOverlayRefresh').addEventListener('click', () => void refreshPetOverlay());
$('petOverlayShow').addEventListener('click', async () => {
  if (petOverlayBusy || !nativeAndroidRuntime || !isTauri() || extraView !== 'pets') return;
  if (!window.confirm('Request one foreground-only Pocket Bot overlay? Android requires a separate overlay grant and an additional native Allow-once confirmation. It will not survive leaving the app.')) return;
  petOverlayBusy = true;
  petOverlayButtons(null);
  $('petOverlayStatus').textContent = 'Awaiting native one-time overlay consent…';
  try {
    const result = await invoke<PetOverlayState & {shown:boolean;cancelled:boolean}>(
      'pet_overlay_show', {userInitiated:true});
    if (!validPetOverlayState(result) || typeof result.shown !== 'boolean' ||
        typeof result.cancelled !== 'boolean' ||
        (result.shown && (!result.visible || result.cancelled)) ||
        (!result.shown && (!result.cancelled || result.visible)))
      throw Error('No verifiable native overlay receipt');
    $('petOverlayStatus').textContent = result.shown ?
      'Pocket Bot shown by Android in this foreground session only.' :
      'Overlay request cancelled; no bot shown.';
  } catch (error) {
    $('petOverlayStatus').textContent =
      `Overlay not confirmed: ${String((error as Error).message).slice(0,160)}`;
  } finally {
    petOverlayBusy = false;
    if (extraView === 'pets') await refreshPetOverlay();
  }
});
$('petOverlayHide').addEventListener('click', async () => {
  if (petOverlayBusy || !nativeAndroidRuntime || !isTauri() || extraView !== 'pets') return;
  petOverlayBusy = true;
  petOverlayButtons(null);
  try {
    const result = await invoke<PetOverlayState & {hidden:boolean}>(
      'pet_overlay_hide',{userInitiated:true});
    if (!validPetOverlayState(result) || result.hidden !== true || result.visible)
      throw Error('Android did not confirm overlay removal');
    $('petOverlayStatus').textContent = 'Foreground Pocket Bot overlay removed.';
  } catch (error) {
    $('petOverlayStatus').textContent =
      `Overlay removal not confirmed: ${String((error as Error).message).slice(0,160)}`;
  } finally {
    petOverlayBusy = false;
    if (extraView === 'pets') await refreshPetOverlay();
  }
});
function openExtraView(which: string) {
  if (state.platform !== 'android' && !nativeAndroidRuntime) return;
  if (!['plugins', 'skills', 'pets'].includes(which)) return;
  closeToolPanel();
  closeSidebar();
  setSettings(false);
  extraView = which;
  $('extrasPanel').classList.remove('hidden');
  $('extrasTitle').textContent = which[0].toUpperCase() + which.slice(1);
  for (const tab of document.querySelectorAll<HTMLButtonElement>('[data-extras-tab]'))
    tab.setAttribute('aria-current', String(tab.dataset.extrasTab === which));
  for (const name of ['Plugins', 'Skills', 'Pets'])
    $(`extras${name}`).classList.toggle('hidden', name.toLowerCase() !== which);
  if (which === 'skills') renderMobileSkills();
  if (which === 'plugins') void refreshMobileMcpStatus();
  if (which === 'pets') void refreshPetOverlay();
}
$('extrasTabs').addEventListener('click', (event: MouseEvent) => {
  const tab = (event.target as Element).closest<HTMLButtonElement>('[data-extras-tab]');
  if (tab) openExtraView(tab.dataset.extrasTab || '');
});
$('extrasClose').addEventListener('click', closeExtrasPanel);
$('androidPluginsOpenExternalMcp').addEventListener('click',()=>openExtraView('plugins'));
$('pluginTerminal').addEventListener('click', event => openToolPanel('terminal', event.isTrusted));
$('pluginFiles').addEventListener('click', () => openToolPanel('files'));
type MobileMcpTool = { name: string; description?: string; inputSchema?: unknown };
let mobileMcpConnected = false;
let mobileMcpTools: MobileMcpTool[] = [];
let mobileSelectedTool: string | null = null;
function showMobileMcpTools() {
  const view = $('mobileMcpToolsList');
  view.replaceChildren();
  $('mobileMcpToolCount').textContent = String(mobileMcpTools.length);
  if (!mobileMcpTools.length) {
    view.append(el('div', 'tool-empty', mobileMcpConnected
      ? 'Connected server has no tools to display.'
      : 'Connect a remote MCP server to discover its tools.'));
  }
  for (const tool of mobileMcpTools) {
    const row = el('button', 'mobile-mcp-tool');
    row.type = 'button';
    row.append(el('strong', '', tool.name),
      el('small', '', (tool.description || 'Remote MCP tool').slice(0, 300)));
    row.addEventListener('click', () => {
      mobileSelectedTool = tool.name;
      $('mobileMcpSelectedTool').textContent = tool.name;
      $('mobileMcpArgs').value = '{}';
      $('mobileMcpToolForm').classList.remove('hidden');
      $('mobileMcpResult').classList.add('hidden');
    });
    view.append(row);
  }
}
function setMobileMcpConnected(connected: boolean) {
  mobileMcpConnected = connected;
  // Initial module setup calls this before Goal state is initialized; defer
  // the Android-only view update until the current JS turn has finished.
  if (nativeAndroidRuntime) queueMicrotask(() => refreshGoalMcpOptions());
  const enableForNext = $('mobileMcpModelOptIn') as HTMLInputElement;
  // Android ChatGPT website chat does not use the direct provider/model tool
  // proposal API. Keep manual MCP tool calls and their explicit review, but
  // never offer a misleading model opt-in toggle.
  enableForNext.checked = false;
  enableForNext.disabled = true;
  $('mobileMcpModelOptInRow').classList.add('hidden');
  $('mobileMcpDisconnect').disabled = !connected;
  $('mobileMcpReload').disabled = !connected;
  const preview = !nativeAndroidRuntime || !isTauri();
  $('mobileMcpConnect').disabled = preview;
  $('mobileMcpEndpoint').disabled = preview;
  $('mobileMcpToken').disabled = preview;
  const summary = $('extrasPlugins').querySelector('.extras-summary strong');
  if (summary) summary.textContent = connected ? 'Remote MCP server connected' : 'No MCP plugins connected';
  const description = $('extrasPlugins').querySelector('.extras-summary small');
  if (description) description.textContent = connected
    ? 'Explicit HTTPS MCP session active. Remote tools run only after your confirmation.'
    : 'HTTPS JSON or bounded SSE MCP requests are supported on Android. Local stdio and long-lived SSE subscriptions remain unavailable.';
}
async function refreshMobileMcpStatus() {
  if (!nativeAndroidRuntime || !isTauri()) {
    setMobileMcpConnected(false);
    $('mobileMcpFeedback').textContent = 'Remote MCP requires the installed Android app.';
    showMobileMcpTools();
    return;
  }
  try {
    const status = await api('/api/mcp', { action: 'status' });
    setMobileMcpConnected(status.connected === true);
    if (!status.connected) {
      mobileMcpTools = [];
      mobileSelectedTool = null;
      $('mobileMcpToolForm').classList.add('hidden');
      showMobileMcpTools();
    }
  } catch (error) {
    setMobileMcpConnected(false);
    $('mobileMcpFeedback').textContent = String(error.message);
  }
}
$('mobileMcpConnectForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  if (!nativeAndroidRuntime || !isTauri() || mobileMcpConnected) return;
  const endpoint = $('mobileMcpEndpoint').value.trim();
  if (!endpoint) return;
  $('mobileMcpConnect').disabled = true;
  $('mobileMcpFeedback').textContent = 'Connecting to the HTTPS MCP server…';
  try {
    const result = await api('/api/mcp', {
      action: 'connect', endpoint, token: $('mobileMcpToken').value,
    });
    $('mobileMcpToken').value = '';
    mobileMcpTools = Array.isArray(result.tools?.tools) ? result.tools.tools : [];
    setMobileMcpConnected(result.connected === true);
    showMobileMcpTools();
    $('mobileMcpFeedback').textContent = `Connected · ${mobileMcpTools.length} remote tool(s)`;
  } catch (error) {
    $('mobileMcpToken').value = '';
    setMobileMcpConnected(false);
    $('mobileMcpFeedback').textContent = String(error.message);
  } finally {
    $('mobileMcpConnect').disabled = !nativeAndroidRuntime || !isTauri() || mobileMcpConnected;
  }
});
$('mobileMcpDisconnect').addEventListener('click', async () => {
  try {
    await api('/api/mcp', { action: 'disconnect' });
    mobileMcpTools = [];
    mobileSelectedTool = null;
    setMobileMcpConnected(false);
    showMobileMcpTools();
    $('mobileMcpToolForm').classList.add('hidden');
    $('mobileMcpResult').classList.add('hidden');
    $('mobileMcpFeedback').textContent = 'Remote MCP connection closed.';
  } catch (error) { $('mobileMcpFeedback').textContent = String(error.message); }
});
$('mobileMcpReload').addEventListener('click', async () => {
  $('mobileMcpReload').disabled = true;
  try {
    const result = await api('/api/mcp', { action: 'list' });
    mobileMcpTools = Array.isArray(result.tools) ? result.tools : [];
    setMobileMcpConnected(true);
    showMobileMcpTools();
    $('mobileMcpFeedback').textContent = `${mobileMcpTools.length} tool(s) loaded.`;
  } catch (error) { $('mobileMcpFeedback').textContent = String(error.message); }
  finally { $('mobileMcpReload').disabled = !mobileMcpConnected; }
});
$('mobileMcpToolForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  const name = mobileSelectedTool;
  if (!mobileMcpConnected || !name || !mobileMcpTools.some(t => t.name === name)) return;
  let argumentsObject: Record<string, unknown>;
  try {
    const data = JSON.parse($('mobileMcpArgs').value);
    if (!data || Array.isArray(data) || typeof data !== 'object') throw Error();
    argumentsObject = data;
  } catch {
    $('mobileMcpFeedback').textContent = 'Enter a valid JSON object of tool arguments.';
    return;
  }
  if (!window.confirm(`Run remote MCP tool "${name}"? This may send data or change the remote service.`)) return;
  const button = $('mobileMcpToolForm').querySelector('button[type="submit"]');
  button.disabled = true;
  $('mobileMcpFeedback').textContent = 'Running the selected tool…';
  try {
    const response = await api('/api/mcp', { action: 'call', name,
      arguments: argumentsObject, userInitiated: true });
    $('mobileMcpResult').textContent = JSON.stringify(response, null, 2).slice(0, 10000);
    $('mobileMcpResult').classList.remove('hidden');
    $('mobileMcpFeedback').textContent = response.isError ? 'Tool reported an error.' : 'Remote tool response received.';
  } catch (error) { $('mobileMcpFeedback').textContent = String(error.message); }
  finally { button.disabled = false; }
});
setMobileMcpConnected(false);
showMobileMcpTools();
$('mobileSkillForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  const button = $('mobileSkillForm').querySelector('button');
  button.disabled = true;
  $('mobileSkillsFeedback').classList.remove('error');
  $('mobileSkillsFeedback').textContent = '';
  try {
    await api('/api/mobile-skills', {
      action: 'add',
      name: $('mobileSkillName').value,
      instructions: $('mobileSkillInstructions').value,
    });
    $('mobileSkillName').value = '';
    $('mobileSkillInstructions').value = '';
    await refresh();
    $('mobileSkillsFeedback').textContent = 'Skill saved disabled. Switch it on when ready.';
  } catch (error) {
    $('mobileSkillsFeedback').classList.add('error');
    $('mobileSkillsFeedback').textContent = String(error.message);
  } finally { button.disabled = false; }
});
type ReviewedSkillImport = { treeUri: string; path: string[]; name: string; instructions: string; raw: string };
let pendingSkillImport: ReviewedSkillImport | null = null;
function readSkillImportPath(): string[] {
  const raw = String($('mobileSkillImportPath').value).trim();
  if (raw.length > 256 || raw.startsWith('/') || raw.includes('\\') || raw.includes('\0'))
    throw Error('Invalid relative SKILL.md path');
  const path = raw.split('/');
  if (path.length > 16 || path.at(-1) !== 'SKILL.md' || path.some(segment =>
    !segment || segment === '.' || segment === '..' || segment.startsWith('.') ||
    segment.length > 128 || !/^[\p{L}\p{N} _.-]+$/u.test(segment)))
    throw Error('Enter a safe relative path ending in SKILL.md');
  return path;
}
async function readSkillImport(treeUri: string, path: string[]): Promise<string> {
  const result = await invoke<{ data: string; encoding: string; size: number }>(
    'document_tree_open', {treeUri, path});
  if (result.encoding !== 'base64' || result.size > 12_000 || result.size < 1)
    throw Error('SKILL.md must be UTF-8 text up to 12 KiB');
  const bytes = Uint8Array.from(atob(result.data), c => c.charCodeAt(0));
  if (bytes.length !== result.size) throw Error('SKILL.md size did not match provider receipt');
  return new TextDecoder('utf-8', {fatal:true}).decode(bytes);
}
function parseSkillImport(raw: string): {name:string;instructions:string} {
  const cleaned = raw.replace(/\r\n/g, '\n').replace(/^\uFEFF/, '');
  if (!cleaned.startsWith('---\n')) throw Error('SKILL.md needs YAML frontmatter with a name');
  const end = cleaned.indexOf('\n---\n', 4);
  if (end < 0 || end > 2_048) throw Error('SKILL.md frontmatter is missing or too large');
  const frontmatter = cleaned.slice(4,end);
  // Only a single-line `name:` is imported; scripts/manifest directives are
  // never evaluated and unknown YAML fields cannot enable capabilities.
  const names = [...frontmatter.matchAll(/^name:\s*(.+?)\s*$/gmi)];
  if (names.length !== 1) throw Error('SKILL.md needs exactly one name field');
  const name = names[0][1].replace(/^(?:"([^"]*)"|'([^']*)')$/, '$1$2').trim();
  const instructions = cleaned.slice(end+5).trim();
  if (!name || Array.from(name).length > 55 || /[\x00-\x1f]/.test(name) ||
      !instructions || new TextEncoder().encode(instructions).length > 2_000 || instructions.includes('\0'))
    throw Error('Skill name or instructions exceed the existing private library limits');
  return {name,instructions};
}
function resetSkillImportReview() {
  pendingSkillImport = null;
  $('mobileSkillImportPreview').classList.add('hidden');
  $('mobileSkillImportApprove').classList.add('hidden');
}
$('mobileSkillImportPath').addEventListener('input', resetSkillImportReview);
$('mobileSkillImportForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  resetSkillImportReview();
  if (!nativeAndroidRuntime || !isTauri()) return;
  const button = $('mobileSkillImportPreviewButton');
  button.disabled = true;
  $('mobileSkillImportStatus').textContent = 'Checking the selected Android folder…';
  try {
    const path = readSkillImportPath();
    const status = await invoke<{granted:boolean;treeUri:string}>('document_tree_status');
    if (!status.granted || !status.treeUri) throw Error('Choose a readable Android folder in Files first');
    const raw = await readSkillImport(status.treeUri, path);
    const {name,instructions} = parseSkillImport(raw);
    pendingSkillImport = {treeUri:status.treeUri,path,name,instructions,raw};
    $('mobileSkillImportPreview').textContent = `Name: ${name}\nStarts disabled\n\n${instructions}`;
    $('mobileSkillImportPreview').classList.remove('hidden');
    $('mobileSkillImportApprove').classList.remove('hidden');
    $('mobileSkillImportStatus').textContent = 'Review the content below before explicitly importing; no skill was saved or enabled.';
  } catch (error) {
    $('mobileSkillImportStatus').textContent = `Nothing imported: ${error.message}`;
  } finally { button.disabled = false; }
});
$('mobileSkillImportApprove').addEventListener('click', async () => {
  const preview = pendingSkillImport;
  if (!preview || !nativeAndroidRuntime || !isTauri()) return;
  const button = $('mobileSkillImportApprove');
  button.disabled = true;
  $('mobileSkillImportStatus').textContent = 'Rechecking reviewed SKILL.md…';
  try {
    if (readSkillImportPath().join('/') !== preview.path.join('/')) throw Error('Import path changed');
    const status = await invoke<{granted:boolean;treeUri:string}>('document_tree_status');
    if (!status.granted || status.treeUri !== preview.treeUri) throw Error('Android folder selection changed');
    const fresh = await readSkillImport(preview.treeUri, preview.path);
    if (fresh !== preview.raw) throw Error('SKILL.md changed since preview. Review again before importing');
    if (!window.confirm(`Import reviewed skill "${preview.name}" as DISABLED?\n\n${preview.instructions.slice(0,250)}`)) return;
    const result = await api('/api/mobile-skills', {
      action:'add',name:preview.name,instructions:preview.instructions,
    });
    if (!Array.isArray(result.skills) ||
      !result.skills.some((skill:any) => skill.name === preview.name && skill.enabled === false))
      throw Error('Native skill library did not confirm a disabled import');
    resetSkillImportReview();
    await refresh();
    $('mobileSkillImportStatus').textContent = `Imported ${preview.name} as disabled. Review it before enabling.`;
  } catch (error) {
    $('mobileSkillImportStatus').textContent = `Import not confirmed: ${error.message}`;
    resetSkillImportReview();
  } finally { button.disabled = false; }
});
$('mobilePetEnabled').addEventListener('change', () => showPet($('mobilePetEnabled').checked, true));
$('petBuddy').addEventListener('click', () => openExtraView('pets'));
try { showPet(localStorage.getItem('mcp-mobile-pocket-bot-v1') === 'on'); }
catch { showPet(false); }
function closeToolPanel() {
  closeToolMenu();
  clearSelectedImagePreview();
  invalidateGoalReviewRequest();
  workspacePanel = null;
  $('toolViewPanel').classList.add('hidden');
  $('toolViewPanel').dataset.workspacePanel='none';
  $('androidSubagentsToggle').setAttribute('aria-expanded','false');
  closePrimeOptionsMenu();
}
// Only native-core's successful, explicitly user-approved model MCP receipt
// qualifies as Review evidence. Never display arguments, results, or arbitrary
// metadata from Session.actions; the native store limits a session to 128.
function reviewedMcpToolNames(session: Session): string[] {
  if (!Array.isArray(session.actions) || session.actions.length > 128) return [];
  const names: string[] = [];
  for (const entry of session.actions) {
    if (!entry || typeof entry !== 'object' || Array.isArray(entry)) continue;
    const action = entry as Record<string, unknown>;
    if (action.type === 'mcp_tool_call' && action.approvedByUser === true &&
        action.resultIsError === false && action.resultStored === false &&
        typeof action.name === 'string' && /^[A-Za-z0-9_.:-]{1,64}$/.test(action.name))
      names.push(action.name);
  }
  return names;
}
type ReviewMcpAuditEvent = {
  reviewId: string; requestId: string; sessionId: string | null;
  name: string; status: string; atUnixMs: number;
  approvedByUser: boolean; resultStored: boolean;
};
let reviewMcpAudit: ReviewMcpAuditEvent[] | null = null;
let reviewMcpAuditState = 'Open Review to check persisted MCP approval metadata.';
let reviewMcpAuditRequest = 0;
type GoalReviewEvent = {
  type: 'turn' | 'manual_approval';
  kind: 'user_attested_completed' | 'retryable_failure' | 'terminal_failure' |
    'needs_approval' | 'approved' | 'declined';
  stepIndex: number; attempt: number | null; turnNumber: number | null;
  ticketId: string | null; independentlyVerified: false;
};
type GoalReviewProvenance = {
  goalId: string; revision: number; status: string; pausedFrom: string | null;
  currentStep: number; totalSteps: number; turnsUsed: number; turnBudget: number;
  turnsRemaining: number; retriesUsedOnStep: number; historyEventCount: number;
  approvalEventCount: number; independentlyVerified: false;
  crossStreamChronologyKnown: false;
  eventOrder: 'turn_history_then_approval_history'; events: GoalReviewEvent[];
};
const goalReviewUuid = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const goalReviewStatuses = ['ready','awaiting_approval','paused','in_turn','completed','failed','rejected','cancelled'];
const goalReviewTurnKinds = ['user_attested_completed','retryable_failure','terminal_failure','needs_approval'];
let goalReviewReceipt: GoalReviewProvenance | null = null;
let goalReviewState = 'Open Review to inspect a saved Goal checkpoint.';
let goalReviewRequest = 0;
function invalidateGoalReviewRequest(): void {
  ++goalReviewRequest;
  goalReviewReceipt = null;
  goalReviewState = 'Open Review to inspect a saved Goal checkpoint.';
  $('reviewGoalProvenanceDetails').replaceChildren();
  $('reviewGoalProvenanceState').textContent = goalReviewState;
}
function validateGoalReviewResponse(receipt: any): GoalReviewProvenance | null {
  const object = (value: any): value is Record<string, any> =>
    !!value && typeof value === 'object' && !Array.isArray(value);
  const exact = (value: any, names: string[]) =>
    object(value) && Object.keys(value).length === names.length &&
    Object.keys(value).every(name => names.includes(name));
  const integer = (value: any, min: number, max: number) =>
    Number.isSafeInteger(value) && value >= min && value <= max;
  const validUuid = (value: any) => typeof value === 'string' && goalReviewUuid.test(value);
  if (!object(receipt)) throw Error('Invalid Goal Review receipt.');
  if (exact(receipt,['goal','provenance']) && receipt.goal === null &&
      receipt.provenance === null) {
    if (mobileGoalSnapshot) throw Error('Latest Goal checkpoint disagrees with empty review.');
    return null;
  }
  const p = receipt.provenance;
  if (!exact(receipt, ['goalId','provenance']) || !validUuid(receipt.goalId) ||
      !exact(p, ['goalId','revision','status','pausedFrom','currentStep',
        'totalSteps','turnsUsed','turnBudget','turnsRemaining','retriesUsedOnStep',
        'historyEventCount','approvalEventCount','independentlyVerified',
        'crossStreamChronologyKnown','eventOrder','events']) ||
      p.goalId !== receipt.goalId || !integer(p.revision,0,Number.MAX_SAFE_INTEGER) ||
      !goalReviewStatuses.includes(p.status) ||
      !integer(p.totalSteps,1,16) || !integer(p.currentStep,0,p.totalSteps) ||
      !integer(p.turnBudget,p.totalSteps,64) || !integer(p.turnsUsed,0,p.turnBudget) ||
      !integer(p.turnsRemaining,0,p.turnBudget) ||
      p.turnsRemaining !== p.turnBudget - p.turnsUsed ||
      !integer(p.retriesUsedOnStep,0,6) ||
      !integer(p.historyEventCount,0,64) ||
      !integer(p.approvalEventCount,0,80) ||
      p.independentlyVerified !== false || p.crossStreamChronologyKnown !== false ||
      p.eventOrder !== 'turn_history_then_approval_history' ||
      !Array.isArray(p.events) || p.events.length > 144 ||
      new TextEncoder().encode(JSON.stringify(p)).length > 32 * 1024 ||
      p.events.length !== p.historyEventCount + p.approvalEventCount ||
      (p.status === 'completed') !== (p.currentStep === p.totalSteps) ||
      (p.status === 'paused') !== (p.pausedFrom !== null) ||
      (p.pausedFrom !== null && !['ready','awaiting_approval'].includes(p.pausedFrom)) ||
      (p.status !== 'in_turn' && p.status !== 'cancelled' &&
        p.historyEventCount !== p.turnsUsed) ||
      (p.status === 'in_turn' && p.historyEventCount + 1 !== p.turnsUsed) ||
      (p.status === 'cancelled' && p.historyEventCount !== p.turnsUsed &&
        p.historyEventCount + 1 !== p.turnsUsed)) throw Error('Invalid Goal Review provenance.');
  const seen = new Set<string>();
  let complete = 0;
  let attempt = 1;
  let previousApprovalStep = 0;
  for (const [index, event] of p.events.entries()) {
    if (!exact(event, ['type','kind','stepIndex','attempt','turnNumber',
      'ticketId','independentlyVerified']) ||
      event.independentlyVerified !== false ||
      !integer(event.stepIndex,0,p.totalSteps - 1)) throw Error('Invalid Goal Review event.');
    if (index < p.historyEventCount) {
      if (event.type !== 'turn' || !goalReviewTurnKinds.includes(event.kind) ||
          !validUuid(event.ticketId) || seen.has(event.ticketId) ||
          !integer(event.attempt,1,6) || event.attempt !== attempt ||
          event.turnNumber !== index + 1 || event.stepIndex !== complete)
        throw Error('Invalid Goal Review turn history.');
      seen.add(event.ticketId);
      if (event.kind === 'user_attested_completed') { complete++; attempt = 1; }
      else if (event.kind === 'retryable_failure') attempt++;
      if (event.kind === 'terminal_failure' && index + 1 !== p.historyEventCount)
        throw Error('Invalid Goal Review terminal order.');
    } else {
      if (event.type !== 'manual_approval' ||
          !['approved','declined'].includes(event.kind) ||
          event.attempt !== null || event.turnNumber !== null ||
          event.ticketId !== null ||
          event.stepIndex < previousApprovalStep || event.stepIndex > p.currentStep ||
          (event.kind === 'declined' && index + 1 !== p.events.length))
        throw Error('Invalid Goal Review approval history.');
      previousApprovalStep = event.stepIndex;
    }
  }
  if (complete !== p.currentStep || (p.historyEventCount === 0 && p.retriesUsedOnStep !== 0) ||
      (p.historyEventCount > 0 && p.retriesUsedOnStep !== attempt - 1) ||
      (p.status === 'rejected' && p.events.at(-1)?.kind !== 'declined') ||
      (p.events.at(-1)?.kind === 'declined' && p.status !== 'rejected'))
    throw Error('Inconsistent Goal Review counters.');
  if (mobileGoalSnapshot && (mobileGoalSnapshot.id !== p.goalId ||
      mobileGoalSnapshot.status !== p.status ||
      mobileGoalSnapshot.current_step !== p.currentStep ||
      mobileGoalSnapshot.turns_used !== p.turnsUsed))
    throw Error('Goal Review differs from the latest displayed Goal checkpoint.');
  return p as GoalReviewProvenance;
}
function renderGoalReviewProvenance(): void {
  const section = $('reviewGoalProvenance');
  section.classList.toggle('hidden', !nativeAndroidRuntime || !isTauri());
  if (!nativeAndroidRuntime || !isTauri()) return;
  const body = $('reviewGoalProvenanceDetails');
  body.replaceChildren();
  $('reviewGoalProvenanceState').textContent = goalReviewState;
  const p = goalReviewReceipt;
  if (!p) return;
  const summary = el('div','local-worker-job');
  const goalTitle = el('strong','', 'Goal ' + p.goalId);
  goalTitle.style.overflowWrap = 'anywhere';
  summary.append(goalTitle,
    el('small','', 'Revision ' + p.revision + ' · ' + p.status.replaceAll('_',' ') +
      (p.pausedFrom ? ' (paused from ' + p.pausedFrom.replaceAll('_',' ') + ')' : '')),
    el('small','', 'Current step ' + p.currentStep + '/' + p.totalSteps +
      ' · ' + p.turnsUsed + '/' + p.turnBudget + ' turns · ' + p.turnsRemaining +
      ' remaining · retry count ' + p.retriesUsedOnStep),
    el('small','', p.historyEventCount + ' recorded turn events · ' +
      p.approvalEventCount + ' manual approval events'),
    el('small','', 'Caller-attested outcomes only. Independently verified: No.'),
    el('small','', 'Turn history followed by approval history; not a cross-stream chronological timeline.'));
  body.append(summary);
  if (!p.events.length) {
    body.append(el('small','', 'No persisted Goal turn or approval events.'));
    return;
  }
  for (const event of p.events) {
    const row = el('div','local-worker-job');
    const title = event.kind === 'user_attested_completed' ? 'User-attested completed (not independently verified)' :
      event.kind === 'retryable_failure' ? 'Retryable failure' :
      event.kind === 'terminal_failure' ? 'Terminal failure' :
      event.kind === 'needs_approval' ? 'Turn requested approval' :
      event.kind === 'approved' ? 'Manual approval · approved' : 'Manual approval · declined';
    const details = el('small','', event.type === 'turn'
        ? 'Step ' + (event.stepIndex + 1) + ' · attempt ' + event.attempt +
          ' · turn ' + event.turnNumber + ' · ticket ' + event.ticketId
        : 'Step ' + (event.stepIndex + 1) + ' · no ticket or turn number recorded');
    details.style.overflowWrap = 'anywhere';
    row.append(el('strong','', title), details);
    body.append(row);
  }
}
async function loadGoalReviewProvenance(): Promise<void> {
  if (workspacePanel !== 'review' || !nativeAndroidRuntime || !isTauri()) return;
  const request = ++goalReviewRequest;
  goalReviewReceipt = null;
  goalReviewState = 'Checking saved Goal checkpoint provenance…';
  renderGoalReviewProvenance();
  try {
    const receipt = await api('/api/goal-review', {});
    if (workspacePanel !== 'review' || request !== goalReviewRequest) return;
    goalReviewReceipt = validateGoalReviewResponse(receipt);
    goalReviewState = goalReviewReceipt
      ? 'Saved checkpoint metadata only; no independent verification claimed.'
      : 'No saved Goal checkpoint. No provenance events to display.';
  } catch {
    if (workspacePanel !== 'review' || request !== goalReviewRequest) return;
    goalReviewReceipt = null;
    goalReviewState = 'Goal Review unavailable or stale. No provenance inferred.';
  }
  renderGoalReviewProvenance();
}
function renderReviewMcpAudit(): HTMLElement {
  const view = el('div', 'local-worker-job');
  view.append(el('strong', '', 'MCP approval audit · metadata only'));
  if (reviewMcpAudit === null) {
    view.append(el('small', '', reviewMcpAuditState));
    return view;
  }
  if (!reviewMcpAudit.length) {
    view.append(el('small', '', 'No persisted user-approved MCP dispatches recorded.'));
    return view;
  }
  const statuses: Record<string, string> = {
    approved_dispatched: 'Approved dispatch; server execution NOT confirmed',
    server_confirmed: 'MCP server response confirmed; final answer not yet saved',
    server_reported_error: 'MCP server reported error; side effects may still have occurred',
    answer_saved: 'Server response confirmed; final AI answer saved',
  };
  const counts = Object.fromEntries(Object.keys(statuses)
    .map(status => [status, reviewMcpAudit!.filter(event => event.status === status).length]));
  view.append(el('small', '', `${reviewMcpAudit.length} approval record(s) · ${counts.approved_dispatched} unconfirmed · ${counts.server_confirmed} server responses · ${counts.server_reported_error} server errors · ${counts.answer_saved} answers saved`));
  for (const event of reviewMcpAudit.slice(-6).reverse()) {
    view.append(el('small', '', `${event.name} · ${statuses[event.status]}`));
  }
  if (reviewMcpAudit.length > 6)
    view.append(el('small', '', `Showing the latest 6 of ${reviewMcpAudit.length} metadata records.`));
  return view;
}
async function loadReviewMcpAudit() {
  if (workspacePanel !== 'review' || !nativeAndroidRuntime || !isTauri()) return;
  const request = ++reviewMcpAuditRequest;
  reviewMcpAudit = null;
  reviewMcpAuditState = 'Checking local MCP approval receipts…';
  renderReview();
  try {
    const response = await api('/api/mcp-audit', {});
    if (workspacePanel !== 'review' || request !== reviewMcpAuditRequest) return;
    if (!Array.isArray(response.events) || response.events.length > 128)
      throw Error('Audit response failed its safety bound');
    const safeNames = /^[A-Za-z0-9_.:-]{1,64}$/;
    const validStatuses = ['approved_dispatched','server_confirmed','server_reported_error','answer_saved'];
    reviewMcpAudit = response.events.filter((event: any) =>
      event && typeof event === 'object' && !Array.isArray(event) &&
      event.approvedByUser === true && event.resultStored === false &&
      typeof event.name === 'string' && safeNames.test(event.name) &&
      validStatuses.includes(event.status) &&
      Number.isSafeInteger(event.atUnixMs) && event.atUnixMs > 0 &&
      typeof event.reviewId === 'string' && typeof event.requestId === 'string');
    renderReview();
  } catch {
    if (workspacePanel !== 'review' || request !== reviewMcpAuditRequest) return;
    reviewMcpAudit = null;
    reviewMcpAuditState = 'MCP audit unavailable; no execution status inferred.';
    renderReview();
  }
}
function renderReview() {
  const view = $('reviewSummary');
  view.replaceChildren();
  if (nativeAndroidRuntime && isTauri()) view.append(renderReviewMcpAudit());
  if (!state.sessions.length) {
    view.append(el('div', 'tool-empty', 'No recorded conversations yet. Your completed chats will appear here.'));
    return;
  }
  for (const session of state.sessions) {
    const label = session.messages?.find(m => m.role === 'user')?.text || 'Conversation';
    const row = el('button');
    row.type = 'button';
    const copy = el('span');
    copy.append(el('strong', '', label.slice(0, 80)), el('small', '', `${session.provider} · ${session.messages.length} messages`));
    const approvedTools = reviewedMcpToolNames(session);
    if (approvedTools.length) {
      const uniqueNames = [...new Set(approvedTools)];
      const shown = uniqueNames.slice(0, 3).join(', ');
      const remaining = uniqueNames.length - 3;
      copy.append(el('small', 'review-mcp-evidence',
        `${approvedTools.length} recorded user-approved MCP model tool ${approvedTools.length === 1 ? 'call' : 'calls'} · ${shown}${remaining > 0 ? ` (+${remaining} more names)` : ''}`));
    }
    row.append(uiIcon('message'), copy, uiIcon('chevron'));
    row.addEventListener('click', () => {
      if (working) return;
      selectedProjectId = null;
      $('sidebarSearch').value = '';
      renderThreads();
      const thread = Array.from(document.querySelectorAll<HTMLButtonElement>('#threads button'))
        .find(item => item.dataset.sessionId === session.id);
      if (thread) { closeToolPanel(); thread.click(); }
    });
    const exportButton = el('button', 'review-export-button', 'Export');
    exportButton.type = 'button';
    exportButton.setAttribute('aria-label', `Export conversation ${label.slice(0,30)} to Markdown`);
    exportButton.addEventListener('click', async () => {
      exportButton.disabled = true;
      $('reviewShareMarkdown').classList.add('hidden');
      $('reviewExportPreview').value = '';
      $('reviewShareStatus').textContent = '';
      $('reviewExportStatus').textContent = 'Preparing local Markdown export…';
      try {
        const data = await api('/api/session-export', { id: session.id });
        const text = String(data.content);
        if (text.length > 131072) throw Error('Export exceeded safety limit');
        $('reviewExportPreview').value = text;
        $('reviewExportPreview').classList.remove('hidden');
        $('reviewShareMarkdown').classList.toggle('hidden', !nativeAndroidRuntime || !isTauri());
        try {
          await navigator.clipboard.writeText(text);
          $('reviewExportStatus').textContent = 'Conversation Markdown copied. A selectable copy is below.';
        } catch {
          $('reviewExportStatus').textContent = 'Clipboard unavailable. Select the Markdown text below to copy.';
        }
      } catch (error) {
        $('reviewExportStatus').textContent = String(error.message);
      } finally { exportButton.disabled = false; }
    });
    const wrapper = el('div', 'review-export-row');
    wrapper.append(row, exportButton);
    view.append(wrapper);
  }
}
$('reviewShareMarkdown').addEventListener('click', async () => {
  if (!nativeAndroidRuntime || !isTauri()) return;
  const text = String($('reviewExportPreview').value);
  if (!text || new TextEncoder().encode(text).length > 131072) {
    $('reviewShareStatus').textContent = 'Export unavailable or too large; no share action was opened.';
    return;
  }
  if (!window.confirm('Open the Android share chooser with the reviewed Markdown? Select the destination yourself; nothing is shared automatically.')) return;
  $('reviewShareMarkdown').disabled = true;
  $('reviewShareStatus').textContent = 'Requesting Android share chooser…';
  try {
    const result = await invoke<{opened?: boolean}>('review_share_markdown', {text,userInitiated:true});
    $('reviewShareStatus').textContent = result?.opened === true
      ? 'Android share chooser opened. Delivery depends on your selection.'
      : 'Android share chooser not confirmed; nothing was shared by the app.';
  } catch (error) {
    $('reviewShareStatus').textContent = `Share chooser unavailable: ${error.message}`;
  } finally { $('reviewShareMarkdown').disabled = false; }
});
function terminalLine(command: string, output: string, error = false) {
  const node = el('div', 'terminal-line');
  if (command) node.append(el('div', 'terminal-command', `› ${command}`));
  node.append(el('div', error ? 'terminal-error' : '', output));
  $('terminalOutput').append(node);
  $('terminalOutput').scrollTop = $('terminalOutput').scrollHeight;
}
let activeNativeTerminalId: string | null = null;
let androidPtyBundleLoading = false;
type AndroidPtyController = {
  boot:()=>Promise<void>;
  show:(projectId:string|null, userOpenedTerminal:boolean)=>void
};
function ensureAndroidPtyWorkspace(userOpenedTerminal = false) {
  if (!nativeAndroidRuntime) return;
  $('androidPtyWorkspace').classList.remove('hidden');
  const controller = (window as any).__mcpAndroidPty as AndroidPtyController | undefined;
  if (controller) {
    controller.show(selectedProjectId, userOpenedTerminal);
    void controller.boot();
    return;
  }
  if (androidPtyBundleLoading) return;
  androidPtyBundleLoading = true;
  const script = document.createElement('script');
  script.src = './android-pty-client.js';
  script.onload = () => {
    androidPtyBundleLoading = false;
    const loaded = (window as any).__mcpAndroidPty as AndroidPtyController | undefined;
    if (loaded) {
      loaded.show(selectedProjectId, userOpenedTerminal);
      void loaded.boot();
    }
    else $('androidPtyStatus').textContent = 'Android terminal bundle is unavailable.';
  };
  script.onerror = () => {
    androidPtyBundleLoading = false;
    $('androidPtyStatus').textContent = 'Android terminal bundle could not be loaded.';
  };
  document.head.append(script);
}
type AndroidCliProbe = {probeSucceeded?:boolean;path?:string;error?:string;
  status?:string;exitCode?:number;available?:boolean};
type AndroidToolchainReport = {
  supported?:boolean;kind?:string;executionContext?:string;
  node?:AndroidCliProbe;npm?:AndroidCliProbe;sshClient?:AndroidCliProbe;
  git?:AndroidCliProbe;python3?:AndroidCliProbe;
  nodeCodeExecutionProbeSucceeded?:boolean;
  nodeNpmCliProbesSucceeded?:boolean;
  sshProjectTransportImplemented?:boolean;
  npmInstallTested?:boolean;
};
let androidToolchainChecking=false;
async function checkAndroidInstalledRuntimes():Promise<void>{
  if(!nativeAndroidRuntime)return;
  const status=$('androidRuntimeStatus');
  const button=$('androidRuntimeCheck') as HTMLButtonElement;
  if(androidToolchainChecking)return;
  if(!isTauri()){
    status.textContent='Preview mode cannot check Android executable files.';
    return;
  }
  androidToolchainChecking=true;
  button.disabled=true;
  status.textContent='Probing executable tools in this app’s Android shell…';
  $('androidRuntimeResults').replaceChildren();
  try{
    const receipt=await invoke<AndroidToolchainReport>('android_pty_capabilities');
    if(receipt?.supported!==true||receipt.kind!=='android-in-app-pty'||
      receipt.executionContext!=='app-uid-user-operated-shell')
      throw Error('Native Android toolchain receipt is unavailable');
    const tools:[string,AndroidCliProbe|undefined][]=[
      ['Node.js',receipt.node],['npm',receipt.npm],
      ['OpenSSH',receipt.sshClient],['Git',receipt.git],['Python 3',receipt.python3]
    ];
    for(const [name,entry] of tools){
      const item=el('div','android-runtime-tool');
      const success=entry?.probeSucceeded===true;
      item.dataset.ready=String(success);
      item.append(el('strong','',name),
        el('span','',success?'Executable verified':'Not verified in app shell'));
      $('androidRuntimeResults').append(item);
    }
    status.textContent=receipt.nodeCodeExecutionProbeSucceeded===true &&
      receipt.nodeNpmCliProbesSucceeded===true
      ? 'Node code execution and npm CLI were verified inside this Android app. npm package installation is not tested.'
      : 'Node/npm execution is not verified inside this app. Termux has a separate Android UID and cannot silently supply its binaries.';
    if(receipt.sshProjectTransportImplemented!==true)
      status.textContent+=' SSH project authentication has not been verified.';
  }catch(error){
    status.textContent='Could not verify native runtimes: '+String(error).slice(0,170);
  }finally{
    androidToolchainChecking=false;
    button.disabled=false;
  }
}
$('androidRuntimeCheck').addEventListener('click',()=>void checkAndroidInstalledRuntimes());
type TermuxStatus = {supported:boolean;installed:boolean;launchable:boolean;
  mode:string;inAppShellAvailable:boolean;setupVerified:boolean;
  installIsUserManaged:boolean;commandsAvailableInApp:boolean};
type TermuxRunStatus={supported:boolean;installed:boolean;
  runCommandPermissionGranted:boolean;foreground:boolean;running:boolean;
  termuxResultSupported:boolean;allowExternalAppsVerified:boolean;
  termuxExecutionContext:'external-termux-app';termuxWorkspaceShared:false;
  userInitiatedRequired:true;maxConcurrency:number;maxOutputBytes:number;maxWaitMs:number};
let termuxRunStatus:TermuxRunStatus|null=null;
let termuxRunBusy=false;
const termuxActions:Record<string,[string,string][]>={
  node:[['version','Check version'],['check','Check JavaScript syntax'],['run','Run a Termux JavaScript file']],
  npm:[['version','Check version'],['list','List offline project packages'],['install','Offline install without lifecycle scripts']],
  ssh:[['version','Check version'],['test','Test strict key-authenticated host']]
};
function syncTermuxActionOptions():void{
  const tool=($('androidTermuxCommandTool') as HTMLSelectElement).value;
  const select=$('androidTermuxCommandAction') as HTMLSelectElement;
  const previous=select.value;
  select.replaceChildren();
  for(const [value,label] of termuxActions[tool]||[]){
    const option=document.createElement('option');option.value=value;option.textContent=label;
    select.append(option);
  }
  if((termuxActions[tool]||[]).some(([value])=>value===previous))select.value=previous;
  const action=select.value;
  const projectNeeded=(tool==='node'&&action!=='version')||
    (tool==='npm'&&action!=='version');
  $('androidTermuxProjectInputs').classList.toggle('hidden',!projectNeeded);
  ($('androidTermuxEntry') as HTMLInputElement).classList.toggle('hidden',tool!=='node'||action==='version');
  ($('androidTermuxEntry').previousElementSibling as HTMLElement).classList.toggle('hidden',tool!=='node'||action==='version');
  $('androidTermuxSshInputs').classList.toggle('hidden',tool!=='ssh'||action==='version');
}
function refreshTermuxRunButton():void{
  ($('androidTermuxCommandRun') as HTMLButtonElement).disabled=termuxRunBusy||
    !nativeAndroidRuntime||!isTauri()||termuxRunStatus?.runCommandPermissionGranted!==true||
    termuxRunStatus?.installed!==true||termuxRunStatus?.termuxResultSupported!==true||
    termuxRunStatus?.foreground!==true;
}
async function refreshTermuxRunStatus():Promise<void>{
  if(!nativeAndroidRuntime||!isTauri())return;
  const status=$('androidTermuxCommandStatus');
  try{
    const reply=await invoke<TermuxRunStatus>('android_termux_command_status');
    if(reply.termuxExecutionContext!=='external-termux-app'||reply.termuxWorkspaceShared!==false||
        reply.userInitiatedRequired!==true||reply.maxConcurrency!==1||
        reply.maxOutputBytes!==16384||reply.maxWaitMs!==30000)
      throw Error('Unexpected Termux command status');
    termuxRunStatus=reply;
    status.textContent=!reply.installed?'Termux is not installed. Install it first.':
      !reply.termuxResultSupported?'Installed Termux does not support verified command-result callbacks.':
      !reply.runCommandPermissionGranted?'Android RUN_COMMAND permission not granted to Mcp Agent Ai.':
      !reply.foreground?'Open Mcp Agent Ai in the foreground before running commands.':
      reply.running?'Another Termux command is in progress.':
      'Termux is detected and Android RUN_COMMAND permission is granted. A separate on-device approval and a real Termux exit result are still required.';
    if(!reply.allowExternalAppsVerified)
      status.textContent+=' Termux allow-external-apps is not independently verified; it may block the command.';
  }catch(error){
    termuxRunStatus=null;
    status.textContent='Cannot verify the Termux command bridge: '+String(error).slice(0,160);
  }finally{refreshTermuxRunButton();}
}
syncTermuxActionOptions();
$('androidTermuxCommandTool').addEventListener('change',syncTermuxActionOptions);
$('androidTermuxCommandAction').addEventListener('change',syncTermuxActionOptions);
$('androidTermuxCommands').addEventListener('toggle',()=>{
  if(($('androidTermuxCommands') as HTMLDetailsElement).open)void refreshTermuxRunStatus();
});
$('androidTermuxCommandForm').addEventListener('submit',async event=>{
  event.preventDefault();
  if(termuxRunBusy||!nativeAndroidRuntime||!isTauri())return;
  const tool=($('androidTermuxCommandTool') as HTMLSelectElement).value;
  const action=($('androidTermuxCommandAction') as HTMLSelectElement).value;
  const project=($('androidTermuxProject') as HTMLInputElement).value.trim();
  const entry=($('androidTermuxEntry') as HTMLInputElement).value.trim();
  const host=($('androidTermuxSshHost') as HTMLInputElement).value.trim();
  const username=($('androidTermuxSshUser') as HTMLInputElement).value.trim();
  const port=Number(($('androidTermuxSshPort') as HTMLInputElement).value);
  if(!(termuxActions[tool]||[]).some(([value])=>value===action))return;
  let params:Record<string,unknown>={};
  if(tool==='node'&&['check','run'].includes(action)){
    if(!/^[a-z][a-z0-9_-]{0,39}$/.test(project)||
        !/^[A-Za-z0-9][A-Za-z0-9_-]{0,62}\.(js|mjs|cjs)$/.test(entry)){
      $('androidTermuxCommandStatus').textContent='Node needs a Termux project slug and single JavaScript filename.';return;
    }
    params={project,entry};
  }
  if(tool==='npm'&&['list','install'].includes(action)){
    if(!/^[a-z][a-z0-9_-]{0,39}$/.test(project)){
      $('androidTermuxCommandStatus').textContent='npm needs a simple Termux project folder name.';return;
    }
    params={project};
  }
  if(tool==='ssh'&&action==='test'){
    if(!/^[a-z_][a-z0-9_-]{0,31}$/.test(username)||
        !/^[A-Za-z0-9][A-Za-z0-9.-]{0,251}$/.test(host)||host.includes('..')||
        !host.split('.').every(part=>part.length>0&&part.length<=63&&
          /^[A-Za-z0-9]/.test(part)&&/[A-Za-z0-9]$/.test(part))||
        !Number.isInteger(port)||port<1||port>65535){
      $('androidTermuxCommandStatus').textContent='SSH needs a valid user, host and port; strict known_hosts checking is mandatory.';return;
    }
    params={host,username,port};
  }
  const description=`${tool} ${action}`+
    (params.project?` on Termux project ${params.project}`:'')+
    (params.host?` on ${params.host}:${params.port}`:'');
  if(!window.confirm(`Request permission to run ${description}? This executes in the separate Termux app, not inside Mcp Agent Ai. Node and npm scripts may modify Termux files. Every command also requires Android approval.`))return;
  const requestId=crypto.randomUUID();
  termuxRunBusy=true;refreshTermuxRunButton();
  $('androidTermuxCommandResult').textContent='';
  $('androidTermuxCommandStatus').textContent='Waiting for a real Android Termux result…';
  let finalStatus='';
  try{
    const reply=await invoke<{requestId:string;tool:string;action:string;
      processStarted:boolean;completed:boolean;resultVerified:boolean;success:boolean;
      exitCode:number;stdout:string;stderr:string;timedOut:boolean;
      termuxExecutionContext:string;termuxWorkspaceShared:boolean}>(
      'android_termux_command_run',{requestId,tool,action,...params,userInitiated:true});
    if(reply.requestId!==requestId||reply.tool!==tool||reply.action!==action||
        reply.processStarted!==true||reply.completed!==true||reply.resultVerified!==true||
        !Number.isInteger(reply.exitCode)||reply.success!==(reply.exitCode===0)||
        reply.timedOut!==false||reply.termuxExecutionContext!=='external-termux-app'||
        reply.termuxWorkspaceShared!==false||typeof reply.stdout!=='string'||
        typeof reply.stderr!=='string')throw Error('Termux did not return a verified process exit result');
    finalStatus=`Termux process finished with real exit code ${reply.exitCode}; ${reply.success?'success':'nonzero failure'}.`;
    $('androidTermuxCommandResult').textContent=(reply.stdout+'\n'+reply.stderr).slice(0,16384);
  }catch(error){
    finalStatus='No verified Termux execution: '+String(error).slice(0,190);
  }finally{
    termuxRunBusy=false;
    await refreshTermuxRunStatus();
    // The next setup-status probe must not erase the receipt (or failure) of
    // the action the user just approved. Its button-state change still applies.
    if(finalStatus)$('androidTermuxCommandStatus').textContent=finalStatus;
  }
});
let termuxChecking = false;
async function refreshAndroidTermux() {
  if (!nativeAndroidRuntime) return;
  const status = $('androidTermuxStatus');
  $('androidTermuxInstall').classList.add('hidden');
  $('androidTermuxOpen').classList.add('hidden');
  if (!isTauri()) {
    status.textContent = 'Preview only. Termux install and launch require the installed Android app.';
    $('androidTermuxRefresh').disabled = true;
    return;
  }
  if (termuxChecking) return;
  termuxChecking = true;
  $('androidTermuxRefresh').disabled = true;
  try {
    const receipt = await invoke<TermuxStatus>('android_termux_status');
    if (receipt.mode !== 'external-termux-app' || receipt.inAppShellAvailable !== false ||
        receipt.commandsAvailableInApp !== false || receipt.installIsUserManaged !== true)
      throw Error('Native Termux status receipt was not recognized');
    $('androidTermuxInstall').classList.toggle('hidden', receipt.installed);
    $('androidTermuxOpen').classList.toggle('hidden',!receipt.installed || !receipt.launchable);
    status.textContent = receipt.installed
      ? receipt.launchable
        ? 'Termux app detected. Open it to run a real interactive shell. Mcp Agent Ai has no in-app shell connection or verified Termux setup.'
        : 'Termux installed but its terminal activity is unavailable. Check installation and press Check setup.'
      : 'Termux not installed. Choose Install Termux to open its official F-Droid page, then install it yourself and return to Check setup.';
  } catch (error) { status.textContent = `Termux setup could not be verified: ${String(error).slice(0,180)}`; }
  finally { termuxChecking = false; $('androidTermuxRefresh').disabled = false; }
}
$('androidTermuxRefresh').addEventListener('click', () => {
  void refreshAndroidTermux();
  if(($('androidTermuxCommands') as HTMLDetailsElement).open)
    void refreshTermuxRunStatus();
});
$('androidTermuxInstall').addEventListener('click', async () => {
  if (!nativeAndroidRuntime || !isTauri() ||
      !window.confirm('Open the official F-Droid Termux installation page in Android? You decide whether to download and install; this app will not install anything automatically.')) return;
  $('androidTermuxInstall').disabled = true;
  try {
    const receipt = await invoke<{opened?:boolean}>('android_termux_install',{userInitiated:true});
    $('androidTermuxStatus').textContent = receipt.opened === true
      ? 'Official Termux page opened. Install Termux manually, then return and press Check setup.'
      : 'Termux installation page was not confirmed. Nothing was installed.';
  } catch (error) { $('androidTermuxStatus').textContent = `Install handoff unavailable: ${String(error).slice(0,150)}`; }
  finally { $('androidTermuxInstall').disabled = false; }
});
$('androidTermuxOpen').addEventListener('click', async () => {
  if (!nativeAndroidRuntime || !isTauri()) return;
  $('androidTermuxOpen').disabled = true;
  try {
    const receipt = await invoke<{opened?:boolean}>('android_termux_open',{userInitiated:true});
    $('androidTermuxStatus').textContent = receipt.opened === true
      ? 'Termux terminal launch requested. Run shell commands in the Termux app; this app did not run any commands.'
      : 'Termux could not be opened. Check the installation.';
  } catch (error) { $('androidTermuxStatus').textContent = `Termux launch failed: ${String(error).slice(0,150)}`; }
  finally { $('androidTermuxOpen').disabled = false; }
});
async function refreshNativeTerminalStatus(userOpenedTerminal = false) {
  if (nativeAndroidRuntime) {
    ensureAndroidPtyWorkspace(userOpenedTerminal);
    void refreshAndroidTermux();
    return;
  }
  $('terminalNativeModeRow').classList.toggle('hidden', !nativeAndroidRuntime || !isTauri());
  if (!nativeAndroidRuntime || !isTauri()) return;
  try {
    const status = await invoke<{supported:boolean;running:boolean;scope:string}>('android_terminal_status');
    $('terminalNativeStatus').textContent = status.supported
      ? `${status.scope} · ${status.running ? 'a command is running' : 'ready for explicit command'}`
      : 'Android isolated terminal is unavailable.';
    $('terminalNativeMode').disabled = !status.supported;
  } catch (error) {
    $('terminalNativeMode').disabled = true;
    $('terminalNativeStatus').textContent = `Android terminal unavailable: ${error.message}`;
  }
}
$('terminalNativeMode').addEventListener('change', () => {
  if (!$('terminalNativeMode').checked && activeNativeTerminalId) {
    $('terminalNativeMode').checked = true;
    return;
  }
  terminalLine('', $('terminalNativeMode').checked
    ? 'Isolated Android terminal selected. Command allowlist: pwd, ls, stat name.txt, cat name.txt. Requires confirmation each time.'
    : 'App workspace read-only terminal selected.');
});
$('terminalNativeCancel').addEventListener('click', async () => {
  const id = activeNativeTerminalId;
  if (!id) return;
  $('terminalNativeCancel').disabled = true;
  try {
    await invoke('android_terminal_cancel', {requestId:id,userInitiated:true});
    terminalLine('', 'Cancellation requested for matching Android command.');
  } catch (error) { terminalLine('', String(error.message), true); }
  finally { $('terminalNativeCancel').disabled = false; }
});
async function listFiles(path = '') {
  filesCurrentPath = path;
  $('filesPath').textContent = `app://workspace/${path}`;
  $('filesSearchStatus').textContent = '';
  $('filesList').replaceChildren(el('div', 'tool-empty', 'Loading private app files…'));
  $('filesPreview').classList.add('hidden');
  $('filesInfo').classList.add('hidden');
  try {
    const result = await api('/api/workspace', { action: 'list', path });
    if (workspacePanel !== 'files' || filesCurrentPath !== path) return;
    const view = $('filesList');
    view.replaceChildren();
    if (!result.entries?.length) {
      view.append(el('div', 'tool-empty', 'This folder is empty.'));
      return;
    }
    for (const item of result.entries) {
      const row = el('button');
      row.type = 'button';
      const copy = el('span');
      copy.append(el('strong', '', item.name), el('small', '',
        item.directory ? 'Folder' : `${item.bytes} bytes · Read-only`));
      row.append(uiIcon(item.directory ? 'folder' : 'file'), copy, uiIcon('chevron'));
      row.addEventListener('click', async () => {
        const next = path ? `${path}/${item.name}` : item.name;
        if (item.directory) { await listFiles(next); return; }
        await previewPrivateFile(next);
      });
      row.addEventListener('contextmenu', event => {
        event.preventDefault();
        const next = path ? `${path}/${item.name}` : item.name;
        void showPrivateFileProperties(next);
      });
      const info = el('button', 'files-row-info', 'Info');
      info.type = 'button';
      info.setAttribute('aria-label', `Properties for ${item.name}`);
      info.addEventListener('click', event => {
        event.stopPropagation();
        void showPrivateFileProperties(path ? `${path}/${item.name}` : item.name);
      });
      const wrapper = el('div', 'files-row');
      wrapper.append(row, info);
      view.append(wrapper);
    }
  } catch (error) {
    if (workspacePanel === 'files') $('filesList').replaceChildren(el('div', 'tool-empty', String(error.message)));
  }
}
let selectedFileInfoPath = '';
let selectedDocumentTreeUri = '';
let selectedDocumentPath: string[] = [];
let documentTreeEpoch = 0;
let selectedDocumentWriteGranted = false;
let selectedFolderCreationBusy = false;
let selectedDocumentRenameBusy = false;
let selectedDocumentEntries: Array<{name:string;isDirectory:boolean;mimeType:string}> = [];
let selectedDocumentPreviewTicket = 0;
function clearSelectedImagePreview() {
  // Invalidate any earlier asynchronous document read or image decode. A
  // response from an old selection must never replace a newer user's choice.
  ++selectedDocumentPreviewTicket;
  $('filesExternalImage').removeAttribute('src');
  $('filesExternalImagePreview').classList.add('hidden');
}
function switchFilesSource(selected: boolean) {
  clearSelectedImagePreview();
  $('filesSourcePrivate').setAttribute('aria-pressed', String(!selected));
  $('filesSourceSelected').setAttribute('aria-pressed', String(selected));
  $('filesPrivateView').classList.toggle('hidden', selected);
  $('filesExternalView').classList.toggle('hidden', !selected);
  const canWrite = nativeAndroidRuntime && isTauri();
  $('filesPrivateEditToggle').classList.toggle('hidden', !canWrite);
  $('filesContentSearchForm').classList.toggle('hidden', !canWrite);
  $('filesChooseWritableFolder').classList.toggle('hidden', !canWrite);
  if (selected) void refreshSelectedDocuments();
  else void listFiles(filesCurrentPath);
}
$('filesPrivateEditToggle').addEventListener('click', () => {
  if (!nativeAndroidRuntime || !isTauri()) return;
  const form = $('filesPrivateWriteForm');
  form.classList.toggle('hidden');
  $('filesPrivateEditToggle').setAttribute('aria-expanded', String(!form.classList.contains('hidden')));
});
async function writePrivateText(action: 'create_text' | 'replace_text') {
  if (!nativeAndroidRuntime || !isTauri()) return;
  const name = String($('filesPrivateWriteName').value).trim();
  const content = String($('filesPrivateWriteContent').value);
  if (!name || name.length > 128 || name.startsWith('.') || /[\\/\x00-\x1f]/.test(name) || name === '..') {
    $('filesPrivateWriteStatus').textContent = 'Use a valid plain filename (no folders or hidden files).';
    return;
  }
  if (new TextEncoder().encode(content).length > 65536) {
    $('filesPrivateWriteStatus').textContent = 'Text exceeds 64 KiB UTF-8 limit.';
    return;
  }
  const path = `user-files/${name}`;
  if (!window.confirm(`${action === 'create_text' ? 'Create' : 'Replace'} app-private ${path}? This is an explicit write inside user-files/ only.`)) return;
  $('filesPrivateCreate').disabled = true;
  $('filesPrivateReplace').disabled = true;
  $('filesPrivateWriteStatus').textContent = 'Writing approved app-private text…';
  try {
    const result = await api('/api/workspace', {action, path, content, allowWrite: true});
    if (result?.written !== true || result.path !== path)
      throw Error('Private file write did not return a matching success receipt');
    $('filesPrivateWriteStatus').textContent = `Saved ${path} in private user-files/.`;
    if (workspacePanel === 'files') await listFiles('user-files');
  } catch (error) {
    $('filesPrivateWriteStatus').textContent = `No write confirmed: ${error.message}`;
  } finally {
    $('filesPrivateCreate').disabled = false;
    $('filesPrivateReplace').disabled = false;
  }
}
$('filesPrivateCreate').addEventListener('click', () => void writePrivateText('create_text'));
$('filesPrivateReplace').addEventListener('click', () => void writePrivateText('replace_text'));
async function refreshSelectedDocuments() {
  if (workspacePanel !== 'files') return;
  if (!nativeAndroidRuntime || !isTauri()) {
    selectedDocumentWriteGranted = false;
    $('filesExternalPropertiesForm').classList.add('hidden');
    $('filesExternalCreateFolderForm').classList.add('hidden');
    $('filesExternalRenameForm').classList.add('hidden');
    $('filesChooseFolder').disabled = true;
    $('filesExternalStatus').textContent = 'Android system picker unavailable in this preview.';
    $('filesExternalList').replaceChildren();
    return;
  }
  try {
    const status = await invoke<{ granted: boolean; treeUri: string; pickerOpen?: boolean;
      writeGranted?: boolean }>('document_tree_status');
    // The Setup Folder receipt is derived independently from the same native
    // SAF status, never from a local checkbox or picker-open event.
    void refreshAndroidCosFolderGrant();
    selectedDocumentTreeUri = status.granted ? status.treeUri : '';
    selectedDocumentWriteGranted = !!selectedDocumentTreeUri && status.writeGranted === true;
    $('filesChooseFolder').disabled = status.pickerOpen === true;
    $('filesChooseWritableFolder').disabled = !selectedDocumentTreeUri || status.pickerOpen === true;
    $('filesExternalCreateForm').classList.toggle('hidden', !selectedDocumentWriteGranted);
    $('filesExternalCreateFolderForm').classList.toggle('hidden', !selectedDocumentWriteGranted);
    $('filesExternalRenameForm').classList.toggle('hidden', !selectedDocumentWriteGranted);
    $('filesExternalPropertiesForm').classList.toggle('hidden', !selectedDocumentTreeUri);
    $('filesExternalStatus').textContent = selectedDocumentTreeUri
      ? status.writeGranted
        ? 'Selected folder has verified read + write grants. Creating text still requires confirmation.'
        : 'Selected folder has a read-only grant. Request separate write access to create text.'
      : 'No selected folder. Choose one to enable browsing.';
    if (selectedDocumentTreeUri) await listSelectedDocuments([]);
    else $('filesExternalList').replaceChildren(el('div', 'tool-empty', 'No Android folder selected.'));
  } catch (error) {
    selectedDocumentTreeUri = '';
    selectedDocumentWriteGranted = false;
    selectedDocumentEntries = [];
    $('filesExternalPropertiesForm').classList.add('hidden');
    $('filesExternalCreateForm').classList.add('hidden');
    $('filesExternalCreateFolderForm').classList.add('hidden');
    $('filesExternalRenameForm').classList.add('hidden');
    $('filesExternalStatus').textContent = 'Cannot verify Android document-tree permission.';
    $('filesExternalList').replaceChildren(el('div', 'tool-empty', String(error.message)));
  }
}
async function listSelectedDocuments(path: string[]) {
  if (!selectedDocumentTreeUri || workspacePanel !== 'files') return;
  clearSelectedImagePreview();
  selectedDocumentEntries = [];
  $('filesExternalRenameEntry').replaceChildren();
  $('filesExternalPropertiesEntry').replaceChildren();
  $('filesExternalPropertiesResult').textContent = 'No properties inspected.';
  selectedDocumentPath = [...path];
  const epoch = ++documentTreeEpoch;
  $('filesExternalPath').textContent = `Selected folder / ${path.join(' / ')}`;
  $('filesExternalPreview').classList.add('hidden');
  $('filesExternalList').replaceChildren(el('div', 'tool-empty', 'Loading selected folder…'));
  try {
    const result = await invoke<{ entries: Array<{
      name: string; isDirectory: boolean; size?: number | null; mimeType: string;
    }> }>('document_tree_list', { treeUri: selectedDocumentTreeUri, path });
    if (epoch !== documentTreeEpoch || workspacePanel !== 'files') return;
    const view = $('filesExternalList');
    view.replaceChildren();
    selectedDocumentEntries = result.entries.filter((item: any) =>
      item && typeof item.name === 'string' && !!item.name &&
      typeof item.mimeType === 'string' && typeof item.isDirectory === 'boolean').slice(0,128);
    const renameSelect = $('filesExternalRenameEntry');
    renameSelect.replaceChildren();
    const propertiesSelect = $('filesExternalPropertiesEntry');
    propertiesSelect.replaceChildren();
    for (const item of selectedDocumentEntries) {
      const option = document.createElement('option');
      option.value = item.name;
      option.textContent = `${item.name} · ${item.isDirectory ? 'folder' : 'file'}`;
      renameSelect.append(option);
      propertiesSelect.append(option.cloneNode(true));
    }
    $('filesExternalPropertiesForm').classList.toggle('hidden', !selectedDocumentTreeUri ||
      !selectedDocumentEntries.length);
    $('filesExternalRenameForm').querySelector('button[type="submit"]').disabled =
      !selectedDocumentWriteGranted || selectedDocumentRenameBusy || !selectedDocumentEntries.length;
    if (!result.entries.length) view.append(el('div', 'tool-empty', 'Selected folder is empty.'));
    for (const item of result.entries) {
      const row = el('button');
      row.type = 'button';
      const copy = el('span');
      copy.append(el('strong', '', item.name),
        el('small', '', item.isDirectory ? 'Folder' :
          `${item.size == null ? 'Unknown size' : `${item.size} bytes`} · Read-only`));
      row.append(uiIcon(item.isDirectory ? 'folder' : 'file'), copy, uiIcon('chevron'));
      row.addEventListener('click', () => {
        const next = [...path, item.name];
        if (item.isDirectory) void listSelectedDocuments(next);
        else void openSelectedDocument(next, item.mimeType);
      });
      view.append(row);
    }
  } catch (error) {
    if (epoch === documentTreeEpoch) $('filesExternalList').replaceChildren(
      el('div', 'tool-empty', String(error.message)));
  }
}
async function openSelectedDocument(path: string[], mime: string) {
  if (workspacePanel !== 'files' || !selectedDocumentTreeUri) return;
  clearSelectedImagePreview();
  const previewTicket = selectedDocumentPreviewTicket;
  const file = path[path.length-1] || '';
  const imageType = mime === 'image/png' || mime === 'image/jpeg';
  const text = mime.startsWith('text/') ||
    /\.(txt|md|markdown|json|csv|log|xml|yaml|yml|js|ts|rs|kt|css|html)$/i.test(file);
  $('filesExternalPreview').classList.remove('hidden');
  if (!text && !imageType) {
    $('filesExternalPreview').textContent =
      'Only text, PNG and JPEG previews are supported after selecting a file. Other binary files are not opened.';
    return;
  }
  const epoch = documentTreeEpoch;
  const treeUri = selectedDocumentTreeUri;
  $('filesExternalPreview').textContent = `Reading selected ${imageType ? 'image' : 'text'} document…`;
  try {
    const result = await invoke<{treeUri:string;path:string[];mimeType:string;
      data:string;encoding:string;size:number}>(
      'document_tree_open', {treeUri,path});
    if (epoch !== documentTreeEpoch || previewTicket !== selectedDocumentPreviewTicket ||
        workspacePanel !== 'files' ||
        selectedDocumentTreeUri !== treeUri) return;
    if (result?.treeUri !== treeUri || !Array.isArray(result.path) ||
        result.path.length !== path.length || result.path.some((name,index) => name !== path[index]) ||
        result.mimeType !== mime || result.encoding !== 'base64' ||
        !Number.isSafeInteger(result.size) || result.size < 0 || result.size > 1048576 ||
        typeof result.data !== 'string' || result.data.length > 1398104 ||
        result.data.length % 4 !== 0 ||
        !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(result.data))
      throw Error('Invalid or mismatched Android selected-document receipt');
    const binary = atob(result.data);
    if (binary.length !== result.size) throw Error('Selected-document byte count differs from provider receipt');
    const bytes = Uint8Array.from(binary, c => c.charCodeAt(0));
    if (imageType) {
      const png = mime === 'image/png' && bytes.length >= 24 &&
        [137,80,78,71,13,10,26,10].every((v,i) => bytes[i] === v) &&
        [73,72,68,82].every((v,i) => bytes[12+i] === v);
      const jpeg = mime === 'image/jpeg' && bytes.length >= 4 &&
        bytes[0] === 255 && bytes[1] === 216 && bytes[2] === 255 &&
        bytes[bytes.length-2] === 255 && bytes[bytes.length-1] === 217;
      if (!png && !jpeg) throw Error('Selected file is not a verified PNG/JPEG image');
      const preview = $('filesExternalImage') as HTMLImageElement;
      preview.alt = `Preview of ${file.slice(0,120)}`;
      preview.src = `data:${mime};base64,${result.data}`;
      // A correct magic header is not sufficient to prove decodable pixels.
      // Require an actual WebView image decode before claiming a preview.
      try { await preview.decode(); }
      catch { throw Error('Selected image could not be decoded by Android WebView'); }
      if (previewTicket !== selectedDocumentPreviewTicket || epoch !== documentTreeEpoch || selectedDocumentTreeUri !== treeUri ||
          workspacePanel !== 'files') {
        return;
      }
      $('filesExternalImagePreview').classList.remove('hidden');
      $('filesExternalPreview').classList.add('hidden');
      return;
    }
    const content = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    $('filesExternalPreview').textContent = content.slice(0, 50000) ||
      '(empty text file)';
  } catch (error) {
    if (previewTicket !== selectedDocumentPreviewTicket || epoch !== documentTreeEpoch ||
        selectedDocumentTreeUri !== treeUri || workspacePanel !== 'files') return;
    clearSelectedImagePreview();
    $('filesExternalPreview').textContent = String(error.message);
  }
}
// Provider properties require only the separately verified selected-tree READ
// grant, not WRITE or all-files access. Flags describe the provider's claimed
// capabilities; they never prove this app has permission to mutate the item.
$('filesExternalPropertiesForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  if (!nativeAndroidRuntime || !isTauri() || workspacePanel !== 'files' ||
      !selectedDocumentTreeUri || !selectedDocumentEntries.length) return;
  const name = String($('filesExternalPropertiesEntry').value);
  const selected = selectedDocumentEntries.find(entry => entry.name === name);
  if (!selected || !name || name.length > 255) return;
  const treeUri = selectedDocumentTreeUri;
  const path = [...selectedDocumentPath, name];
  const epoch = documentTreeEpoch;
  const form = $('filesExternalPropertiesForm');
  const button = form.querySelector('button[type="submit"]') as HTMLButtonElement;
  const output = $('filesExternalPropertiesResult');
  button.disabled = true;
  output.textContent = 'Reading selected provider metadata only…';
  try {
    const data = await invoke<{
      treeUri: string; path: string[]; name: string; isDirectory: boolean;
      mimeType: string; size?: number; lastModified?: number;
      canWrite: boolean; canRename: boolean; canDelete: boolean;
    }>('document_tree_properties', {treeUri,path,userInitiated:true});
    if (epoch !== documentTreeEpoch || treeUri !== selectedDocumentTreeUri ||
        workspacePanel !== 'files') return;
    if (!data || data.treeUri !== treeUri || !Array.isArray(data.path) ||
        data.path.length !== path.length || data.path.some((value,index) => value !== path[index]) ||
        data.name !== name || data.isDirectory !== selected.isDirectory ||
        typeof data.mimeType !== 'string' || !data.mimeType || data.mimeType.length > 256 ||
        typeof data.canWrite !== 'boolean' || typeof data.canRename !== 'boolean' ||
        typeof data.canDelete !== 'boolean' ||
        (data.size !== undefined && (!Number.isSafeInteger(data.size) || data.size < 0)) ||
        (data.lastModified !== undefined &&
          (!Number.isSafeInteger(data.lastModified) || data.lastModified <= 0)))
      throw Error('Android returned invalid or mismatched file metadata');
    const lines = [
      `Name: ${data.name}`, `Type: ${data.isDirectory ? 'Folder' : 'File'}`,
      `MIME: ${data.mimeType}`,
      `Size: ${data.size === undefined ? 'Unknown' : `${data.size} bytes`}`,
      `Last modified: ${data.lastModified === undefined ? 'Unknown' :
        new Date(data.lastModified).toISOString()}`,
      `Provider supports write: ${data.canWrite ? 'Yes' : 'No'}`,
      `Provider supports rename: ${data.canRename ? 'Yes' : 'No'}`,
      `Provider supports delete: ${data.canDelete ? 'Yes' : 'No'}`,
      'Provider capability flags do not grant app permissions.',
    ];
    output.textContent = lines.join('\n');
  } catch (error) {
    if (epoch === documentTreeEpoch && workspacePanel === 'files')
      output.textContent = `Properties not confirmed: ${String((error as Error).message).slice(0,200)}`;
  } finally {
    button.disabled = false;
  }
});
$('filesSourcePrivate').addEventListener('click', () => switchFilesSource(false));
$('filesSourceSelected').addEventListener('click', () => switchFilesSource(true));
$('filesChooseFolder').addEventListener('click', async () => {
  if (!nativeAndroidRuntime || !isTauri()) return;
  $('filesChooseFolder').disabled = true;
  $('filesExternalStatus').textContent = 'Choose a folder in Android system picker…';
  try {
    const result = await invoke<{selected?:boolean}>('document_tree_request');
    if (result.selected) {
      documentTreeEpoch++;
      selectedDocumentPath = [];
      selectedDocumentWriteGranted = false;
      $('filesExternalCreateFolderForm').classList.add('hidden');
      $('filesExternalRenameForm').classList.add('hidden');
      await refreshSelectedDocuments();
    } else $('filesExternalStatus').textContent = 'Folder selection cancelled; no changes made.';
  } catch (error) {
    $('filesExternalStatus').textContent = String(error.message);
  } finally { $('filesChooseFolder').disabled = false; }
  void refreshAndroidCosFolderGrant();
});
$('filesChooseWritableFolder').addEventListener('click', async () => {
  if (!nativeAndroidRuntime || !isTauri() || !selectedDocumentTreeUri) return;
  if (!window.confirm('Open Android system picker to explicitly grant write access to the selected folder? No file will be modified by the picker.')) return;
  $('filesChooseWritableFolder').disabled = true;
  $('filesExternalStatus').textContent = 'Choose the same folder in Android picker to grant write permission…';
  try {
    const result = await invoke<{selected?:boolean}>('document_tree_request_write', {userInitiated: true});
    if (result.selected) await refreshSelectedDocuments();
    else $('filesExternalStatus').textContent = 'Write permission not granted; existing read access retained.';
  } catch (error) {
    $('filesExternalStatus').textContent = `Write access not granted: ${error.message}`;
  } finally { $('filesChooseWritableFolder').disabled = false; }
});
$('filesExternalCreateForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  if (!nativeAndroidRuntime || !isTauri() || !selectedDocumentTreeUri) return;
  const name = String($('filesExternalCreateName').value).trim();
  const content = String($('filesExternalCreateContent').value);
  if (!name || name.length > 128 || name.startsWith('.') || !/\.txt$/i.test(name) || /[\\/\x00-\x1f]/.test(name) || name === '..') {
    $('filesExternalCreateStatus').textContent = 'Invalid filename; no file created.';
    return;
  }
  if (new TextEncoder().encode(content).length > 65536) {
    $('filesExternalCreateStatus').textContent = 'Text exceeds 64 KiB UTF-8 limit.';
    return;
  }
  const path = [...selectedDocumentPath];
  const treeUri = selectedDocumentTreeUri;
  if (!window.confirm(`Create new text file ${name} inside the explicitly selected Android folder? Existing files cannot be overwritten.`)) return;
  const submit = ($('filesExternalCreateForm') as HTMLFormElement)
    .querySelector<HTMLButtonElement>('button[type="submit"]')!;
  submit.disabled = true;
  $('filesExternalCreateStatus').textContent = 'Creating user-approved text file…';
  try {
    const result = await invoke<{created: boolean; name: string; treeUri: string}>(
      'document_tree_create_text', {treeUri, path, name, content, userInitiated: true});
    if (result?.created !== true || result.name !== name || result.treeUri !== treeUri)
      throw Error('SAF provider did not return a matching creation receipt');
    $('filesExternalCreateStatus').textContent = `Created ${name} inside the selected folder.`;
    if (workspacePanel === 'files' && selectedDocumentTreeUri === treeUri) await listSelectedDocuments(path);
  } catch (error) {
    $('filesExternalCreateStatus').textContent = `No file creation confirmed: ${error.message}`;
  } finally { submit.disabled = false; }
});
$('filesExternalCreateFolderForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  if (selectedFolderCreationBusy || !nativeAndroidRuntime || !isTauri() ||
      !selectedDocumentTreeUri || !selectedDocumentWriteGranted || workspacePanel !== 'files') return;
  const name = String($('filesExternalCreateFolderName').value);
  const feedback = $('filesExternalCreateFolderStatus');
  if (!name || name.length > 128 || name !== name.trim() ||
      name !== name.normalize('NFC') || name.startsWith('.') || name.endsWith('.') ||
      /[\\/<>:"|?*\x00-\x1f\x7f\u061c\u200b-\u200f\u202a-\u202e\u2060-\u206f\ufeff]/u.test(name) ||
      /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/u.test(name) ||
      selectedDocumentPath.length >= 16) {
    feedback.textContent = 'Unsafe folder name or path. No folder was created.';
    return;
  }
  const treeUri = selectedDocumentTreeUri;
  const path = [...selectedDocumentPath];
  if (!window.confirm(`Create NEW folder "${name}" inside the selected Android folder? Existing folders and files must not be overwritten. Android will ask again.`)) return;
  const submit = $('filesExternalCreateFolderForm').querySelector('button[type="submit"]');
  selectedFolderCreationBusy = true;
  submit.disabled = true;
  feedback.textContent = 'Requesting native confirmation for one new folder…';
  try {
    const receipt = await invoke<{created:boolean;cancelled?:boolean;treeUri:string;
      name:string;path:string[];parentPath:string[];isDirectory:boolean;mimeType:string}>(
      'document_tree_create_folder', {treeUri,path,name,userInitiated:true});
    if (receipt?.created === false && receipt?.cancelled === true) {
      feedback.textContent = 'Folder creation cancelled in Android. No creation confirmed.';
      return;
    }
    const expectedPath = [...path,name];
    if (receipt?.created !== true || receipt.treeUri !== treeUri ||
        receipt.name !== name || receipt.isDirectory !== true ||
        receipt.mimeType !== 'vnd.android.document/directory' ||
        !Array.isArray(receipt.path) || receipt.path.length !== expectedPath.length ||
        receipt.path.some((part,index) => part !== expectedPath[index]) ||
        !Array.isArray(receipt.parentPath) || receipt.parentPath.length !== path.length ||
        receipt.parentPath.some((part,index) => part !== path[index]))
      throw Error('SAF provider did not return matching create-only folder evidence');
    feedback.textContent = `Created folder ${name} inside selected Android folder.`;
    if (workspacePanel === 'files' && selectedDocumentTreeUri === treeUri &&
        selectedDocumentPath.join('/') === path.join('/')) await listSelectedDocuments(path);
  } catch (error) {
    feedback.textContent = `No folder creation confirmed: ${String(error.message).slice(0,180)}`;
  } finally {
    selectedFolderCreationBusy = false;
    submit.disabled = !selectedDocumentWriteGranted;
  }
});
function validAndroidRenameName(name: string): boolean {
  return !!name && name.length <= 128 && name === name.trim() &&
    name === name.normalize('NFC') && !name.startsWith('.') && !name.endsWith('.') &&
    !/[\\/<>:"|?*\x00-\x1f\x7f\u061c\u200b-\u200f\u202a-\u202e\u2060-\u206f\ufeff]/u.test(name) &&
    !/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/u.test(name);
}
$('filesExternalRenameForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  if (!nativeAndroidRuntime || !isTauri() || selectedDocumentRenameBusy ||
      !selectedDocumentWriteGranted || !selectedDocumentTreeUri || workspacePanel !== 'files') return;
  const oldName = String($('filesExternalRenameEntry').value);
  const name = String($('filesExternalRenameName').value);
  const feedback = $('filesExternalRenameStatus');
  if (!validAndroidRenameName(name) || !selectedDocumentEntries.some(item => item.name === oldName) ||
      oldName === name || selectedDocumentEntries.some(item => item.name === name)) {
    feedback.textContent = 'Unsafe, unchanged or colliding rename; no request sent.';
    return;
  }
  const treeUri = selectedDocumentTreeUri;
  const parentPath = [...selectedDocumentPath];
  const oldPath = [...parentPath,oldName];
  if (!window.confirm(`Rename EXISTING selected Android item "${oldName}" to "${name}" in the SAME folder? Existing siblings must not be overwritten. Android will ask again; no deletion is performed.`)) return;
  selectedDocumentRenameBusy = true;
  const submit = $('filesExternalRenameForm').querySelector('button[type="submit"]');
  submit.disabled = true;
  feedback.textContent = 'Waiting for native confirmation of this one rename…';
  try {
    const receipt = await invoke<{renamed:boolean;verified?:boolean;cancelled?:boolean;
      uncertain?:boolean;providerAttempted?:boolean;treeUri?:string;oldName?:string;
      name?:string;oldPath?:string[];newPath?:string[];parentPath?:string[];
      isDirectory?:boolean;documentIdChanged?:boolean;mimeType?:string}>(
      'document_tree_rename_entry', {treeUri,path:oldPath,name,userInitiated:true});
    if (receipt?.renamed === false && receipt.cancelled === true && receipt.uncertain === false) {
      feedback.textContent = 'Native rename cancelled. No change confirmed.';
      return;
    }
    if (receipt?.renamed === false && receipt.uncertain === true) {
      feedback.textContent = 'Rename outcome uncertain: the document provider MAY have changed this item. Refresh the selected folder manually; do not retry blindly.';
      return;
    }
    const equalPath = (actual: unknown, expected: string[]) =>
      Array.isArray(actual) && actual.length === expected.length &&
      actual.every((part,index) => part === expected[index]);
    const previous = selectedDocumentEntries.find(item => item.name === oldName);
    if (receipt?.renamed !== true || receipt.verified !== true || receipt.uncertain !== false ||
        receipt.treeUri !== treeUri || receipt.oldName !== oldName || receipt.name !== name ||
        !equalPath(receipt.oldPath, oldPath) ||
        !equalPath(receipt.newPath, [...parentPath,name]) ||
        !equalPath(receipt.parentPath, parentPath) ||
        receipt.isDirectory !== previous?.isDirectory ||
        receipt.mimeType !== previous?.mimeType ||
        typeof receipt.documentIdChanged !== 'boolean')
      throw Error('Native rename receipt did not verify the same selected item and parent');
    feedback.textContent = `Verified rename: ${oldName} → ${name}. No other item renamed.`;
    if (workspacePanel === 'files' && selectedDocumentTreeUri === treeUri &&
        selectedDocumentPath.join('/') === parentPath.join('/')) {
      await listSelectedDocuments(parentPath);
    }
  } catch (error) {
    feedback.textContent = `Rename not verified; provider may have changed this item: ${mobileAiDelegatePreview(String(error.message), 180)}. Refresh manually before retrying.`;
  } finally {
    selectedDocumentRenameBusy = false;
    submit.disabled = !selectedDocumentWriteGranted || !selectedDocumentEntries.length;
  }
});
$('filesExternalUp').addEventListener('click', () =>
  void listSelectedDocuments(selectedDocumentPath.slice(0,-1)));
$('filesExternalReload').addEventListener('click', () =>
  void listSelectedDocuments(selectedDocumentPath));
async function showPrivateFileProperties(path: string) {
  selectedFileInfoPath = path;
  $('filesInfo').classList.remove('hidden');
  $('filesInfoBody').textContent = 'Loading metadata…';
  $('filesCopyFeedback').textContent = '';
  try {
    const result = await api('/api/workspace', { action: 'properties', path });
    if (workspacePanel !== 'files' || selectedFileInfoPath !== path) return;
    const modified = result.modifiedMs ? new Date(result.modifiedMs).toLocaleString() : 'Unknown';
    const values = [
      ['Name', result.name], ['Type', result.directory ? 'Folder' : 'File'],
      ['Size', result.directory ? '—' : `${result.bytes} bytes`],
      ['Modified', modified], ['Path', `app://workspace/${result.path}`],
      ['Access', 'Read-only · private app data'],
    ];
    const table = el('dl');
    for (const [label, value] of values) {
      const row = el('div');
      row.append(el('dt', '', label), el('dd', '', value));
      table.append(row);
    }
    $('filesInfoBody').replaceChildren(table);
  } catch (error) {
    if (workspacePanel === 'files') $('filesInfoBody').textContent = String(error.message);
  }
}
async function previewPrivateFile(path: string) {
  $('filesPreview').textContent = 'Opening private text preview…';
  $('filesPreview').classList.remove('hidden');
  try {
    const content = await api('/api/workspace', { action: 'read', path });
    if (workspacePanel === 'files')
      $('filesPreview').textContent = content.content || '(empty file)';
  } catch (error) { if (workspacePanel === 'files') $('filesPreview').textContent = String(error.message); }
}
type LocalJobSnapshot = {
  id: string; status: string; progress: number; sequence: number;
  operation: { type: string }; result?: unknown; error?: string;
};
function renderLocalWorkerJobs(jobs: LocalJobSnapshot[]) {
  const container = $('localWorkerHistory');
  container.replaceChildren();
  if (!jobs.length) {
    container.append(el('div', 'tool-empty', 'No local worker jobs yet.'));
    return;
  }
  for (const job of jobs) {
    const row = el('div', 'local-worker-job');
    row.append(el('strong', '', `#${job.sequence} · ${job.operation.type}`),
      el('small', '', `${job.status} · ${job.progress}%`));
    if (job.error) row.append(el('small', 'terminal-error', job.error));
    if (job.result !== undefined) {
      const output = el('pre', '', JSON.stringify(job.result, null, 2).slice(0, 3000));
      row.append(output);
    }
    if (job.status === 'queued' || job.status === 'running') {
      const button = el('button', '', 'Cancel');
      button.type = 'button';
      button.addEventListener('click', async () => {
        button.disabled = true;
        try { await api('/api/mobile-agents', { action: 'cancel', id: job.id }); }
        catch (error) { $('localWorkerFeedback').textContent = String(error.message); }
        await refreshLocalWorkers();
      });
      row.append(button);
    }
    container.append(row);
  }
}
async function refreshLocalWorkers() {
  if (workspacePanel !== 'subagents') return;
  if (!nativeAndroidRuntime || !isTauri()) {
    $('localWorkerStats').textContent = 'Android app required';
    $('localWorkerForm').querySelector('button[type="submit"]').disabled = true;
    $('localWorkerFeedback').textContent = 'Background job execution is unavailable in browser previews.';
    renderLocalWorkerJobs([]);
    return;
  }
  try {
    const result = await api('/api/mobile-agents', { action: 'status' });
    if (workspacePanel !== 'subagents') return;
    $('localWorkerStats').textContent =
      `Running ${result.stats.running} · Queued ${result.stats.queued} · History ${result.stats.retained}`;
    renderLocalWorkerJobs(result.history || []);
  } catch (error) { $('localWorkerFeedback').textContent = String(error.message); }
}
type MobileAiDelegateJob = {
  id: string; task: string; provider: string; model: string;
  order?: number; ticket?: string | null;
  status: 'queued' | 'running' | 'completed' | 'failed' | 'cancelled' | 'interrupted';
  answer?: string | null; error?: string | null;
};
let mobileAiDelegateJobs: MobileAiDelegateJob[] = [];
let mobileAiDelegateRunning = 0;
let mobileAiDelegateCapacity = 0;
let mobileAiDelegateStatusEpoch = 0;
const mobileAiDelegatePending = new Map<string, string>();
const mobileAiDelegateCancelling = new Set<string>();
function mobileAiDelegateSelection(): {provider:string;model:string} | null {
  const provider = String($('provider').value || '').trim();
  const model = String($('model').value || '').trim();
  return ['openai','anthropic','openrouter'].includes(provider) &&
    state.providers.includes(provider) && state.hasKeys.includes(provider) &&
    model.length > 0 && model.length <= 120 && !/[\r\n\0]/.test(model)
    ? {provider,model} : null;
}
function mobileAiDelegateTask(): string {
  return String($('mobileAiDelegateTask').value || '').trim();
}
function mobileAiDelegateFingerprint(task: string, provider: string, model: string) {
  return JSON.stringify([task,provider,model]);
}
function updateMobileAiDelegateSubmit() {
  const selected = mobileAiDelegateSelection();
  const task = mobileAiDelegateTask();
  const bytes = new TextEncoder().encode(task).length;
  const duplicate = selected && mobileAiDelegatePending.has(
    mobileAiDelegateFingerprint(task, selected.provider, selected.model));
  const atCapacity = mobileAiDelegateRunning + mobileAiDelegatePending.size >= mobileAiDelegateCapacity;
  $('mobileAiDelegateForm').querySelector('button[type="submit"]').disabled =
    !nativeAndroidRuntime || !isTauri() || mobileAiDelegateCapacity !== 2 ||
    !selected || task.length === 0 || task.length > 2000 || bytes > 3000 ||
    !!duplicate || atCapacity || working;
}
function mobileAiDelegatePreview(value: unknown, max: number): string {
  let text = typeof value === 'string' ? value : '';
  // Provider results can contain credentials even if a task did not request them.
  text = text.replace(/\b(?:sk-(?:proj-|ant-)?[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9_]{16,}|github_pat_[A-Za-z0-9_]{16,})\b/g, '[REDACTED]')
    .replace(/\bBearer\s+[A-Za-z0-9._~+\/-]{8,}/gi, 'Bearer [REDACTED]')
    .replace(/\b(api[_-]?key|access[_-]?token|password)\s*[:=]\s*\S{6,}/gi, '$1: [REDACTED]');
  return text.slice(0, max) + (text.length > max ? '\n[Preview truncated]' : '');
}
function renderMobileAiDelegateJobs() {
  const history = $('mobileAiDelegateJobs');
  history.replaceChildren();
  $('mobileAiDelegateStats').textContent = mobileAiDelegateCapacity === 2
    ? `Native jobs: ${mobileAiDelegateRunning}/2 running · ${mobileAiDelegateJobs.length} saved · ${mobileAiDelegatePending.size} UI request(s) awaiting response`
    : 'Android AI delegate status not verified; sending disabled.';
  if (!mobileAiDelegateJobs.length) {
    history.append(el('div', 'tool-empty', 'No saved AI delegate jobs reported by Android.'));
  }
  for (const job of mobileAiDelegateJobs) {
    const row = el('div', 'local-worker-job');
    row.append(el('strong', '', job.task.slice(0, 140)),
      el('small', '', `${job.provider} / ${job.model.slice(0, 120)} · ${job.status}`));
    if (job.status === 'completed' && typeof job.answer === 'string') {
      row.append(el('pre', '', mobileAiDelegatePreview(job.answer, 1600) || '(Empty provider answer)'));
    } else if (['failed','interrupted'].includes(job.status) && job.error) {
      row.append(el('small', 'terminal-error', mobileAiDelegatePreview(job.error, 240)));
    }
    if (['running','queued'].includes(job.status)) {
      const cancel = el('button', '', 'Cancel delegate');
      cancel.type = 'button';
      cancel.disabled = mobileAiDelegateCancelling.has(job.id);
      cancel.addEventListener('click', async () => {
        if (mobileAiDelegateCancelling.has(job.id) ||
            !window.confirm(`Ask Android to cancel the ${job.provider} delegate? Provider billing or remote work may already have occurred.`)) return;
        mobileAiDelegateCancelling.add(job.id);
        cancel.disabled = true;
        $('mobileAiDelegateFeedback').textContent = 'Requesting cancellation of selected native delegate job…';
        try {
          const reply = await api('/api/mobile-ai-agents', {action:'cancel',id:job.id,userInitiated:true});
          if (!reply || (!reply.job && !Array.isArray(reply.jobs)))
            throw Error('Native cancellation did not return an updated job receipt');
          $('mobileAiDelegateFeedback').textContent = 'Cancellation requested. Check the persisted native status; remote work may already have occurred.';
        } catch (error) {
          $('mobileAiDelegateFeedback').textContent =
            `Cancellation not confirmed: ${mobileAiDelegatePreview(String(error.message), 200)}`;
        } finally {
          mobileAiDelegateCancelling.delete(job.id);
          await refreshMobileAiDelegates();
        }
      });
      row.append(cancel);
    }
    history.append(row);
  }
  updateMobileAiDelegateSubmit();
}
async function refreshMobileAiDelegates() {
  if (workspacePanel !== 'subagents') return;
  if (!nativeAndroidRuntime || !isTauri()) {
    mobileAiDelegateCapacity = 0;
    $('mobileAiDelegateFeedback').textContent =
      'AI delegates require the installed Android app and a configured provider; no provider connection has been checked.';
    renderMobileAiDelegateJobs();
    return;
  }
  const epoch = ++mobileAiDelegateStatusEpoch;
  try {
    const response = await api('/api/mobile-ai-agents', {action:'status'});
    if (epoch !== mobileAiDelegateStatusEpoch || workspacePanel !== 'subagents') return;
    if (!response || response.maxConcurrent !== 2 ||
        !Number.isInteger(response.running) || response.running < 0 || response.running > 2 ||
        !Array.isArray(response.jobs) || response.jobs.length > 64)
      throw Error('Native delegate status failed safety validation');
    const valid = (job: any): job is MobileAiDelegateJob =>
      job && typeof job === 'object' && !Array.isArray(job) &&
      typeof job.id === 'string' && job.id.length > 0 && job.id.length <= 128 &&
      typeof job.task === 'string' && new TextEncoder().encode(job.task).length <= 3000 &&
      typeof job.provider === 'string' && job.provider.length <= 32 &&
      typeof job.model === 'string' && job.model.length <= 120 &&
      ['queued','running','completed','failed','cancelled','interrupted'].includes(job.status);
    if (!response.jobs.every(valid)) throw Error('Native delegate job metadata invalid');
    mobileAiDelegateCapacity = 2;
    mobileAiDelegateRunning = response.running;
    mobileAiDelegateJobs = response.jobs;
    renderMobileAiDelegateJobs();
  } catch (error) {
    if (epoch !== mobileAiDelegateStatusEpoch || workspacePanel !== 'subagents') return;
    mobileAiDelegateCapacity = 0;
    $('mobileAiDelegateFeedback').textContent =
      `Cannot verify native AI delegates: ${mobileAiDelegatePreview(String(error.message), 180)}`;
    renderMobileAiDelegateJobs();
  }
}
$('mobileAiDelegateTask').addEventListener('input', updateMobileAiDelegateSubmit);
$('provider').addEventListener('change', updateMobileAiDelegateSubmit);
$('model').addEventListener('input', updateMobileAiDelegateSubmit);
$('mobileAiDelegateRefresh').addEventListener('click', () => void refreshMobileAiDelegates());
$('mobileAiDelegateForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  const selected = mobileAiDelegateSelection();
  const task = mobileAiDelegateTask();
  const fingerprint = selected
    ? mobileAiDelegateFingerprint(task, selected.provider, selected.model) : '';
  if (!nativeAndroidRuntime || !isTauri() || working || !selected ||
      !task || task.length > 2000 || new TextEncoder().encode(task).length > 3000 ||
      mobileAiDelegateCapacity !== 2 ||
      mobileAiDelegateRunning + mobileAiDelegatePending.size >= 2 ||
      mobileAiDelegatePending.has(fingerprint)) return;
  if (!window.confirm(`Run ONE AI delegate task with ${selected.provider} / ${selected.model}? The provider may charge for this request. No automatic retries, follow-ups or replay will occur.`)) return;
  // Reserve synchronously after the confirmation, before any await. A second
  // distinct task may be submitted only while the shared limit permits.
  mobileAiDelegatePending.set(fingerprint, createChatRequestId());
  renderMobileAiDelegateJobs();
  $('mobileAiDelegateFeedback').textContent = 'Request sent to Android; awaiting actual provider outcome. No result has been confirmed.';
  try {
    const response = await api('/api/mobile-ai-agents', {
      action:'run',task,provider:selected.provider,model:selected.model,
      requestId:mobileAiDelegatePending.get(fingerprint),userInitiated:true,
    });
    const job = response?.job;
    if (!job || typeof job.id !== 'string' || !job.id ||
        job.task !== task || job.provider !== selected.provider ||
        job.model !== selected.model ||
        !['completed','failed','cancelled','interrupted'].includes(job.status))
      throw Error('Native provider did not return a matching terminal delegate receipt');
    $('mobileAiDelegateFeedback').textContent = job.status === 'completed'
      ? 'Android returned a completed provider receipt. See saved job status for result.'
      : `Android returned a ${job.status} delegate receipt. No success is claimed.`;
  } catch (error) {
    $('mobileAiDelegateFeedback').textContent =
      `AI delegate result not confirmed: ${mobileAiDelegatePreview(String(error.message), 220)}. Check native status before trying again; no automatic retry.`;
  } finally {
    mobileAiDelegatePending.delete(fingerprint);
    await refreshMobileAiDelegates();
    updateMobileAiDelegateSubmit();
  }
});
// One explicit Android Prime session coordinates at most two provider-backed
// workers. Every network request starts from a user gesture; these reports
// never imply that a worker executed local tools, browser actions or MCP.
type MobilePrimeReport = {source:'user'|'provider';text:string;
  provider_verified:boolean;delegate_job_id?:string|null};
type MobilePrimeWorker = {id:string;label:string;assignment:string;
  provider:string;model:string;status:string;reports:MobilePrimeReport[];
  error?:string|null};
type MobilePrimeTask = {id:string;goal:string;status:string;
  summary?:string|null;workers:MobilePrimeWorker[]};
type MobilePrimeHistoryWorker = {
  id:string; label:string; assignment:string; provider:string; model:string;
  status:string; reportCount:number; verifiedReportCount:number;
  providerVerified:boolean; error?:string|null;
};
type MobilePrimeHistoryTask = {
  id:string;goal:string;status:string;summary?:string|null;
  summaryUserWritten?:boolean;summaryTruncated?:boolean;autoVerified?:boolean;
  workers:MobilePrimeHistoryWorker[];
};
let mobilePrimeTask:MobilePrimeTask|null = null;
let mobilePrimeHistory:MobilePrimeHistoryTask[] = [];
let mobilePrimeVerified = false;
let mobilePrimeUpdating = false;
let mobilePrimeRefreshing = false;
let mobilePrimeCheckedAt = 0;
const mobilePrimePending = new Set<string>();
const mobilePrimeExpandedWorkers = new Set<string>();
const mobilePrimeLimits = {goal:700,task:650,message:700};
function mobilePrimeBytes(text:string):number { return new TextEncoder().encode(text).length; }
function mobilePrimeValidText(text:string,max:number):boolean {
  return text.trim().length>0 && mobilePrimeBytes(text.trim())<=max &&
    !/[\u0000-\u0008\u000b\u000c\u000e-\u001f]/.test(text);
}
function validMobilePrimeWorker(input:unknown):input is MobilePrimeWorker {
  const row=input as MobilePrimeWorker;
  return !!row && typeof row==='object' && !Array.isArray(row) &&
    typeof row.id==='string' && /^[a-f0-9-]{20,60}$/i.test(row.id) &&
    typeof row.label==='string' && row.label.length<=80 &&
    typeof row.assignment==='string' && mobilePrimeBytes(row.assignment)<=700 &&
    typeof row.provider==='string' && row.provider.length<=32 &&
    typeof row.model==='string' && row.model.length<=120 &&
    typeof row.status==='string' && row.status.length<=30 &&
    Array.isArray(row.reports) && row.reports.length<=12 &&
    row.reports.every(report => !!report &&
      (report.source==='user'||report.source==='provider') &&
      typeof report.text==='string' && new TextEncoder().encode(report.text).length<=20*1024 &&
      typeof report.provider_verified==='boolean');
}
function validMobilePrimeTask(task:unknown):task is MobilePrimeTask {
  const run=task as MobilePrimeTask;
  return !!run && typeof run==='object' && !Array.isArray(run) &&
    typeof run.id==='string' && /^[a-f0-9-]{20,60}$/i.test(run.id) &&
    typeof run.goal==='string' && mobilePrimeBytes(run.goal)<=1000 &&
    typeof run.status==='string' && ['active','finished','cancelled','interrupted'].includes(run.status) &&
    Array.isArray(run.workers) && run.workers.length<=2 && run.workers.every(validMobilePrimeWorker);
}
function validMobilePrimeHistory(value:unknown):value is MobilePrimeHistoryTask[] {
  if (!Array.isArray(value)||value.length>12)return false;
  const uuids=new Set<string>();
  return value.every((raw:unknown)=>{
    const task=raw as MobilePrimeHistoryTask;
    return !!task && typeof task==='object'&&!Array.isArray(task) &&
      typeof task.id==='string' && /^[a-f0-9-]{20,60}$/i.test(task.id) &&
      !uuids.has(task.id) && uuids.add(task.id) &&
      typeof task.goal==='string' && mobilePrimeBytes(task.goal)<=1000 &&
      ['finished','cancelled'].includes(task.status) &&
      (task.summary==null || typeof task.summary==='string'&&mobilePrimeBytes(task.summary)<=1000) &&
      Array.isArray(task.workers) && task.workers.length<=2 &&
      task.workers.every(worker=>!!worker && typeof worker.id==='string'&&
        /^[a-f0-9-]{20,60}$/i.test(worker.id) &&
        typeof worker.label==='string'&&worker.label.length<=80 &&
        typeof worker.assignment==='string'&&mobilePrimeBytes(worker.assignment)<=700 &&
        typeof worker.provider==='string'&&worker.provider.length<=32 &&
        typeof worker.model==='string'&&worker.model.length<=120 &&
        typeof worker.status==='string'&&worker.status.length<=30 &&
        Number.isInteger(worker.reportCount)&&worker.reportCount>=0&&worker.reportCount<=12 &&
        Number.isInteger(worker.verifiedReportCount)&&worker.verifiedReportCount>=0&&
        worker.verifiedReportCount<=worker.reportCount&&
        typeof worker.providerVerified==='boolean');
  });
}
function mobilePrimeWorkerState(status:string):string {
  return ({active:'Active',running:'Running',waiting:'Idle',finished:'Finished',
    cancelled:'Cancelled',failed:'Failed',interrupted:'Interrupted'} as Record<string,string>)[status]
    || 'Unverified';
}
function openPrimeAdvancedControls(workerId?:string):void {
  openToolPanel('subagents');
  ($('androidPrimeAdvanced') as HTMLDetailsElement).open=true;
  if(workerId){
    const input=$('mobilePrimeWorkerList').querySelector(
      '[data-prime-worker-id="'+workerId+'"] textarea') as HTMLTextAreaElement|null;
    input?.focus({preventScroll:true});
  }
}
function makePrimeOverviewWorker(
  worker:MobilePrimeWorker|MobilePrimeHistoryWorker,taskId:string,
  isCurrent:boolean
):HTMLElement {
  const key=taskId+':'+worker.id;
  const details=el('details','android-prime-worker-row') as HTMLDetailsElement;
  details.dataset.workerId=worker.id;
  details.open=mobilePrimeExpandedWorkers.has(key);
  const heading=el('summary');
  const dot=el('span','android-prime-worker-dot');
  dot.dataset.running=String(worker.status==='running');
  const label=el('strong','',worker.label);
  const state=el('span','android-prime-worker-state',mobilePrimeWorkerState(worker.status));
  heading.append(dot,label,state,el('span','','⌄'));
  details.append(heading);
  const body=el('div','android-prime-worker-details');
  body.append(el('div','',worker.assignment.slice(0,240)),
    el('div','','Provider: '+worker.provider+' · '+worker.model.slice(0,80)));
  if('reports' in worker){
    for(const report of worker.reports.slice(-4)){
      body.append(el('small','',report.provider_verified
        ? 'Verified provider reply' : 'User note'));
      body.append(el('pre','',mobileAiDelegatePreview(report.text,500)));
    }
    if(worker.error)body.append(el('small','terminal-error',worker.error.slice(0,180)));
  } else {
    body.append(el('small','',
      worker.verifiedReportCount+' verified provider reply/replies · '+worker.reportCount+' reports recorded'));
    if(worker.error)body.append(el('small','terminal-error',worker.error.slice(0,180)));
  }
  if(isCurrent && mobilePrimeTask?.status==='active' &&
      ['waiting','running'].includes(worker.status)){
    const controls=el('div','mobile-mcp-actions');
    const message=el('button','','Message');
    message.type='button';
    message.disabled=worker.status!=='waiting'||mobilePrimePending.size>0;
    message.addEventListener('click',()=>openPrimeAdvancedControls(worker.id));
    const finish=el('button','','Finish');
    finish.type='button';
    finish.disabled=worker.status!=='waiting'||mobilePrimePending.size>0;
    finish.addEventListener('click',()=>void mobilePrimeWorkerAction('finish',worker.id));
    const cancel=el('button','','Cancel');
    cancel.type='button';
    cancel.addEventListener('click',()=>void mobilePrimeWorkerAction('cancel',worker.id));
    controls.append(message,finish,cancel);
    body.append(controls);
  }
  details.append(body);
  details.addEventListener('toggle',()=>{
    if(details.open)mobilePrimeExpandedWorkers.add(key);
    else mobilePrimeExpandedWorkers.delete(key);
  });
  return details;
}
function primeSessionCard(task:MobilePrimeTask|MobilePrimeHistoryTask,
  workers:(MobilePrimeWorker|MobilePrimeHistoryWorker)[],position:number,
  isCurrent:boolean):HTMLElement {
  const card=el('article','android-prime-session');
  card.dataset.state=task.status;
  card.dataset.primeTaskId=task.id;
  const header=el('div','android-prime-session-head');
  const badge=el('span','android-prime-number',position);
  const text=el('div');
  text.append(el('strong','',task.goal.slice(0,110)),
    el('small','',workers.length+' worker(s) · '+mobilePrimeWorkerState(task.status)));
  header.append(badge,text);
  card.append(header);
  for(const worker of workers){
    card.append(makePrimeOverviewWorker(worker,task.id,isCurrent));
  }
  if(!workers.length)
    card.append(el('p','android-prime-overview-state','No workers were started for this Prime session.'));
  if(task.summary){
    const summary=el('details','android-prime-worker-row');
    summary.append(el('summary','','User-written Prime summary'),
      el('div','android-prime-worker-details',task.summary.slice(0,650)));
    card.append(summary);
  }
  return card;
}
function renderPrimeOverview():void {
  if(!nativeAndroidRuntime)return;
  const run=mobilePrimeTask;
  // Only report progress for an actual native task. The ring measures workers
  // with confirmed finished status, never speculative tokens or fake usage.
  const workerTotal=mobilePrimeVerified && run ? run.workers.length : 0;
  const finished=workerTotal ? run!.workers.filter(worker=>worker.status==='finished').length : 0;
  const progress=workerTotal ? Math.round(finished/workerTotal*100) : null;
  $('androidComposerProgressValue').textContent=progress===null?'—':progress+'%';
  $('androidComposerProgress').title=progress===null?'No verified Prime task progress':
    `${finished} of ${workerTotal} Prime workers finished`;
  $('androidComposerProgressRing').style.setProperty('--task-progress',String(progress??0));
  const activeWorkers=run?.status==='active'
    ? run.workers.filter(w=>w.status==='running'): [];
  const isRunning=activeWorkers.length>0 || mobilePrimePending.size>0;
  $('androidSubagentsHeaderDot').dataset.running=String(isRunning);
  $('androidSubagentsToggle').dataset.running=String(isRunning);
  $('androidPrimeActiveCount').textContent='Active · '+activeWorkers.length+' / 2';
  $('androidPrimeOverviewState').textContent=!mobilePrimeVerified
    ? 'Native worker status is unavailable; no activity is assumed.'
    : mobilePrimePending.size
      ? 'One or more approved provider requests are pending confirmation.'
      : run?.status==='active'
        ? 'Prime session active · '+run.workers.length+' / 2 worker slots used'
        : 'Native Prime ready · provider calls require explicit approval';
  const active=$('androidPrimeActiveRows');active.replaceChildren();
  if(run?.status==='active' && activeWorkers.length){
    active.append(primeSessionCard(run,activeWorkers,1,true));
  } else active.append(el('p','android-prime-empty','No active sub-agents'));
  const historyTasks=[
    ...(run && run.workers.some(worker=>worker.status!=='running')
      ? [{task:run,workers:run.workers.filter(worker=>worker.status!=='running'),current:true}]
      : []),
    ...(run && run.status!=='active' && !run.workers.length
      ? [{task:run,workers:[],current:true}] : []),
    ...mobilePrimeHistory.filter(item=>item.id!==run?.id)
      .map(item=>({task:item,workers:item.workers,current:false}))
  ];
  const total=historyTasks.reduce((sum,entry)=>sum+Math.max(1,entry.workers.length),0);
  $('androidPrimeHistoryCount').textContent=String(total);
  const history=$('androidPrimeHistoryRows');history.replaceChildren();
  if(!historyTasks.length){
    history.append(el('p','android-prime-empty','No previous Prime workers'));
  } else historyTasks.forEach((entry,index)=>{
    history.append(primeSessionCard(entry.task,entry.workers,index+1,entry.current));
  });
  renderPrimeChatActivity();
}
function renderPrimeChatActivity():void {
  if(!nativeAndroidRuntime)return;
  const box=$('androidPrimeChatActivity');
  const run=mobilePrimeTask;
  // The chat chip follows an ACTIVE Prime only; finished/cancelled runs stay in
  // the sidebar history instead of appearing in unrelated future chats.
  const display=mobilePrimeVerified && !!run && run.status==='active';
  box.classList.toggle('hidden',!display);
  if(!display||!run){box.replaceChildren();return;}
  const expanded=box.querySelector('details')?.open===true;
  box.replaceChildren();
  const verifiedWorkerRunning=run.workers.some(worker=>worker.status==='running');
  const running=verifiedWorkerRunning||mobilePrimePending.size>0;
  const title=el('div','android-prime-chat-heading');
  if(running)title.append(el('span','android-prime-running-spinner'));
  title.append(el('strong','',verifiedWorkerRunning?'Prime · Worker running':
    mobilePrimePending.size?'Prime · Provider request pending':
    'Prime · '+mobilePrimeWorkerState(run.status)));
  const open=el('button','','Sub-agents');
  open.type='button';
  open.addEventListener('click',()=>openToolPanel('subagents'));
  title.append(open);box.append(title);
  const details=el('details');
  details.open=expanded;
  details.append(el('summary','',run.workers.length+
    ' worker(s) · '+(running?'Processing approved request':'View worker status')));
  if(!run.workers.length)details.append(el('span','android-prime-chat-worker','No worker started.'));
  for(const worker of run.workers){
    const item=el('div','android-prime-chat-worker');
    item.append(el('strong','',worker.label+' · '+mobilePrimeWorkerState(worker.status)),
      el('span','', ' · '+worker.provider+'/'+worker.model.slice(0,48)));
    details.append(item);
  }
  box.append(details);
}
function renderMobilePrime():void {
  const run=mobilePrimeTask;
  const active=!!run && run.status==='active';
  $('mobilePrimeCurrent').classList.toggle('hidden',!run);
  $('mobilePrimeCreateForm').classList.toggle('hidden',active);
  $('mobilePrimeCapacity').textContent=run
    ? `${run.workers.length}/2 workers · ${run.status}` : 'Up to 2 workers';
  $('mobilePrimeHeadline').textContent=run?.goal || 'Prime session';
  $('mobilePrimeState').textContent=run
    ? `Prime ${run.status} · ${run.workers.filter(w=>w.status==='running').length} worker(s) running · ${mobilePrimePending.size} request(s) pending`
    : 'No Prime session';
  const provider=mobileAiDelegateSelection();
  const objective=($('mobilePrimeObjective') as HTMLTextAreaElement).value;
  const task=($('mobilePrimeWorkerTask') as HTMLTextAreaElement).value;
  const submit=$('mobilePrimeCreateForm').querySelector('button[type="submit"]') as HTMLButtonElement|null;
  if(submit) submit.disabled=!nativeAndroidRuntime||!isTauri()||
    !mobilePrimeVerified||active||mobilePrimeUpdating||
    !mobilePrimeValidText(objective,mobilePrimeLimits.goal);
  const workerButton=$('mobilePrimeWorkerForm').querySelector('button[type="submit"]') as HTMLButtonElement|null;
  if(workerButton) workerButton.disabled=!nativeAndroidRuntime||!isTauri()||!mobilePrimeVerified||
    !active||!provider||run!.workers.length+mobilePrimePending.size>=2||
    !mobilePrimeValidText(task,mobilePrimeLimits.task)||working;
  ($('mobilePrimeFinish') as HTMLButtonElement).disabled=!active||
    mobilePrimePending.size>0||mobilePrimeUpdating||
    !mobilePrimeValidText(($('mobilePrimeSummary') as HTMLTextAreaElement).value,1000);
  ($('mobilePrimeCancel') as HTMLButtonElement).disabled=!active||mobilePrimeUpdating;
  const list=$('mobilePrimeWorkerList');list.replaceChildren();
  if(!run?.workers.length){
    list.append(el('p','tool-empty','No workers yet. Add one task after selecting a configured AI provider.'));
    renderPrimeOverview();
    return;
  }
  for(const worker of run.workers){
    const card=el('div','android-prime-worker');
    card.append(el('strong','',`${worker.label} · ${worker.status}`),
      el('small','',`${worker.provider} / ${worker.model.slice(0,80)} · ${worker.assignment.slice(0,160)}`));
    for(const report of worker.reports.slice(-8)){
      const note=el('div','android-prime-report');
      note.append(el('small','',report.source==='provider'&&report.provider_verified
        ? 'Verified provider reply' : report.source==='user'?'User message':'Provider result unverified'),
        el('pre','',mobileAiDelegatePreview(report.text,1200)));
      card.append(note);
    }
    if(worker.error)card.append(el('small','terminal-error',mobileAiDelegatePreview(worker.error,180)));
    if(active && (worker.status==='waiting'||worker.status==='running')){
      const form=el('form','mobile-mcp-form');
      form.dataset.primeWorkerId=worker.id;
      const label=el('label','',`Message ${worker.label}`);
      const input=document.createElement('textarea');
      input.rows=2;input.maxLength=700;
      input.placeholder='One follow-up for this worker';
      const controls=el('div','mobile-mcp-actions');
      const send=el('button','','Send message');send.type='submit';
      const finish=el('button','','Finish worker');finish.type='button';
      const cancel=el('button','','Cancel worker');cancel.type='button';
      send.disabled=worker.status!=='waiting'||mobilePrimePending.size>=2||working;
      finish.disabled=worker.status!=='waiting'||mobilePrimePending.size>0;
      controls.append(send,finish,cancel);label.append(input);form.append(label,controls);
      form.addEventListener('submit',(event:Event)=>{
        event.preventDefault();void mobilePrimeWorkerAction('message',worker.id,input.value);
      });
      finish.addEventListener('click',()=>void mobilePrimeWorkerAction('finish',worker.id));
      cancel.addEventListener('click',()=>void mobilePrimeWorkerAction('cancel',worker.id));
      card.append(form);
    }
    list.append(card);
  }
  renderPrimeOverview();
}
async function refreshMobilePrime():Promise<void>{
  if(!nativeAndroidRuntime||!isTauri()||mobilePrimeRefreshing)return;
  mobilePrimeRefreshing=true;
  mobilePrimeCheckedAt=Date.now();
  try{
    const response=await api('/api/mobile-prime',{action:'status'});
    if(!response || response.maxConcurrentWorkers!==2 ||
       (response.task!==null&&!validMobilePrimeTask(response.task)) ||
       (response.history!==undefined&&!validMobilePrimeHistory(response.history)))
      throw Error('Native Prime status receipt malformed');
    mobilePrimeTask=response.task;
    mobilePrimeHistory=response.history||[];
    mobilePrimeVerified=true;
    $('mobilePrimeFeedback').textContent=runPrimeStatusText();
  }catch(error){
    mobilePrimeVerified=false;
    $('mobilePrimeFeedback').textContent='Prime status could not be verified: '+
      mobileAiDelegatePreview(String(error),160);
  }finally{
    mobilePrimeRefreshing=false;
    const editing=document.activeElement?.closest('#mobilePrimeWorkerList form');
    if(editing)renderPrimeOverview();
    else renderMobilePrime();
  }
}
function runPrimeStatusText():string{
  return mobilePrimeTask
    ? `Native Prime ${mobilePrimeTask.status}. Worker reports are bounded provider replies, not local tool executions.`
    : 'Prime ready. Start one goal and explicitly approve each provider-backed worker request.';
}
async function mobilePrimeWorkerAction(action:'message'|'finish'|'cancel',workerId:string,message=''):Promise<void>{
  const run=mobilePrimeTask;
  if(!mobilePrimeVerified||!run||run.status!=='active'||mobilePrimePending.size>=2)return;
  const worker=run.workers.find(w=>w.id===workerId);
  if(!worker)return;
  if(action!=='cancel' && worker.status!=='waiting')return;
  if(action==='message'&&!mobilePrimeValidText(message,mobilePrimeLimits.message))return;
  if(!window.confirm(action==='message'
    ? `Send one additional provider request to ${worker.label}? Model usage may be billed; no tools will run.`
    : action==='finish'?`Mark ${worker.label} finished after reviewing its reports?`
      : `Cancel ${worker.label}? Remote provider work may already have happened.`))return;
  const requestKey=`${workerId}:${action}`;
  if(mobilePrimePending.has(requestKey))return;
  mobilePrimePending.add(requestKey);renderMobilePrime();
  try{
    const payload:Record<string,unknown>={action,taskId:run.id,workerId,userInitiated:true};
    if(action==='message'){
      payload.message=message.trim();payload.requestId=createChatRequestId();
    }
    await api('/api/mobile-prime',payload);
    $('mobilePrimeFeedback').textContent=action==='message'
      ? 'Worker provider request returned. Refreshing verified reports.'
      : `Worker ${action} receipt returned. Refreshing Prime state.`;
  }catch(error){
    $('mobilePrimeFeedback').textContent='Worker action not verified: '+mobileAiDelegatePreview(String(error),175);
  }finally{
    mobilePrimePending.delete(requestKey);await refreshMobilePrime();
  }
}
$('mobilePrimeCreateForm').addEventListener('submit',async(event:Event)=>{
  event.preventDefault();
  const goal=($('mobilePrimeObjective') as HTMLTextAreaElement).value.trim();
  if(!mobilePrimeVerified||mobilePrimeUpdating||mobilePrimeTask?.status==='active'||
    !mobilePrimeValidText(goal,mobilePrimeLimits.goal)||
    !window.confirm('Create this Prime coordination session? Workers run only when you approve their individual tasks.'))return;
  mobilePrimeUpdating=true;renderMobilePrime();
  try{
    const response=await api('/api/mobile-prime',{action:'start',goal,userInitiated:true});
    if(!validMobilePrimeTask(response?.task))throw Error('Prime was not confirmed by native backend');
    mobilePrimeTask=response.task;
    $('mobilePrimeFeedback').textContent='Prime session created. Add up to two explicitly approved workers.';
  }catch(error){$('mobilePrimeFeedback').textContent='Could not create Prime: '+mobileAiDelegatePreview(String(error),170);}
  finally{mobilePrimeUpdating=false;await refreshMobilePrime();}
});
$('mobilePrimeWorkerForm').addEventListener('submit',async(event:Event)=>{
  event.preventDefault();
  const run=mobilePrimeTask,selected=mobileAiDelegateSelection();
  const task=($('mobilePrimeWorkerTask') as HTMLTextAreaElement).value.trim();
  const label=($('mobilePrimeWorkerRole') as HTMLSelectElement).value;
  if(!run||run.status!=='active'||!selected||!mobilePrimeVerified||
    !['research','code','review','qa'].includes(label)||
    run.workers.length+mobilePrimePending.size>=2||
    !mobilePrimeValidText(task,mobilePrimeLimits.task)||working||
    !window.confirm(`Run ${label} worker with ${selected.provider} / ${selected.model}? The provider may charge; no shell, browser, MCP tools or automatic follow-ups.`))return;
  const requestKey=createChatRequestId();mobilePrimePending.add(requestKey);renderMobilePrime();
  $('mobilePrimeFeedback').textContent='Running explicitly approved AI worker; awaiting provider receipt.';
  try{
    const reply=await api('/api/mobile-prime',{action:'spawn',taskId:run.id,
      label,provider:selected.provider,model:selected.model,task,requestId:requestKey,userInitiated:true});
    if(reply?.providerReceipt!==true||!validMobilePrimeWorker(reply.worker))
      throw Error('Provider did not return verified worker receipt');
    ($('mobilePrimeWorkerTask') as HTMLTextAreaElement).value='';
    $('mobilePrimeFeedback').textContent='Worker replied and was saved in native Prime history.';
  }catch(error){$('mobilePrimeFeedback').textContent='Worker provider result not verified: '+mobileAiDelegatePreview(String(error),180);}
  finally{mobilePrimePending.delete(requestKey);await refreshMobilePrime();}
});
$('mobilePrimeObjective').addEventListener('input',renderMobilePrime);
$('mobilePrimeWorkerTask').addEventListener('input',renderMobilePrime);
$('mobilePrimeSummary').addEventListener('input',renderMobilePrime);
$('provider').addEventListener('change',renderMobilePrime);
$('model').addEventListener('input',renderMobilePrime);
$('mobilePrimeRefresh').addEventListener('click',()=>void refreshMobilePrime());
$('mobilePrimeFinish').addEventListener('click',async()=>{
  const run=mobilePrimeTask;
  const summary=($('mobilePrimeSummary') as HTMLTextAreaElement).value.trim();
  if(!run||run.status!=='active'||!mobilePrimeValidText(summary,1000)||
    mobilePrimePending.size>0||!window.confirm('Finish Prime with YOUR summary? This does not certify worker claims or run tools.'))return;
  mobilePrimeUpdating=true;renderMobilePrime();
  try{await api('/api/mobile-prime',{action:'finish',taskId:run.id,summary,userInitiated:true});
    $('mobilePrimeFeedback').textContent='Prime finished using your summary; not automatically verified.';
  }catch(error){$('mobilePrimeFeedback').textContent='Finish not confirmed: '+mobileAiDelegatePreview(String(error),170);}
  finally{mobilePrimeUpdating=false;await refreshMobilePrime();}
});
$('mobilePrimeCancel').addEventListener('click',async()=>{
  const run=mobilePrimeTask;
  if(!run||run.status!=='active'||!window.confirm('Cancel Prime and all active worker requests? Provider billing may already have occurred.'))return;
  mobilePrimeUpdating=true;renderMobilePrime();
  try{await api('/api/mobile-prime',{action:'cancel',taskId:run.id,userInitiated:true});
    $('mobilePrimeFeedback').textContent='Prime cancellation requested. Inspect saved native state.';
  }catch(error){$('mobilePrimeFeedback').textContent='Cancellation not verified: '+mobileAiDelegatePreview(String(error),170);}
  finally{mobilePrimeUpdating=false;await refreshMobilePrime();}
});
function closePrimeOptionsMenu():void {
  $('androidPrimeOptionsMenu').classList.add('hidden');
  $('androidPrimeOptionsToggle').setAttribute('aria-expanded','false');
}
$('androidPrimeOptionsToggle').addEventListener('click',(event:MouseEvent)=>{
  event.stopPropagation();
  const opening=$('androidPrimeOptionsMenu').classList.contains('hidden');
  closePrimeOptionsMenu();
  if(opening){
    $('androidPrimeOptionsMenu').classList.remove('hidden');
    $('androidPrimeOptionsToggle').setAttribute('aria-expanded','true');
  }
});
$('androidPrimeOptionsMenu').addEventListener('click',async(event:MouseEvent)=>{
  const action=(event.target as Element).closest<HTMLButtonElement>('[data-prime-option]')?.dataset.primeOption;
  if(!action)return;
  closePrimeOptionsMenu();
  if(action==='refresh'){await refreshMobilePrime();return;}
  if(action==='manage'){openPrimeAdvancedControls();return;}
  if(action==='copy'){
    if(!mobilePrimeVerified){
      announce('Native Prime status unavailable; nothing copied.');
      return;
    }
    const run=mobilePrimeTask;
    const workerLines=run?.workers.map(w=>
      w.label+' · '+mobilePrimeWorkerState(w.status)+' · '+w.provider+'/'+w.model).join('\n')||'No workers';
    const copied='Native Prime status\n'+(run?.goal||'No Prime session')+
      '\n'+(run?.status||'none')+'\n'+workerLines+
      '\nPrevious sessions: '+mobilePrimeHistory.length+'\n';
    try{
      if(!navigator.clipboard?.writeText)throw Error('Clipboard unavailable');
      await navigator.clipboard.writeText(copied);
      announce('Verified Prime status copied.',true);
    }catch{announce('Clipboard is unavailable; nothing copied.');}
  }
});
document.addEventListener('pointerdown',(event:PointerEvent)=>{
  if(!(event.target as Element).closest('.android-prime-options'))
    closePrimeOptionsMenu();
});
if(nativeAndroidRuntime){
  window.setTimeout(()=>{if(!document.hidden&&isTauri())void refreshMobilePrime();},900);
  window.setInterval(()=>{
    if(document.hidden||!isTauri()||mobilePrimeRefreshing)return;
    if(Date.now()-mobilePrimeCheckedAt<5_000)return;
    if(workspacePanel==='subagents'||mobilePrimeTask?.status==='active'||
      mobilePrimePending.size>0)void refreshMobilePrime();
  },2_000);
  document.addEventListener('visibilitychange',()=>{
    if(!document.hidden&&isTauri())void refreshMobilePrime();
  });
}
type MobileGoalSnapshot = {
  id: string; status: string; current_step: number; turns_used: number;
  active_turn?: { id: string; turn_number: number } | null;
  approval_reason?: string | null; stop_reason?: string | null;
  spec: { title: string; max_turns: number;
    steps: Array<{ description: string; requires_approval: boolean }> };
};
let mobileGoalSnapshot: MobileGoalSnapshot | null = null;
let mobileGoalRunBusy = false;
let activeMobileGoalAiRequestId: string | null = null;
let mobileGoalDraftBusy = false;
type PendingGoalMcpReview = {
  reviewId: string; requestId: string; goalId: string; turnId: string;
  name: string; arguments: Record<string, unknown>; argumentsText: string;
  expiresAt: number; originalStep: number; originalTurns: number;
};
let pendingGoalMcpReview: PendingGoalMcpReview | null = null;
let goalMcpReviewSubmitting = false;
let goalMcpReviewAction: 'approve' | 'reject' | null = null;
const goalMcpUuid = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
function validGoalMcpUuid(value: unknown): value is string {
  return typeof value === 'string' && goalMcpUuid.test(value);
}
function resetGoalMcpReview(): void {
  pendingGoalMcpReview = null;
  goalMcpReviewSubmitting = false;
  goalMcpReviewAction = null;
  $('mobileGoalMcpReview').classList.add('hidden');
  $('mobileGoalMcpReviewName').textContent = '';
  $('mobileGoalMcpReviewArguments').textContent = '';
  $('mobileGoalMcpReviewState').textContent = '';
}
function renderGoalMcpReview(): void {
  const pending = pendingGoalMcpReview;
  $('mobileGoalMcpReview').classList.toggle('hidden', !pending || !nativeAndroidRuntime || !isTauri());
  if (!pending) return;
  $('mobileGoalMcpReviewName').textContent = 'Exact proposed tool: ' + pending.name;
  $('mobileGoalMcpReviewArguments').textContent = pending.argumentsText;
  $('mobileGoalMcpReviewArguments').style.overflowWrap = 'anywhere';
  const remaining = Math.max(0, Math.ceil((pending.expiresAt - Date.now()) / 1000));
  $('mobileGoalMcpReviewState').textContent = remaining
    ? 'Expires within ' + remaining + ' seconds of the original proposal. No MCP call has been approved or executed.'
    : 'This proposal has expired. Approval is blocked; reject or reconcile the Goal checkpoint.';
  $('mobileGoalMcpApprove').disabled = goalMcpReviewSubmitting || remaining === 0 || !mobileMcpConnected;
  $('mobileGoalMcpReject').disabled = goalMcpReviewSubmitting;
}
function validatePendingGoalMcp(receipt: any, goal: MobileGoalSnapshot, requestId: string): PendingGoalMcpReview {
  const p = receipt?.pendingGoalTool;
  const ticket = receipt?.ticket;
  if (!receipt || !p || typeof p !== 'object' || Array.isArray(p) ||
      Object.keys(p).length !== 7 ||
      !['reviewId','requestId','goalId','turnId','name','arguments','expiresInSeconds']
        .every(key => Object.prototype.hasOwnProperty.call(p,key)) ||
      !validGoalMcpUuid(p.reviewId) || p.requestId !== requestId ||
      p.goalId !== goal.id || !validGoalMcpUuid(p.turnId) ||
      !/^[A-Za-z0-9_.:-]{1,64}$/.test(p.name) || p.expiresInSeconds !== 120 ||
      !p.arguments || typeof p.arguments !== 'object' || Array.isArray(p.arguments) ||
      !receipt.goal || receipt.goal.id !== goal.id ||
      receipt.goal.status !== 'in_turn' || receipt.goal.current_step !== goal.current_step ||
      receipt.goal.turns_used !== goal.turns_used + 1 ||
      ticket?.id !== p.turnId || receipt.goal.active_turn?.id !== p.turnId ||
      ticket?.turn_number !== goal.turns_used + 1 ||
      receipt.requestId !== requestId || receipt.answer !== null ||
      receipt.awaitingUserOutcome !== false || receipt.modelToolsExecuted !== false ||
      receipt.savedAsComplete !== false || receipt.delegatedToSubagent !== false ||
      receipt.delegatePersisted !== false || receipt.delegateJobId !== null)
    throw Error('Goal MCP proposal identity or nonexecution receipt is invalid.');
  let argumentsText: string;
  try { argumentsText = JSON.stringify(p.arguments, null, 2); }
  catch { throw Error('Goal MCP arguments are invalid.'); }
  if (!argumentsText || new TextEncoder().encode(argumentsText).length > 8192 ||
      /"(?:__proto__|prototype|constructor)"\s*:/u.test(argumentsText))
    throw Error('Goal MCP tool arguments failed bounded review validation.');
  return {reviewId:p.reviewId,requestId,goalId:goal.id,turnId:p.turnId,
    name:p.name,arguments:p.arguments,argumentsText,
    expiresAt:Date.now() + p.expiresInSeconds * 1000,
    originalStep:goal.current_step,originalTurns:goal.turns_used + 1};
}
function goalMcpCurrent(p: PendingGoalMcpReview): boolean {
  return pendingGoalMcpReview === p &&
    mobileGoalSnapshot?.id === p.goalId && mobileGoalSnapshot.status === 'in_turn' &&
    mobileGoalSnapshot.active_turn?.id === p.turnId &&
    mobileGoalSnapshot.current_step === p.originalStep &&
    mobileGoalSnapshot.turns_used === p.originalTurns;
}
function refreshGoalMcpOptions(): void {
  const ready = !!mobileGoalSnapshot && mobileGoalSnapshot.status === 'ready' &&
    !mobileGoalRunBusy && nativeAndroidRuntime && isTauri();
  const permitted = ready && mobileMcpConnected;
  $('mobileGoalMcpControls').classList.toggle('hidden', !permitted);
  $('mobileGoalMcpOptIn').disabled = !permitted;
  if (!permitted) $('mobileGoalMcpOptIn').checked = false;
  if (pendingGoalMcpReview) renderGoalMcpReview();
}
$('mobileGoalDelegateOptIn').addEventListener('change', () => {
  if ($('mobileGoalDelegateOptIn').checked) $('mobileGoalMcpOptIn').checked = false;
});
$('mobileGoalMcpOptIn').addEventListener('change', () => {
  if ($('mobileGoalMcpOptIn').checked) $('mobileGoalDelegateOptIn').checked = false;
});
type ReviewedGoalStep = {description:string;requires_approval:boolean};
let reviewedGoalSteps: ReviewedGoalStep[] = [];
function goalStepLines(): string[] {
  return String($('mobileGoalSteps').value).split(/\r?\n/)
    .map((line: string) => line.trim()).filter(Boolean);
}
// Match UNIQUE, unchanged descriptions regardless of their position. If a line
// is rewritten, removed, or becomes duplicated, its approval cannot migrate
// to a different step. Duplicate rows may be checked explicitly again.
function reconcileGoalStepApprovals(): void {
  const next = goalStepLines();
  const oldCount = new Map<string,number>();
  const newCount = new Map<string,number>();
  for (const row of reviewedGoalSteps)
    oldCount.set(row.description,(oldCount.get(row.description) || 0) + 1);
  for (const name of next)
    newCount.set(name,(newCount.get(name) || 0) + 1);
  const retained = new Map(reviewedGoalSteps.map(row => [row.description,row.requires_approval]));
  reviewedGoalSteps = next.map(description => ({
    description,requires_approval:oldCount.get(description) === 1 &&
      newCount.get(description) === 1 && retained.get(description) === true,
  }));
  renderGoalApprovalReview();
}
function renderGoalApprovalReview(): void {
  const visible = nativeAndroidRuntime && isTauri() && !mobileGoalSnapshot;
  $('mobileGoalApprovalReview').classList.toggle('hidden', !visible);
  $('mobileGoalRetryControls').classList.toggle('hidden', !visible);
  const container = $('mobileGoalApprovalRows');
  container.replaceChildren();
  reviewedGoalSteps.forEach((entry,index) => {
    const row = el('div','local-worker-job');
    const label = document.createElement('label');
    const checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    checkbox.checked = entry.requires_approval;
    checkbox.dataset.goalStepApproval = String(index);
    checkbox.setAttribute('aria-label', `Require user approval before step ${index + 1}: ${entry.description.slice(0,120)}`);
    label.append(checkbox, document.createTextNode(` Step ${index + 1}: ${entry.description}`));
    const detail = el('small','',entry.requires_approval
      ? 'Approval required BEFORE this step begins' : 'No prior approval requested');
    checkbox.addEventListener('change', () => {
      entry.requires_approval = checkbox.checked;
      detail.textContent = checkbox.checked
        ? 'Approval required BEFORE this step begins' : 'No prior approval requested';
      updateGoalApprovalSummary();
    });
    row.append(label,detail);
    container.append(row);
  });
  updateGoalApprovalSummary();
}
function updateGoalApprovalSummary(): void {
  const approvals = reviewedGoalSteps.filter(step => step.requires_approval).length;
  const duplicates = reviewedGoalSteps.length !== new Set(reviewedGoalSteps.map(step => step.description)).size;
  $('mobileGoalApprovalSummary').textContent = reviewedGoalSteps.length
    ? `Review before Create: ${reviewedGoalSteps.length} steps; ${approvals} require explicit approval; ${reviewedGoalSteps.length - approvals} do not. ${duplicates ? 'Repeated descriptions are ambiguous after edits: re-check each duplicate manually.' : 'Changed descriptions reset their approvals; unchanged unique lines retain them across reorder.'}`
    : 'Enter steps above to review approval requirements. No approval is assumed.';
}
$('mobileGoalSteps').addEventListener('input', reconcileGoalStepApprovals);
$('mobileGoalRetries').addEventListener('input', updateGoalApprovalSummary);
reconcileGoalStepApprovals();
function updateMobileGoalDraftButton() {
  const objective = String($('mobileGoalDraftObjective').value || '').trim();
  $('mobileGoalDraftButton').disabled = mobileGoalDraftBusy || mobileGoalRunBusy ||
    !!mobileGoalSnapshot || working || !nativeAndroidRuntime || !isTauri() ||
    !mobileAiDelegateSelection() || !objective || objective.length > 1000 ||
    new TextEncoder().encode(objective).length > 1000 ||
    /[\x00-\x1f\x7f]/u.test(objective);
}
type ReviewedGoalDraft = {
  title: string;
  steps: {description:string;requires_approval:boolean}[];
  max_turns: number;
  max_retries_per_step: number;
};
function validateReviewedGoalDraft(receipt: any, requestId: string): ReviewedGoalDraft {
  if (!receipt || receipt.requestId !== requestId ||
      receipt.requiresReview !== true || receipt.goalCreated !== false ||
      receipt.turnStarted !== false || receipt.toolsExecuted !== false)
    throw Error('Draft receipt does not prove review-only operation; no changes applied.');
  const draft = receipt.draft;
  const printable = (value: unknown, maxBytes: number) => typeof value === 'string' &&
    !!value.trim() && value === value.trim() &&
    new TextEncoder().encode(value).length <= maxBytes &&
    !/[\x00-\x1f\x7f]/u.test(value);
  if (!draft || typeof draft !== 'object' || Array.isArray(draft) ||
      !printable(draft.title, 200) ||
      !Array.isArray(draft.steps) || draft.steps.length < 1 || draft.steps.length > 16 ||
      !draft.steps.every((step: any) => step &&
        typeof step === 'object' && !Array.isArray(step) &&
        printable(step.description, 240) && typeof step.requires_approval === 'boolean') ||
      !Number.isInteger(draft.max_turns) ||
      draft.max_turns < draft.steps.length || draft.max_turns > 64 ||
      !Number.isInteger(draft.max_retries_per_step) ||
      draft.max_retries_per_step < 0 || draft.max_retries_per_step > 5)
    throw Error('Provider draft does not meet native goal structure limits; no changes applied.');
  if (draft.steps.map((step: {description:string}) => step.description).join('\n').length > 3000)
    throw Error('Draft steps exceed the editable form limit; no changes applied.');
  return draft as ReviewedGoalDraft;
}
function showMobileGoal(snapshot: MobileGoalSnapshot | null) {
  mobileGoalSnapshot = snapshot;
  $('mobileGoalCreate').classList.toggle('hidden', !!snapshot);
  $('mobileGoalDraftControls').classList.toggle('hidden', !!snapshot);
  $('mobileGoalActive').classList.toggle('hidden', !snapshot);
  renderGoalApprovalReview();
  updateMobileGoalDraftButton();
  if (!snapshot) {
    resetGoalMcpReview();
    $('mobileGoalDelegateOptIn').checked = false;
    $('mobileGoalMcpOptIn').checked = false;
    refreshGoalMcpOptions();
    return;
  }
  $('mobileGoalHeadline').textContent = snapshot.spec.title;
  const done = Math.min(snapshot.current_step, snapshot.spec.steps.length);
  $('mobileGoalProgress').textContent =
    `${snapshot.status.replaceAll('_',' ')} · ${done}/${snapshot.spec.steps.length} verified manual steps · ${snapshot.turns_used}/${snapshot.spec.max_turns} turns` +
    (snapshot.stop_reason ? ` · ${snapshot.stop_reason}` : '');
  const steps = $('mobileGoalStepsList');
  steps.replaceChildren();
  snapshot.spec.steps.forEach((step, index) => {
    const row = el('div', 'local-worker-job');
    row.append(el('strong', '', `${index + 1}. ${step.description}`),
      el('small', '', step.requires_approval ? 'Explicit approval required for this step' : 'No step approval required'),
      el('small', '', index < done ? 'Marked complete by user' :
        index === done ? 'Next / current step' : 'Pending'));
    steps.append(row);
  });
  const terminal = ['completed', 'failed', 'rejected', 'cancelled'].includes(snapshot.status);
  $('mobileGoalPause').disabled = mobileGoalRunBusy ||
    !['ready','awaiting_approval'].includes(snapshot.status);
  $('mobileGoalResume').disabled = mobileGoalRunBusy || snapshot.status !== 'paused';
  $('mobileGoalBegin').disabled = mobileGoalRunBusy || snapshot.status !== 'ready';
  const canStopProvider = mobileGoalRunBusy && !!activeMobileGoalAiRequestId;
  $('mobileGoalStopAi').classList.toggle('hidden', !canStopProvider);
  $('mobileGoalStopAi').disabled = !canStopProvider;
  $('mobileGoalRunAi').disabled = mobileGoalRunBusy || snapshot.status !== 'ready' ||
    !nativeAndroidRuntime || !isTauri();
  $('mobileGoalDelegateControls').classList.toggle('hidden',
    snapshot.status !== 'ready' || !nativeAndroidRuntime || !isTauri());
  $('mobileGoalDelegateOptIn').disabled = mobileGoalRunBusy || snapshot.status !== 'ready';
  if (snapshot.status !== 'ready') $('mobileGoalDelegateOptIn').checked = false;
  refreshGoalMcpOptions();
  if (pendingGoalMcpReview && !goalMcpCurrent(pendingGoalMcpReview)) resetGoalMcpReview();
  renderGoalMcpReview();
  $('mobileGoalInterrupt').disabled = mobileGoalRunBusy || snapshot.status !== 'in_turn';
  $('mobileGoalOutcome').classList.toggle('hidden', snapshot.status !== 'in_turn' ||
    !!pendingGoalMcpReview || goalMcpReviewSubmitting);
  $('mobileGoalOutcome').querySelectorAll('button').forEach((button: HTMLButtonElement) => {
    button.disabled = mobileGoalRunBusy;
  });
  $('mobileGoalApproval').classList.toggle('hidden', snapshot.status !== 'awaiting_approval');
  $('mobileGoalApproval').querySelectorAll('button').forEach((button: HTMLButtonElement) => {
    button.disabled = mobileGoalRunBusy;
  });
  $('mobileGoalCancel').disabled = mobileGoalRunBusy || terminal;
  $('mobileGoalArchive').disabled = mobileGoalRunBusy || !terminal;
  $('mobileGoalReload').disabled = mobileGoalRunBusy;
  if (snapshot.status === 'awaiting_approval')
    $('mobileGoalFeedback').textContent = snapshot.approval_reason || 'Explicit approval is required.';
}
async function refreshMobileGoal() {
  if (workspacePanel !== 'subagents' || mobileGoalRunBusy || mobileGoalDraftBusy) return;
  if (!nativeAndroidRuntime || !isTauri()) {
    $('mobileGoalFeedback').textContent = 'Persistent goal checkpoints require the installed Android app.';
    $('mobileGoalCreate').querySelector('button[type="submit"]').disabled = true;
    return;
  }
  try {
    const result = await api('/api/mobile-goal', { action: 'status' });
    if (workspacePanel === 'subagents') showMobileGoal(result.goal || null);
  } catch (error) { $('mobileGoalFeedback').textContent = String(error.message); }
}
async function callMobileGoal(request: Record<string, unknown>) {
  if (mobileGoalRunBusy || mobileGoalDraftBusy) return;
  try {
    const actions = ['create','archive','begin','finish','interrupt',
      'approval','cancel','pause','resume'];
    if (!actions.includes(String(request.action))) throw Error('Unsupported Goal mutation.');
    const result = await api('/api/mobile-goal', {...request,userInitiated:true});
    if (['interrupt','cancel','archive'].includes(String(request.action))) {
      // A user-issued checkpoint change invalidates an unexecuted MCP
      // proposal. No stale approve action may survive the state transition.
      resetGoalMcpReview();
      activeMobileGoalAiRequestId = null;
    }
    if (result.archived) {
      showMobileGoal(null);
      $('mobileGoalFeedback').textContent = 'Previous checkpoint archived; nothing was deleted.';
    } else {
      showMobileGoal(result.goal || null);
      $('mobileGoalFeedback').textContent = 'Saved to private goal checkpoint.';
    }
  } catch (error) { $('mobileGoalFeedback').textContent = String(error.message); }
}
$('mobileGoalCreate').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  if (!nativeAndroidRuntime || !isTauri() || mobileGoalDraftBusy || mobileGoalSnapshot) return;
  const steps = goalStepLines();
  const maxTurns = Number($('mobileGoalTurns').value);
  const maxRetries = Number($('mobileGoalRetries').value);
  if (!steps.length || steps.length > 16 || !Number.isInteger(maxTurns) ||
      maxTurns < steps.length || maxTurns > 64 || !Number.isInteger(maxRetries) ||
      maxRetries < 0 || maxRetries > 5 ||
      steps.some(step => !step || new TextEncoder().encode(step).length > 240 ||
        /[\x00-\x1f\x7f]/u.test(step)) ||
      !reviewedGoalSteps.every((step,index) => step.description === steps[index]) ||
      reviewedGoalSteps.length !== steps.length) {
    $('mobileGoalFeedback').textContent = 'Review 1–16 steps (each at most 240 UTF-8 bytes), a 1–64 turn budget, 0–5 retries and all step approvals before Create.';
    return;
  }
  const spec = {
    title: $('mobileGoalTitle').value.trim(),
    steps: reviewedGoalSteps.map(step => ({description:step.description,
      requires_approval:step.requires_approval})),
    max_turns: maxTurns, max_retries_per_step: maxRetries,
  };
  const button = $('mobileGoalCreate').querySelector('button[type="submit"]');
  button.disabled = true;
  await callMobileGoal({ action: 'create', spec });
  button.disabled = false;
});
$('mobileGoalDraftObjective').addEventListener('input', updateMobileGoalDraftButton);
$('provider').addEventListener('change', updateMobileGoalDraftButton);
$('model').addEventListener('input', updateMobileGoalDraftButton);
$('mobileGoalDraftButton').addEventListener('click', async () => {
  const selected = mobileAiDelegateSelection();
  const objective = String($('mobileGoalDraftObjective').value || '').trim();
  if (mobileGoalDraftBusy || mobileGoalRunBusy || mobileGoalSnapshot || working ||
      workspacePanel !== 'subagents' || !nativeAndroidRuntime || !isTauri() ||
      !selected || !objective || objective.length > 1000 ||
      new TextEncoder().encode(objective).length > 1000 ||
      /[\x00-\x1f\x7f]/u.test(objective)) {
    $('mobileGoalDraftFeedback').textContent =
      'Choose a configured provider/model and a printable objective of at most 1000 UTF-8 bytes.';
    return;
  }
  if (!window.confirm(`Request ONE AI goal draft using ${selected.provider} / ${selected.model}? Provider billing may apply. Nothing will be created or run: review and edit the suggested fields, then press Create checkpointed goal yourself.`))
    return;
  const original = {
    title:String($('mobileGoalTitle').value),
    steps:String($('mobileGoalSteps').value),
    turns:String($('mobileGoalTurns').value),
    retries:String($('mobileGoalRetries').value),
    approvals:JSON.stringify(reviewedGoalSteps),
  };
  const requestId = createChatRequestId();
  mobileGoalDraftBusy = true;
  updateMobileGoalDraftButton();
  $('mobileGoalCreate').querySelector('button[type="submit"]').disabled = true;
  $('mobileGoalDraftFeedback').textContent =
    'Requesting one provider draft… No goal, turn or tool has been created or executed.';
  try {
    const receipt = await api('/api/mobile-goal-draft', {
      provider:selected.provider,model:selected.model,objective,requestId,userInitiated:true,
    });
    const draft = validateReviewedGoalDraft(receipt, requestId);
    const currentSelected = mobileAiDelegateSelection();
    if (workspacePanel !== 'subagents' || mobileGoalSnapshot ||
        !currentSelected || currentSelected.provider !== selected.provider ||
        currentSelected.model !== selected.model ||
        String($('mobileGoalDraftObjective').value).trim() !== objective ||
        String($('mobileGoalTitle').value) !== original.title ||
        String($('mobileGoalSteps').value) !== original.steps ||
        String($('mobileGoalTurns').value) !== original.turns ||
        String($('mobileGoalRetries').value) !== original.retries ||
        JSON.stringify(reviewedGoalSteps) !== original.approvals)
      throw Error('Goal form or provider changed while drafting; stale suggestions discarded.');
    $('mobileGoalTitle').value = draft.title;
    $('mobileGoalSteps').value = draft.steps.map(step => step.description).join('\n');
    $('mobileGoalTurns').value = String(draft.max_turns);
    $('mobileGoalRetries').value = String(draft.max_retries_per_step);
    reviewedGoalSteps = draft.steps.map(step => ({
      description:step.description,requires_approval:step.requires_approval,
    }));
    renderGoalApprovalReview();
    $('mobileGoalDraftFeedback').textContent =
      `AI draft received for review only (${draft.steps.length} suggested steps; ${draft.steps.filter(step => step.requires_approval).length} require approval). Check/edit EACH approval, title, steps, turn and retry budgets; then use Create checkpointed goal separately. No automatic creation or execution.`;
  } catch (error) {
    $('mobileGoalDraftFeedback').textContent =
      `Draft not applied: ${mobileAiDelegatePreview(String(error.message), 220)} No goal was created or run automatically.`;
  } finally {
    mobileGoalDraftBusy = false;
    $('mobileGoalCreate').querySelector('button[type="submit"]').disabled = !nativeAndroidRuntime || !isTauri() || !!mobileGoalSnapshot;
    updateMobileGoalDraftButton();
  }
});
$('mobileGoalBegin').addEventListener('click', () => void callMobileGoal({ action: 'begin' }));
$('mobileGoalRunAi').addEventListener('click', async () => {
  const goal = mobileGoalSnapshot;
  if (mobileGoalRunBusy || working || !goal || goal.status !== 'ready' ||
      !nativeAndroidRuntime || !isTauri()) return;
  const provider = String($('provider').value || '').trim();
  const model = String($('model').value || '').trim();
  if (!['openai', 'anthropic', 'openrouter'].includes(provider) ||
      !state.providers.includes(provider) || !state.hasKeys.includes(provider) ||
      !model || model.length > 120 || /[\r\n\0]/.test(model)) {
    $('mobileGoalFeedback').textContent = 'Choose a configured Android AI provider and model in Connections & tools before running one step.';
    return;
  }
  const step = goal.spec.steps[goal.current_step]?.description || 'current step';
  const delegateToSubagent = ($('mobileGoalDelegateOptIn') as HTMLInputElement).checked;
  const allowMcpTools = ($('mobileGoalMcpOptIn') as HTMLInputElement).checked;
  if (delegateToSubagent && allowMcpTools) {
    $('mobileGoalFeedback').textContent = 'Choose either MCP tool proposal OR Sub-agent delegation, never both.';
    return;
  }
  if (allowMcpTools && (!mobileMcpConnected || $('mobileGoalMcpOptIn').disabled)) {
    $('mobileGoalMcpOptIn').checked = false;
    $('mobileGoalFeedback').textContent = 'An active connected MCP session is required; no Goal turn started.';
    return;
  }
  if (!window.confirm(`Run ONE ${delegateToSubagent ? 'bounded native AI Sub-agent' : allowMcpTools ? 'AI Goal request with optional MCP proposal' : 'AI'} request for step "${step.slice(0,100)}" using ${provider} / ${model}? The provider may charge for this request.${delegateToSubagent ? ' This task and result will be saved privately to native Sub-agents history. No additional tools or agents will run.' : ''}${allowMcpTools ? ' The model may propose ONE remote MCP tool, but it will NOT run unless you inspect its exact arguments and separately approve.' : ''} The answer is NOT a verified Goal outcome; you must review and record the actual result manually.`))
    return;
  $('mobileGoalDelegateOptIn').checked = false; // opt-in never carries to a later step
  $('mobileGoalMcpOptIn').checked = false; // never carry MCP consent forward
  const requestId = createChatRequestId();
  activeMobileGoalAiRequestId = requestId;
  mobileGoalRunBusy = true;
  $('mobileGoalStopAi').disabled = false;
  showMobileGoal(goal);
  $('mobileGoalFeedback').textContent = 'Requesting one user-approved AI step… No step has been completed.';
  try {
    const receipt = await api('/api/mobile-goal-run', {
      provider, model, requestId, userInitiated: true,
      delegateToSubagent, allowMcpTools,
    });
    // MCP is permission to PROPOSE, not an obligation to use a tool. If the
    // model simply answers in text, accept only the ordinary strictly
    // validated, unverified InTurn receipt below; never invent a proposal.
    if (allowMcpTools && receipt?.pendingGoalTool != null) {
      const pending = validatePendingGoalMcp(receipt, goal, requestId);
      pendingGoalMcpReview = pending;
      // The model has returned its proposed tool. There is NO active
      // provider call while a human decides: do not keep Stop/Cancel locked.
      mobileGoalRunBusy = false;
      activeMobileGoalAiRequestId = null;
      showMobileGoal(receipt.goal);
      $('mobileGoalEvidence').value = '';
      $('mobileGoalFeedback').textContent =
        'Goal MCP proposal received, but no tool has executed. Inspect the exact tool and JSON arguments; choose Approve or Reject before this proposal expires. The Goal remains in turn.';
      return;
    }
    if (!receipt || receipt.awaitingUserOutcome !== true ||
        !receipt.goal || receipt.goal.id !== goal.id || receipt.goal.status !== 'in_turn' ||
        receipt.goal.current_step !== goal.current_step ||
        receipt.goal.turns_used !== goal.turns_used + 1 ||
        typeof receipt.ticket?.id !== 'string' || !receipt.ticket.id ||
        receipt.goal.active_turn?.id !== receipt.ticket.id ||
        receipt.delegatedToSubagent !== delegateToSubagent ||
        receipt.delegatePersisted !== delegateToSubagent ||
        (delegateToSubagent && (typeof receipt.delegateJobId !== 'string' ||
          !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(receipt.delegateJobId))) ||
        (!delegateToSubagent && receipt.delegateJobId != null) ||
        receipt.modelToolsExecuted !== false || receipt.savedAsComplete !== false ||
        typeof receipt.answer !== 'string' || !receipt.answer.trim() ||
        new TextEncoder().encode(receipt.answer).length > 20_000)
      throw Error('AI step receipt was incomplete. Refresh the checkpoint before recording any outcome.');
    showMobileGoal(receipt.goal);
    // A provider answer is NOT evidence of completion. Leave the manual
    // evidence field blank so the user must explicitly assess the result.
    $('mobileGoalEvidence').value = '';
    const preview = receipt.answer.slice(0, 1200);
    $('mobileGoalFeedback').textContent = `${delegateToSubagent ? `Native AI Sub-agent ${receipt.delegateJobId} finished and saved to private history. ` : ''}AI answer (unverified; step remains in progress):\n${preview}${receipt.answer.length > 1200 ? '\n[Preview truncated]' : ''}\nReview the answer, then enter the actual verified outcome below. No automatic finish or retry.`;
    if (delegateToSubagent) void refreshMobileAiDelegates();
  } catch (error) {
    // Native run may have checkpointed a retryable interruption. Read its
    // authoritative state; never synthesize an outcome or retry automatically.
    mobileGoalRunBusy = false;
    resetGoalMcpReview();
    if (activeMobileGoalAiRequestId === requestId) activeMobileGoalAiRequestId = null;
    await refreshMobileGoal();
    $('mobileGoalFeedback').textContent =
      `AI step not confirmed: ${String(error.message).slice(0,240)}. Check the saved goal state; no completion was recorded and no automatic retry will occur.`;
    if (mobileGoalSnapshot) showMobileGoal(mobileGoalSnapshot);
    return;
  } finally {
    // A model's unexecuted proposal keeps this exact Goal turn gated until a
    // separate explicit review action; it is not a successful AI answer.
    if (!pendingGoalMcpReview || pendingGoalMcpReview.requestId !== requestId) {
      mobileGoalRunBusy = false;
      if (activeMobileGoalAiRequestId === requestId) activeMobileGoalAiRequestId = null;
    }
    if (mobileGoalSnapshot) showMobileGoal(mobileGoalSnapshot);
  }
});
async function submitGoalMcpReview(action: 'approve' | 'reject'): Promise<void> {
  const pending = pendingGoalMcpReview;
  if (!pending || goalMcpReviewSubmitting || !nativeAndroidRuntime || !isTauri()) return;
  if (!goalMcpCurrent(pending) || (action === 'approve' &&
      (!mobileMcpConnected || Date.now() >= pending.expiresAt))) {
    $('mobileGoalFeedback').textContent =
      'Goal MCP proposal is stale, expired or disconnected. Nothing was approved; refresh the native Goal checkpoint.';
    return;
  }
  const verb = action === 'approve'
    ? 'APPROVE ONE remote MCP tool call for ' + pending.name + '? Review the exact JSON shown above. The tool may change remote data and cannot necessarily be undone.'
    : 'REJECT this Goal MCP proposal? It will record a retryable Goal checkpoint, not execute the tool, and will not retry automatically.';
  if (!window.confirm(verb)) return;
  if (!goalMcpCurrent(pending) || (action === 'approve' &&
      (!mobileMcpConnected || Date.now() >= pending.expiresAt))) return;
  goalMcpReviewSubmitting = true;
  goalMcpReviewAction = action;
  // Approve may resume a cancellable provider request. Reject is only an
  // atomic checkpoint decision; no provider is running for it.
  mobileGoalRunBusy = true;
  activeMobileGoalAiRequestId = action === 'approve' ? pending.requestId : null;
  if (mobileGoalSnapshot) showMobileGoal(mobileGoalSnapshot);
  renderGoalMcpReview();
  $('mobileGoalStopAi').disabled = action !== 'approve';
  $('mobileGoalMcpReviewState').textContent =
    'Submitting one explicit ' + action + ' decision for this matching Goal ticket…';
  try {
    const receipt = await api('/api/goal-tool-review', {
      action,reviewId:pending.reviewId,requestId:pending.requestId,
      goalId:pending.goalId,turnId:pending.turnId,userInitiated:true,
    });
    if (!goalMcpCurrent(pending) || workspacePanel !== 'subagents')
      throw Error('Goal MCP decision returned after the current view changed.');
    if (action === 'reject') {
      const saved = receipt?.goal;
      if (receipt?.rejected !== true || receipt.executed !== false ||
          !saved || saved.id !== pending.goalId ||
          !['ready','failed'].includes(saved.status) ||
          saved.current_step !== pending.originalStep ||
          saved.turns_used !== pending.originalTurns || saved.active_turn != null)
        throw Error('Goal MCP rejection was not confirmed by the saved checkpoint.');
      resetGoalMcpReview();
      mobileGoalRunBusy = false;
      activeMobileGoalAiRequestId = null;
      showMobileGoal(saved);
      $('mobileGoalFeedback').textContent =
        'Goal MCP proposal rejected. No tool was executed. The retryable checkpoint is saved; another turn requires a NEW explicit user action.';
      return;
    }
    const saved = receipt?.goal;
    if (!saved || receipt.reviewId !== pending.reviewId ||
        receipt.requestId !== pending.requestId ||
        saved.id !== pending.goalId || saved.status !== 'in_turn' ||
        saved.current_step !== pending.originalStep ||
        saved.turns_used !== pending.originalTurns ||
        saved.active_turn?.id !== pending.turnId ||
        receipt.ticket?.id !== pending.turnId ||
        receipt.ticket?.turn_number !== pending.originalTurns ||
        receipt.awaitingUserOutcome !== true ||
        receipt.modelToolsExecuted !== true ||
        receipt.toolExecutionConfirmed !== true ||
        receipt.savedAsComplete !== false ||
        typeof receipt.answer !== 'string' || !receipt.answer.trim() ||
        new TextEncoder().encode(receipt.answer).length > 20_000)
      throw Error('Goal MCP tool execution or provider answer was not confirmed for this ticket.');
    resetGoalMcpReview();
    mobileGoalRunBusy = false;
    activeMobileGoalAiRequestId = null;
    showMobileGoal(saved);
    $('mobileGoalEvidence').value = '';
    $('mobileGoalFeedback').textContent =
      'Explicit MCP tool execution confirmed for this single turn. AI answer (UNVERIFIED; Goal still in turn):\n' +
      receipt.answer.slice(0,1200) + (receipt.answer.length > 1200 ? '\n[Preview truncated]' : '') +
      '\nReview the actual result and manually record an outcome. No automatic completion or next turn.';
  } catch (error) {
    // A network error could occur AFTER an irreversible remote tool call.
    // Never claim an approval, rejection, completed Goal, or safe retry.
    resetGoalMcpReview();
    mobileGoalRunBusy = false;
    activeMobileGoalAiRequestId = null;
    await refreshMobileGoal();
    $('mobileGoalFeedback').textContent =
      'Goal MCP review NOT confirmed: ' + String((error as Error)?.message || error).slice(0,160) +
      '. Reconciled the saved Goal checkpoint where available. No automatic retry or completion; inspect remote side effects before further action.';
  } finally {
    goalMcpReviewSubmitting = false;
    goalMcpReviewAction = null;
    // Success/reject already rendered the authoritative checkpoint; an
    // error's reconciliation has its own unverified warning. Re-rendering
    // here would overwrite that warning with a generic approval message.
    if (pendingGoalMcpReview) renderGoalMcpReview();
  }
}
$('mobileGoalMcpApprove').addEventListener('click', () => void submitGoalMcpReview('approve'));
$('mobileGoalMcpReject').addEventListener('click', () => void submitGoalMcpReview('reject'));
$('mobileGoalStopAi').addEventListener('click', async () => {
  if (pendingGoalMcpReview &&
      !(goalMcpReviewSubmitting && goalMcpReviewAction === 'approve')) return;
  const requestId = activeMobileGoalAiRequestId;
  if (!nativeAndroidRuntime || !isTauri() || !mobileGoalRunBusy || !requestId ||
      !window.confirm('Cancel this active AI Goal provider request? The saved Goal may record a retryable interruption. Previously completed remote work cannot be reversed.')) return;
  const button = $('mobileGoalStopAi') as HTMLButtonElement;
  button.disabled = true;
  try {
    const reply = await api('/api/chat-cancel',{requestId});
    if (reply?.cancelRequested !== true) throw Error('Provider cancellation was not acknowledged');
    if (activeMobileGoalAiRequestId === requestId)
      $('mobileGoalFeedback').textContent = 'Cancellation requested for the matching provider turn. Waiting for the native Goal checkpoint; no result is being marked complete.';
  } catch (error) {
    if (activeMobileGoalAiRequestId === requestId) {
      button.disabled = false;
      $('mobileGoalFeedback').textContent = `Goal AI cancellation not confirmed: ${mobileAiDelegatePreview(String(error.message),180)}`;
    }
  }
});
$('mobileGoalInterrupt').addEventListener('click', () => {
  const id = mobileGoalSnapshot?.active_turn?.id;
  if (id) void callMobileGoal({
    action: 'interrupt', turnId: id, reason: 'User marked current turn interrupted',
  });
});
$('mobileGoalOutcome').addEventListener('click', (event: MouseEvent) => {
  const button = (event.target as Element).closest<HTMLButtonElement>('[data-goal-outcome]');
  const id = mobileGoalSnapshot?.active_turn?.id;
  const detail = $('mobileGoalEvidence').value.trim();
  if (!button || !id) return;
  if (!detail) {
    $('mobileGoalFeedback').textContent = 'Enter the actual outcome before recording this step.';
    return;
  }
  void callMobileGoal({ action: 'finish', turnId: id, result: button.dataset.goalOutcome, detail });
});
$('mobileGoalApproval').addEventListener('click', (event: MouseEvent) => {
  const button = (event.target as Element).closest<HTMLButtonElement>('[data-goal-approval]');
  if (!button || !mobileGoalSnapshot) return;
  const approved = button.dataset.goalApproval === 'true';
  void callMobileGoal({ action: 'approval', approved,
    note: approved ? 'User explicitly approved in Android UI' : 'User explicitly rejected in Android UI' });
});
$('mobileGoalCancel').addEventListener('click', () => {
  if (mobileGoalSnapshot && window.confirm('Cancel this saved goal? Completed checkpoints remain until archived.'))
    void callMobileGoal({ action: 'cancel', reason: 'User cancelled in Android UI' });
});
$('mobileGoalPause').addEventListener('click', () => {
  if (mobileGoalSnapshot && ['ready','awaiting_approval'].includes(mobileGoalSnapshot.status) &&
      window.confirm('Pause this Goal checkpoint? No provider turns or tools will run; the exact prior approval gate is retained across restart.'))
    void callMobileGoal({action:'pause',userInitiated:true});
});
$('mobileGoalResume').addEventListener('click', () => {
  if (mobileGoalSnapshot?.status === 'paused' &&
      window.confirm('Resume this Goal checkpoint? This restores only the saved Ready/Approval state; no AI turn, tool or subagent will auto-run.'))
    void callMobileGoal({action:'resume',userInitiated:true});
});
$('mobileGoalArchive').addEventListener('click', () => {
  if (mobileGoalSnapshot && window.confirm('Archive this finished goal? Its checkpoint will be retained locally.'))
    void callMobileGoal({ action: 'archive' });
});
$('mobileGoalReload').addEventListener('click', () => void refreshMobileGoal());
async function monitorLocalJob(id: string) {
  // At most 20 short checks. No permanent interval or background polling.
  for (let i = 0; i < 20 && workspacePanel === 'subagents'; i++) {
    await new Promise<void>(resolve => window.setTimeout(resolve, 350));
    if (workspacePanel !== 'subagents') return;
    try {
      const result = await api('/api/mobile-agents', { action: 'get', id });
      await refreshLocalWorkers();
      if (['completed', 'failed', 'cancelled'].includes(result.job.status)) return;
    } catch { return; }
  }
}
$('localWorkerRefresh').addEventListener('click', () => void refreshLocalWorkers());
$('localWorkerForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  if (!nativeAndroidRuntime || !isTauri()) return;
  const type = $('localWorkerAction').value;
  const target = $('localWorkerTarget').value.trim();
  if (!['list', 'read', 'properties', 'search_names'].includes(type)) return;
  if (type !== 'list' && !target) {
    $('localWorkerFeedback').textContent = 'A relative path or search phrase is required.';
    return;
  }
  const job = type === 'search_names' ? { type, query: target } :
    { type, path: target };
  const button = $('localWorkerForm').querySelector('button[type="submit"]');
  button.disabled = true;
  $('localWorkerFeedback').textContent = 'Submitting a bounded local workspace job…';
  try {
    const result = await api('/api/mobile-agents', { action: 'submit', job });
    $('localWorkerFeedback').textContent = `Submitted job #${result.job.sequence}`;
    await refreshLocalWorkers();
    void monitorLocalJob(result.job.id);
  } catch (error) { $('localWorkerFeedback').textContent = String(error.message); }
  finally { button.disabled = false; }
});
$('filesInfoClose').addEventListener('click', () => $('filesInfo').classList.add('hidden'));
$('filesCopyPath').addEventListener('click', async () => {
  try {
    if (!selectedFileInfoPath) throw Error('Select a file first');
    await navigator.clipboard.writeText(`app://workspace/${selectedFileInfoPath}`);
    $('filesCopyFeedback').textContent = 'App-relative path copied';
  } catch { $('filesCopyFeedback').textContent = 'Clipboard unavailable. Read the path above.'; }
});
$('filesSearchForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  const query = $('filesSearchQuery').value.trim();
  if (!query || workspacePanel !== 'files') return;
  $('filesSearchStatus').textContent = 'Searching app-private filenames…';
  $('filesList').replaceChildren();
  $('filesPreview').classList.add('hidden');
  $('filesInfo').classList.add('hidden');
  try {
    const result = await api('/api/workspace-search', { query });
    if (workspacePanel !== 'files' || $('filesSearchQuery').value.trim() !== query) return;
    $('filesSearchStatus').textContent = `${result.total} match(es)${result.truncated ? ' · first 40 shown' : ''} · app-private names only`;
    if (!result.results.length) $('filesList').append(el('div', 'tool-empty', 'No matching files or folders.'));
    for (const item of result.results) {
      const row = el('div', 'files-row files-search-result');
      const open = el('button');
      open.type = 'button';
      const copy = el('span');
      copy.append(el('strong', '', item.name), el('small', '', `app://workspace/${item.path}`));
      open.append(uiIcon(item.directory ? 'folder' : 'file'), copy);
      open.addEventListener('click', () => {
        if (item.directory) void listFiles(item.path);
        else void previewPrivateFile(item.path);
      });
      const info = el('button', 'files-row-info', 'Info');
      info.type = 'button';
      info.setAttribute('aria-label', `Properties for ${item.name}`);
      info.addEventListener('click', () => void showPrivateFileProperties(item.path));
      row.append(open, info);
      $('filesList').append(row);
    }
  } catch (error) { $('filesSearchStatus').textContent = String(error.message); }
});
$('filesContentSearchForm').addEventListener('submit', async (event: Event) => {
  event.preventDefault();
  if (!nativeAndroidRuntime || !isTauri() || workspacePanel !== 'files' ||
      $('filesPrivateView').classList.contains('hidden')) return;
  const query = String($('filesContentSearchQuery').value).trim();
  if (!query || new TextEncoder().encode(query).length > 80 ||
      /[\x00-\x1f\x7f\u061c\u200b-\u200f\u202a-\u202e\u2060-\u206f\ufeff]/u.test(query)) {
    $('filesSearchStatus').textContent = 'Use 1–80 bytes of printable text.';
    return;
  }
  const submit = $('filesContentSearchForm').querySelector('button[type="submit"]') as HTMLButtonElement;
  submit.disabled = true;
  $('filesSearchStatus').textContent = 'Searching bounded text in private user-files/ only…';
  $('filesList').replaceChildren();
  $('filesPreview').classList.add('hidden');
  $('filesInfo').classList.add('hidden');
  try {
    const result = await api('/api/workspace-search', {query,mode:'content',userInitiated:true});
    if (workspacePanel !== 'files' || $('filesPrivateView').classList.contains('hidden') ||
        String($('filesContentSearchQuery').value).trim() !== query) return;
    if (result?.readOnly !== true || result.scope !== 'App-private user-files/' ||
        result.query !== query || !Array.isArray(result.results) || result.results.length > 40 ||
        !Number.isSafeInteger(result.total) || result.total < result.results.length ||
        typeof result.truncated !== 'boolean') throw Error('Unverified content search receipt');
    for (const hit of result.results) {
      if (typeof hit.path !== 'string' || !hit.path.startsWith('user-files/') ||
          hit.path.length > 256 || hit.path.split('/').some((part: string) =>
            !part || part === '.' || part === '..' || part.startsWith('.')) ||
          !Number.isSafeInteger(hit.line) || hit.line < 1 || hit.line > 65536 ||
          typeof hit.excerpt !== 'string' || Array.from(hit.excerpt).length > 160 ||
          /[\x00-\x1f\x7f]/.test(hit.excerpt))
        throw Error('Content search returned unsafe file metadata');
    }
    $('filesSearchStatus').textContent =
      `${result.total} verified text match(es)${result.truncated ? ' · partial results (safety limits)' : ''} · user-files/ only`;
    if (!result.results.length)
      $('filesList').append(el('div', 'tool-empty', 'No matching user-owned text files.'));
    for (const hit of result.results) {
      const row = el('div', 'files-row files-search-result');
      const open = el('button');
      open.type = 'button';
      const copy = el('span');
      copy.append(el('strong', '', `${hit.path} · line ${hit.line}`),
        el('small', '', hit.excerpt));
      open.append(uiIcon('file'),copy);
      open.addEventListener('click', () => void previewPrivateFile(hit.path));
      row.append(open);
      $('filesList').append(row);
    }
  } catch (error) {
    if (workspacePanel === 'files') {
      $('filesList').replaceChildren();
      $('filesSearchStatus').textContent = `No content search verified: ${String(error.message).slice(0,180)}`;
    }
  } finally { submit.disabled = false; }
});
$('filesSearchClear').addEventListener('click', () => {
  $('filesSearchQuery').value = '';
  if (workspacePanel === 'files') void listFiles('');
});
function openToolPanel(which: string, userOpenedTerminal = false) {
  if (state.platform !== 'android' && !nativeAndroidRuntime) return;
  if (!['review', 'terminal', 'files', 'subagents'].includes(which)) return;
  if (workspacePanel === 'review') invalidateGoalReviewRequest();
  closeToolMenu();
  closeExtrasPanel();
  closeSidebar();
  setSettings(false);
  workspacePanel = which;
  $('toolViewPanel').classList.remove('hidden');
  $('toolViewPanel').dataset.workspacePanel=which;
  $('androidSubagentsToggle').setAttribute('aria-expanded',String(which==='subagents'));
  $('toolViewTitle').textContent = ({ review: 'Review', terminal: 'Terminal', files: 'Files', subagents: 'Sub-agents' })[which];
  for (const tab of document.querySelectorAll<HTMLButtonElement>('[data-work-tab]'))
    tab.setAttribute('aria-current', String(tab.dataset.workTab === which));
  for (const key of ['Review', 'Terminal', 'Files', 'Subagents'])
    $(`tool${key}`).classList.toggle('hidden', key.toLowerCase() !== which.replace('-', ''));
  if (which === 'review') {
    renderReview();
    void loadGoalReviewProvenance();
    void loadReviewMcpAudit();
  }
  if (which === 'files') switchFilesSource(false);
  if (which === 'subagents') {
    // The legacy centered panel overflows upward as delegated jobs grow.
    // Apply this Android-only CSSOM property (not an inline style attribute,
    // which the app's strict CSP disallows), preserving the panel's styling.
    if (nativeAndroidRuntime) $('toolSubagents').style.justifyContent = 'flex-start';
    void refreshLocalWorkers();
    void refreshMobileAiDelegates();
    void refreshMobilePrime();
    void refreshMobileGoal();
  }
  if (which === 'terminal') {
    void refreshNativeTerminalStatus(userOpenedTerminal);
    if (!nativeAndroidRuntime && !$('terminalOutput').childElementCount)
      terminalLine('', 'App workspace terminal · read-only\nType help for supported commands.');
  }
}
$('toolViewTabs').addEventListener('click', (event: MouseEvent) => {
  const tab = (event.target as Element).closest<HTMLButtonElement>('[data-work-tab]');
  if (tab) openToolPanel(tab.dataset.workTab || '', event.isTrusted);
});

// Tauri Android forwards only these four allowlisted actions to Settings.
// Opening Settings never grants permission or enables an unimplemented tool.
type AndroidPermission = 'files' | 'overlay' | 'notifications' | 'battery';
type AndroidPermissionState = { supported: boolean; granted: boolean };
type AndroidPermissionSnapshot = Record<AndroidPermission, AndroidPermissionState>;
const androidPermissionKeys: AndroidPermission[] = ['files', 'overlay', 'notifications', 'battery'];
type AndroidOptionalPermission = 'camera' | 'microphone' | 'location';
type AndroidOptionalPermissionSnapshot = Record<AndroidOptionalPermission, AndroidPermissionState>;
const androidOptionalPermissionKeys:AndroidOptionalPermission[] = ['camera','microphone','location'];
const androidOptionalPermissionStatusIds:Record<AndroidOptionalPermission,string> = {
  camera:'androidOptionalCameraState',
  microphone:'androidOptionalMicrophoneState',
  location:'androidOptionalLocationState'
};
let androidOptionalPermissionRefresh:Promise<void>|null = null;
let androidOptionalPermissionRequestBusy = false;
async function refreshAndroidOptionalPermissions():Promise<void> {
  if (!nativeAndroidRuntime) return;
  if (androidOptionalPermissionRefresh) return androidOptionalPermissionRefresh;
  const task = (async () => {
    const buttons = [...document.querySelectorAll<HTMLButtonElement>('[data-optional-permission-request]')];
    try {
      if (!isTauri()) throw Error('Android native permission status unavailable');
      const result = await invoke<AndroidOptionalPermissionSnapshot>('android_optional_permissions_status');
      let allowed = 0;
      for (const key of androidOptionalPermissionKeys) {
        const receipt = result?.[key];
        if (!receipt || typeof receipt.supported !== 'boolean' ||
            typeof receipt.granted !== 'boolean' || (!receipt.supported && receipt.granted))
          throw Error('Invalid optional Android permission receipt');
        const status = $(androidOptionalPermissionStatusIds[key]);
        status.dataset.supported = String(receipt.supported);
        status.dataset.granted = String(receipt.supported && receipt.granted);
        status.textContent = !receipt.supported ? 'Not supported' :
          receipt.granted ? 'Allowed' : 'Not allowed';
        if (receipt.supported && receipt.granted) allowed++;
        const button = buttons.find(item => item.dataset.optionalPermissionRequest === key);
        if (button) {
          button.disabled = androidOptionalPermissionRequestBusy || !receipt.supported || receipt.granted;
          button.textContent = !receipt.supported ? 'Not supported' : receipt.granted ? 'Allowed' : 'Allow';
        }
      }
      $('androidOptionalPermissionCount').textContent = `${allowed} of 3 allowed`;
      $('androidOptionalPermissionFeedback').textContent =
        'Optional. Android asks you before granting access.';
    } catch {
      for (const key of androidOptionalPermissionKeys) {
        const node = $(androidOptionalPermissionStatusIds[key]);
        node.textContent = 'Unable to check';
        node.dataset.supported = 'unknown';
        node.dataset.granted = 'false';
      }
      for (const button of buttons) button.disabled = true;
      $('androidOptionalPermissionCount').textContent = 'Unable to check';
      $('androidOptionalPermissionFeedback').textContent =
        'Could not check optional permissions. Return here to retry.';
    }
  })();
  androidOptionalPermissionRefresh = task;
  try { await task; }
  finally {
    if (androidOptionalPermissionRefresh === task)
      androidOptionalPermissionRefresh = null;
  }
}
// Accessibility remains an Android OS-controlled permission. It exposes
// status and a single Allow/Allowed action, never window inspection/actions.
let androidAccessibilityStatusTicket = 0;
let androidAccessibilityRefreshPending = false;
let androidAccessibilityRefreshInFlight:Promise<void>|null = null;
let androidAccessibilityStatusUnavailable = false;
let androidAccessibilityOpenedList = false;
let androidPermissionRefreshPending = false;
let androidPermissionRefreshInFlight:Promise<void>|null = null;
let androidPermissionStatusUnavailable = false;
async function refreshAndroidAccessibility(freshAfterPending = false): Promise<void> {
  if (!nativeAndroidRuntime) return;
  if (androidAccessibilityRefreshInFlight) {
    await androidAccessibilityRefreshInFlight;
    if (!freshAfterPending) return;
  }
  const ticket = ++androidAccessibilityStatusTicket;
  androidAccessibilityRefreshPending = true;
  const request = (async () => {
    const badge = $('accessibilityPermissionState');
    const button = $('accessibilityManage') as HTMLButtonElement;
    const reconnectHint = $('accessibilityReconnectHint');
    try {
      if (!isTauri()) throw Error('Native accessibility unavailable');
      const status = await invoke<{supported:boolean;enabled:boolean;connected:boolean;needsReconnect:boolean}>('accessibility_status');
      if (ticket !== androidAccessibilityStatusTicket) return;
      if (!status || typeof status.supported !== 'boolean' ||
          typeof status.enabled !== 'boolean' || typeof status.connected !== 'boolean' ||
          typeof status.needsReconnect !== 'boolean' ||
          (status.enabled && !status.supported) ||
          (status.connected && !status.enabled) ||
          (status.needsReconnect && (!status.enabled || status.connected)))
        throw Error('Invalid OS Accessibility status');
      androidAccessibilityStatusUnavailable = false;
      // Android controls the AccessibilityService binder. An enabled grant
      // with no current bind is often a transient OS rebinding period, not an
      // instruction to toggle the permission again. Native supervisor allows
      // OS rebinding first; only then will needsReconnect become true.
      const recovering = status.enabled && !status.connected && !status.needsReconnect;
      badge.textContent = !status.supported ? 'Not supported' :
        !status.enabled ? 'Not enabled' : status.connected ? 'Connected' :
          status.needsReconnect ? 'Reconnect needed' : 'Recovering…';
      badge.dataset.supported = String(status.supported);
      badge.dataset.granted = String(status.supported && status.enabled);
      badge.dataset.connected = String(status.supported && status.enabled && status.connected);
      badge.dataset.reconnect = String(status.needsReconnect);
      badge.dataset.recovering = String(recovering);
      button.disabled = !status.supported || (status.enabled && (status.connected || recovering));
      button.textContent = !status.supported ? 'Not supported' :
        status.enabled && status.connected ? 'Allowed' :
        recovering ? 'Checking…' : status.enabled ? 'Reconnect' : 'Allow';
      button.title = recovering
        ? 'Android is restoring the previously allowed Accessibility service; no additional permission action is needed yet'
        : status.enabled && !status.connected
        ? 'Android could not rebind Mcp Agent Ai Accessibility; open system Settings to restore it'
        : 'Open Mcp Agent Ai Accessibility Settings';
      if (status.connected) androidAccessibilityOpenedList = false;
      reconnectHint.hidden = !status.needsReconnect && !androidAccessibilityOpenedList;
      reconnectHint.textContent = status.needsReconnect
        ? 'Android still remembers the permission but has disabled the service binding. Automatic recovery is blocked by Android. Tap Reconnect to restore Mcp Agent Ai Accessibility in system Settings.'
        : 'In Accessibility Settings, open Downloaded apps or Installed services, then Mcp Agent Ai Accessibility.';
    } catch {
      if (ticket !== androidAccessibilityStatusTicket) return;
      androidAccessibilityStatusUnavailable = true;
      badge.textContent = 'Unable to check';
      badge.dataset.supported = 'unknown';
      badge.dataset.granted = 'false';
      badge.dataset.connected = 'false';
      badge.dataset.reconnect = 'false';
      badge.dataset.recovering = 'false';
      button.disabled = true;
      button.textContent = 'Allow';
      reconnectHint.hidden = true;
    } finally {
      if (ticket === androidAccessibilityStatusTicket) {
        androidAccessibilityRefreshPending = false;
        renderAndroidCosSetup();
      }
    }
  })();
  androidAccessibilityRefreshInFlight = request;
  try { await request; }
  finally {
    if (androidAccessibilityRefreshInFlight === request) {
      androidAccessibilityRefreshInFlight = null;
      renderAndroidCosSetup();
    }
  }
}
$('accessibilityManage').addEventListener('click', async () => {
  if (!nativeAndroidRuntime || !isTauri() ||
      ($('accessibilityManage') as HTMLButtonElement).disabled) return;
  const button = $('accessibilityManage') as HTMLButtonElement;
  button.disabled = true;
  try {
    const result = await invoke<{opened:boolean;destination?:string}>('accessibility_request_settings',
      {userInitiated:true});
    if (result?.opened === true && result.destination === 'accessibility_settings')
      androidAccessibilityOpenedList = true;
  } catch {
    // No grant inferred. The OS badge remains the only source of truth.
  } finally {
    void refreshAndroidAccessibility(true);
  }
});
// Android onboarding: native OS permissions, a compact app-owned WebExtension
// manager, then independent ChatGPT login in the owned browser. No Chrome
// extension pairing or implicit installation is claimed.
// Progress is recomputed from native receipts / actual provider responses;
// stepping forward never grants an Android permission or fakes connection.
// Keep Step 1 OS permissions distinct from Step 5 ChatGPT tool Access.
// Steps 6–9 independently check the saved Core ID, keys, AI connection, and
// actual inbound MCP Plugin tool calls. Saving an ID is not a remote tool proof.
const androidCosSetupOrder = ['access','extension','browser','folder','tools','tunnel','key','connect','mcpPlugin','ready'] as const;
type AndroidCosSetupStep = typeof androidCosSetupOrder[number];
type AndroidCosSetupResult = {
  verified:boolean; notNeeded?:boolean; checking?:boolean; unavailable?:boolean; detail:string;
};
function androidCosStepLabel(result:AndroidCosSetupResult):string {
  return result.verified ? 'Verified' : result.notNeeded ? 'Not needed' :
    result.checking ? 'Checking…' : result.unavailable ? 'Unable to check' : 'Pending';
}
type AndroidCosTunnelKind = 'openai' | 'cloudflared' | 'manual';
function androidCosTunnelKind(): AndroidCosTunnelKind {
  const value = ($('androidOpenAiTunnelMethodSelect') as HTMLSelectElement).value;
  return value === 'cloudflared' || value === 'manual' ? value : 'openai';
}
// A previous tab selection is not authoritative after reopening the app:
// native permission, tunnel and MCP receipts may have changed meanwhile.
// Store only a presentation hint, never an authorization or native receipt:
// completed Setup reopens without a blocking 'Checking setup status' overlay
// while OS permissions and the Core tunnel are refreshed in the background.
const androidCosSetupCompletedHintKey = 'mcp-agent-ai-android-setup-completed-hint-v1';
let androidCosSetupPreviouslyComplete = false;
if (nativeAndroidRuntime) {
  try {
    androidCosSetupPreviouslyComplete =
      localStorage.getItem(androidCosSetupCompletedHintKey) === '1';
  } catch { /* UI hint is optional and untrusted. */ }
}
// The Setup scanner still chooses the first unresolved stage from fresh OS
// receipts at the end of each refresh, regardless of this presentation hint.
let androidCosSetupSelected: AndroidCosSetupStep = 'access';
let androidCosSetupScanEpoch = 0;
let androidCosSetupScanInFlight = false;
let androidCosSetupQuietScan = false;
let androidCosSetupHeldVerifiedResults: readonly AndroidCosSetupResult[]|null = null;
let androidCosSetupUserSelectedDuringScan = false;
// Keep following native status changes only for a stage picked automatically
// when Setup opened. Explicit navigation remains under the user's control.
let androidCosSetupAutoFollow = false;
const androidCosMcpPhoneStorageKey = 'mcp-agent-ai-android-core-plugin-phone-v1';
function validAndroidCosMcpPhoneName(value:string):boolean {
  return value === '' || /^[A-Za-z0-9][A-Za-z0-9 ._-]{0,31}$/.test(value);
}
if (nativeAndroidRuntime) {
  let storedPhoneName = '';
  try {
    const saved = localStorage.getItem(androidCosMcpPhoneStorageKey);
    if (saved && validAndroidCosMcpPhoneName(saved)) storedPhoneName = saved;
  } catch { /* Private device preference is optional. */ }
  ($('androidCosMcpPhoneName') as HTMLInputElement).value = storedPhoneName;
}
function renderAndroidCosMcpPluginIdentity():void {
  const input = $('androidCosMcpPhoneName') as HTMLInputElement;
  const label = input.value.trim();
  const valid = validAndroidCosMcpPhoneName(label);
  input.setAttribute('aria-invalid',String(!valid));
  $('androidCosMcpPhoneError').hidden = valid;
  ($('androidCosMcpName') as HTMLInputElement).value =
    valid ? (label ? `Mcp Agent Ai Core (${label})` : 'Mcp Agent Ai Core') : '';
  ($('androidCosMcpCopyName') as HTMLButtonElement).disabled = !valid;
  ($('androidCosMcpCopyDescription') as HTMLButtonElement).disabled = !valid;
}
let androidCosMcpOpenBusy = false;
// Advance only after Step 1 was actually incomplete and all five grants become
// ready. Revisiting an already-complete Step 1 must allow Optional expansion.
let androidCosAccessAutoAdvanceArmed = false;
type AndroidGeckoExtension = {
  id:string; name:string; version:string; enabled:boolean; builtIn:boolean;
  requiredPermissions:string[]; requiredOrigins:string[];
};
const androidCosProbeExtensionId = 'mcp-agent-browser-probe@example.com';
let androidCosExtensions: AndroidGeckoExtension[] | null = null;
let androidCosExtensionBusy = false;
let androidCosExtensionRefreshEpoch = 0;
let androidCosExtensionsUnavailable = false;
let androidCosExtensionFeedback = '';
// Step 3 uses OpenAI's native Sign in with ChatGPT OAuth/PKCE and explicit
// Mcp Agent Ai approval in the system browser. This authenticates the app,
// NOT the unrelated ChatGPT website WebView profile or MCP tunnel.
type AndroidChatgptOAuthStatus = {
  supported:boolean;
  state:'signed_out'|'authorizing'|'verified'|'linked'|'error';
  signInVerified:boolean;
  sessionLinked:boolean;
  planEnabled:boolean;
  message:string;
  pending:boolean;
};
let androidCosOAuthStatus:AndroidChatgptOAuthStatus|null = null;
let androidCosOAuthBusy = false;
let androidCosOAuthRefreshInFlight:Promise<void>|null = null;
let androidCosOAuthEpoch = 0;
let androidCosOAuthUnavailable = false;
let androidCosOAuthFeedback = '';
function validateAndroidCosOAuthStatus(receipt:unknown):AndroidChatgptOAuthStatus {
  if (!receipt || typeof receipt !== 'object') throw Error('Invalid ChatGPT authorization status');
  const value = receipt as Record<string,unknown>;
  if (Object.keys(value).sort().join(',') !==
        'message,pending,planEnabled,sessionLinked,signInVerified,state,supported' ||
      typeof value.supported !== 'boolean' || typeof value.signInVerified !== 'boolean' ||
      typeof value.sessionLinked !== 'boolean' || typeof value.planEnabled !== 'boolean' ||
      typeof value.pending !== 'boolean' || typeof value.message !== 'string' ||
      value.message.length > 500 ||
      !['signed_out','authorizing','verified','linked','error'].includes(String(value.state)) ||
      (value.state === 'linked' && (!value.supported || !value.signInVerified ||
        !value.sessionLinked || !value.planEnabled || value.pending)) ||
      (value.state === 'verified' && (!value.supported || !value.signInVerified ||
        value.sessionLinked || value.pending)) ||
      (value.state !== 'linked' && value.state !== 'verified' &&
        (value.signInVerified || value.sessionLinked)) ||
      (value.state === 'authorizing' && !value.pending) ||
      (value.state !== 'authorizing' && value.pending))
    throw Error('Inconsistent ChatGPT authorization receipt');
  return value as AndroidChatgptOAuthStatus;
}
function androidCosOAuthLinked():boolean {
  const status = androidCosOAuthStatus;
  return status?.supported === true && status.state === 'linked' &&
    status.signInVerified === true && status.sessionLinked === true && status.planEnabled === true;
}
function androidCosOAuthReadyToBring():boolean {
  const status = androidCosOAuthStatus;
  return status?.supported === true && status.state === 'verified' &&
    status.signInVerified === true && status.planEnabled === true && !status.pending;
}
async function refreshAndroidCosOAuthStatus(freshAfterPending = false):Promise<void> {
  if (!nativeAndroidRuntime || !isTauri()) return;
  if (androidCosOAuthRefreshInFlight) {
    await androidCosOAuthRefreshInFlight;
    if (!freshAfterPending) return;
  }
  const epoch = ++androidCosOAuthEpoch;
  const request = (async () => {
    try {
      const status = validateAndroidCosOAuthStatus(
        await invoke<unknown>('android_chatgpt_oauth_status'));
      if (epoch === androidCosOAuthEpoch) {
        androidCosOAuthStatus = status;
        androidCosOAuthUnavailable = false;
        if (status.state === 'linked' || status.state === 'verified' || status.state === 'signed_out')
          androidCosOAuthFeedback = '';
      }
    } catch {
      if (epoch === androidCosOAuthEpoch) {
        androidCosOAuthStatus = null;
        androidCosOAuthUnavailable = true;
        androidCosOAuthFeedback = 'Unable to check ChatGPT app authorization. Retry Sign in to ChatGPT.';
      }
    } finally {
      if (epoch === androidCosOAuthEpoch) renderAndroidCosSetup();
    }
  })();
  androidCosOAuthRefreshInFlight = request;
  try { await request; }
  finally {
    if (androidCosOAuthRefreshInFlight === request) {
      androidCosOAuthRefreshInFlight = null;
      renderAndroidCosSetup();
    }
  }
}
// Android Setup requires the Core MCP Plugin to be confirmed by an actual
// acknowledged tool response. Local chat may still function independently.
type AndroidOpenAiTunnelState = 'disconnected' | 'connecting' | 'polling' | 'tool_verified' | 'error';
type AndroidTunnelHealth = {
  verifiedLinkAtMs:number|null;
  lastChatgptCallAtMs:number|null;
  pollErrors:number;
  uptimeSeconds:number|null;
  route:string|null;
  tunnelClient:string;
  localServer:string;
  probe?:string|null;
  toolCount?:number|null;
};
type AndroidTunnelEvent = {
  timestampMs:number;
  kind:'tunnel'|'request'|'tool'|'error';
  detail:string;
};
type AndroidOpenAiTunnelSnapshot = {
  state: AndroidOpenAiTunnelState;
  pollActive: boolean;
  toolCallsVerified: number;
  // Native-only historical pairing: an OpenAI-acknowledged MCP tool response
  // for this Core tunnel, recognized again only while its live poll is healthy.
  corePluginLinked?: boolean;
  desiredConnected?: boolean;
  lastError?: string;
  health?:AndroidTunnelHealth;
  events?:AndroidTunnelEvent[];
};
let androidOpenAiTunnelSnapshot: AndroidOpenAiTunnelSnapshot | null = null;
let androidOpenAiTunnelBusy = false;
let androidOpenAiTunnelTicket = 0;
let androidOpenAiTunnelRefreshInFlight:Promise<void>|null = null;
let androidOpenAiTunnelUnavailable = false;
let androidOpenAiTunnelFeedback = '';
// The native Android Keystore is the only credential store. The renderer
// receives presence metadata, never decrypts a previously saved API key.
type AndroidTunnelCredentialsStatus = {stored:boolean;secure:true};
let androidTunnelCredentials:AndroidTunnelCredentialsStatus|null = null;
let androidTunnelCredentialsUnavailable = false;
let androidTunnelCredentialsBusy = false;
let androidTunnelCredentialsFeedback = '';
let androidTunnelCredentialsEpoch = 0;
let androidTunnelCredentialsInFlight:Promise<void>|null = null;
type AndroidTunnelIdCheck = {valid:boolean;formatOnly:true};
let androidTunnelValidatedId = '';
let androidTunnelIdChecked = false;
let androidTunnelIdBusy = false;
let androidTunnelIdCheckTicket = 0;
// A valid ID shape is not a tunnel record or workspace authorization. Only
// the explicit Save action obtains a fresh, authenticated metadata receipt.
type AndroidTunnelVerifyReason =
  'ok'|'invalid_id'|'key_required'|'unauthorized'|'not_found'|
  'network_error'|'invalid_metadata'|'service_error';
type AndroidTunnelVerifyReceipt = {
  verified:boolean;saved:boolean;reason:AndroidTunnelVerifyReason;
};
type AndroidTunnelDraftSaveReceipt = {tunnelId:string;saved:true;verified:false};
let androidTunnelSaveBusy = false;
let androidTunnelSaveReason:AndroidTunnelVerifyReason|null = null;
let androidTunnelMetadataVerifiedId = '';
let androidTunnelVerifyBusy = false;
let androidTunnelSavedDraftId:string|null = null;
let androidTunnelSavedIdEpoch = 0;
let androidTunnelSavedIdInFlight:Promise<void>|null = null;
function validateAndroidTunnelVerifyReceipt(value:unknown):AndroidTunnelVerifyReceipt {
  if (!value || typeof value !== 'object' || Array.isArray(value))
    throw Error('Invalid authenticated tunnel result');
  const result = value as Record<string,unknown>;
  const reason = result.reason;
  if (Object.keys(result).sort().join(',') !== 'reason,saved,verified' ||
    typeof result.verified !== 'boolean' || typeof result.saved !== 'boolean' ||
    !['ok','invalid_id','key_required','unauthorized','not_found',
      'network_error','invalid_metadata','service_error'].includes(String(reason)) ||
    (result.verified !== true && result.saved !== false) ||
    (result.verified === true && (result.saved !== true || reason !== 'ok')) ||
    (result.verified === false && reason === 'ok'))
    throw Error('Inconsistent authenticated tunnel result');
  return result as AndroidTunnelVerifyReceipt;
}
function validateAndroidTunnelDraftSaveReceipt(value:unknown,tunnelId:string):AndroidTunnelDraftSaveReceipt {
  if (!value || typeof value !== 'object' || Array.isArray(value))
    throw Error('Invalid Core tunnel draft receipt');
  const item = value as Record<string,unknown>;
  if (Object.keys(item).sort().join(',') !== 'saved,tunnelId,verified' ||
      item.saved !== true || item.verified !== false || item.tunnelId !== tunnelId)
    throw Error('Backend did not confirm saving an unverified Core tunnel ID');
  return item as AndroidTunnelDraftSaveReceipt;
}
async function saveAndroidCoreTunnelId(tunnelId:string):Promise<void> {
  if (!isTauri() || androidCosSetupSelected !== 'tunnel' || androidTunnelSaveBusy ||
      !validTunnelIdShape(tunnelId) || androidTunnelIdBusy ||
      !androidTunnelIdChecked || androidTunnelValidatedId !== tunnelId) return;
  androidTunnelSaveBusy = true;
  androidTunnelSaveReason = null;
  androidTunnelMetadataVerifiedId = '';
  renderAndroidCosSetup();
  try {
    const receipt = validateAndroidTunnelDraftSaveReceipt(await invoke<unknown>(
      'android_tunnel_save_id',{tunnelId,userInitiated:true}),tunnelId);
    if (($('androidOpenAiTunnelId') as HTMLInputElement).value.trim() !== tunnelId)
      return; // Ignore stale UI if the user changed the field during Save.
    if (!receipt.saved || receipt.verified) return;
    // A status read started before Save may still be in flight. Invalidate
    // that older read so it cannot replace the newly saved ID with stale data.
    ++androidTunnelSavedIdEpoch;
    androidTunnelSavedDraftId = tunnelId;
    // Step 6 is ID enrollment ONLY. Never ask for a runtime key or claim
    // that OpenAI has verified an account when simply saving an identifier.
    chooseAndroidCosSetupStep('key');
  } catch {
    androidTunnelSaveReason = 'service_error';
  } finally {
    androidTunnelSaveBusy = false;
    renderAndroidCosSetup();
  }
}
async function verifyAndroidSavedCoreTunnel():Promise<void> {
  if (!isTauri() || !['key','connect'].includes(androidCosSetupSelected) ||
      androidTunnelSaveBusy || androidTunnelVerifyBusy || androidTunnelCredentialsBusy)
    return;
  const tunnelId = androidTunnelSavedDraftId;
  if (!tunnelId || !validTunnelIdShape(tunnelId) ||
      ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim() !== tunnelId) {
    androidTunnelSaveReason = 'invalid_id';
    renderAndroidCosSetup();
    return;
  }
  androidTunnelVerifyBusy = true;
  androidTunnelMetadataVerifiedId = '';
  androidTunnelSaveReason = null;
  renderAndroidCosSetup();
  try {
    const receipt = validateAndroidTunnelVerifyReceipt(await invoke<unknown>(
      'android_tunnel_verify_and_save',{tunnelId,userInitiated:true}));
    // A reply for a different ID must not authorize this session.
    if (androidTunnelSavedDraftId !== tunnelId ||
        ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim() !== tunnelId) return;
    androidTunnelSaveReason = receipt.reason;
    if (!receipt.verified || !receipt.saved) return;
    androidTunnelMetadataVerifiedId = tunnelId;
    try {
      // When a credential is replaced during Step 7, retire any old tunnel.
      // Rechecking an unchanged stored key when entering Step 8 must not
      // silently disconnect an otherwise running native transport.
      if (androidCosSetupSelected === 'key' && androidOpenAiTunnelSnapshot &&
          ['connecting','polling','tool_verified','error'].includes(androidOpenAiTunnelSnapshot.state))
        await invoke('android_openai_tunnel_disconnect',{userInitiated:true});
      await refreshAndroidOpenAiTunnel(true);
    } catch {
      // Keep metadata authorization, but Step 8 still checks the native status
      // before offering a fresh connection. No remote tool proof is inferred.
    }
  } catch {
    androidTunnelMetadataVerifiedId = '';
    androidTunnelSaveReason = 'service_error';
  } finally {
    androidTunnelVerifyBusy = false;
    renderAndroidCosSetup();
  }
}
async function refreshAndroidTunnelSavedId():Promise<void> {
  if (!nativeAndroidRuntime || !isTauri()) return;
  if (androidTunnelSavedIdInFlight) return androidTunnelSavedIdInFlight;
  const epoch = ++androidTunnelSavedIdEpoch;
  const originalInput = ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim();
  const task = (async () => {
    try {
      const value = await invoke<unknown>('android_tunnel_saved_id_status');
      if (epoch !== androidTunnelSavedIdEpoch) return;
      if (!value || typeof value !== 'object' || Array.isArray(value)) throw Error('Invalid ID status');
      const receipt = value as Record<string,unknown>;
      if (Object.keys(receipt).sort().join(',') !== 'tunnelId,verified' ||
        receipt.verified !== false ||
        !(receipt.tunnelId === null || typeof receipt.tunnelId === 'string') ||
        (typeof receipt.tunnelId === 'string' && !validTunnelIdShape(receipt.tunnelId)))
        throw Error('Unverified native ID receipt');
      androidTunnelSavedDraftId = receipt.tunnelId as string|null;
      const input = $('androidOpenAiTunnelId') as HTMLInputElement;
      if (receipt.tunnelId && !originalInput && !input.value.trim()) {
        input.value = receipt.tunnelId;
        void checkAndroidTunnelId();
      }
    } catch {
      if (epoch === androidTunnelSavedIdEpoch) androidTunnelSavedDraftId = null;
    } finally {
      if (epoch === androidTunnelSavedIdEpoch) renderAndroidCosSetup();
    }
  })();
  androidTunnelSavedIdInFlight = task;
  try { await task; }
  finally {
    if (androidTunnelSavedIdInFlight === task) androidTunnelSavedIdInFlight = null;
  }
}
function validateAndroidTunnelCredentials(value:unknown):AndroidTunnelCredentialsStatus {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw Error('Invalid credential status');
  const receipt = value as Record<string,unknown>;
  if (Object.keys(receipt).sort().join(',') !== 'secure,stored' ||
      receipt.secure !== true || typeof receipt.stored !== 'boolean')
    throw Error('Native tunnel credential status was not secure');
  return receipt as AndroidTunnelCredentialsStatus;
}
async function refreshAndroidTunnelCredentials():Promise<void> {
  if (!nativeAndroidRuntime || !isTauri()) return;
  if (androidTunnelCredentialsInFlight) return androidTunnelCredentialsInFlight;
  const epoch = ++androidTunnelCredentialsEpoch;
  const request = (async () => {
    try {
      const receipt = validateAndroidTunnelCredentials(
        await invoke<unknown>('android_tunnel_credentials_status'));
      if (epoch !== androidTunnelCredentialsEpoch) return;
      androidTunnelCredentials = receipt;
      androidTunnelCredentialsUnavailable = false;
    } catch {
      if (epoch !== androidTunnelCredentialsEpoch) return;
      androidTunnelCredentials = null;
      androidTunnelCredentialsUnavailable = true;
    } finally {
      if (epoch === androidTunnelCredentialsEpoch) renderAndroidCosSetup();
    }
  })();
  androidTunnelCredentialsInFlight = request;
  try { await request; }
  finally {
    if (androidTunnelCredentialsInFlight === request) androidTunnelCredentialsInFlight = null;
    renderAndroidCosSetup();
  }
}
async function checkSavedTunnelKeyOnStep7():Promise<void> {
  // A stored key is never read back into the renderer. Opening Step 7 or
  // revisiting Step 8 after restart loads only the secure-storage receipt,
  // then checks OpenAI access. No plaintext key recovery is required.
  await Promise.all([refreshAndroidTunnelSavedId(),refreshAndroidTunnelCredentials()]);
  if (!['key','connect'].includes(androidCosSetupSelected) || !androidTunnelSavedDraftId ||
      !androidTunnelCredentials?.stored ||
      !androidTunnelCredentials.secure || androidTunnelCredentialsBusy ||
      androidTunnelVerifyBusy ||
      androidTunnelMetadataVerifiedId === androidTunnelSavedDraftId) return;
  await verifyAndroidSavedCoreTunnel();
}
// Read-only backend authorization check used when Setup opens after process
// restart. The key is decrypted only in native code and never returned to JS.
// This does not connect a tunnel or verify any inbound MCP tool call.
let androidTunnelStoredAccessInFlight:Promise<void>|null = null;
async function refreshAndroidTunnelStoredAccess():Promise<void> {
  if (!nativeAndroidRuntime || !isTauri()) return;
  if (androidTunnelStoredAccessInFlight) return androidTunnelStoredAccessInFlight;
  const task = (async () => {
    await Promise.all([refreshAndroidTunnelSavedId(),refreshAndroidTunnelCredentials()]);
    const expectedId = androidTunnelSavedDraftId;
    if (!expectedId || androidTunnelCredentials?.stored !== true ||
        androidTunnelCredentials.secure !== true) {
      androidTunnelMetadataVerifiedId = '';
      renderAndroidCosSetup();
      return;
    }
    try {
      const raw = await invoke<unknown>('android_tunnel_verify_stored_access');
      if (!raw || typeof raw !== 'object' || Array.isArray(raw))
        throw Error('Invalid native tunnel access status');
      const receipt = raw as Record<string,unknown>;
      const reasons = ['ok','id_required','id_changed','key_required','unauthorized','not_found',
        'network_error','invalid_metadata','service_error'];
      if (Object.keys(receipt).sort().join(',') !== 'reason,tunnelId,verified' ||
          typeof receipt.verified !== 'boolean' ||
          !reasons.includes(String(receipt.reason)) ||
          !(receipt.tunnelId === null || typeof receipt.tunnelId === 'string') ||
          (receipt.verified && (receipt.reason !== 'ok' || receipt.tunnelId !== expectedId)) ||
          (!receipt.verified && receipt.reason === 'ok'))
        throw Error('Untrusted native authorization receipt');
      if (expectedId !== androidTunnelSavedDraftId) return;
      androidTunnelMetadataVerifiedId = receipt.verified ? expectedId : '';
      androidTunnelSaveReason = receipt.verified ? null :
        (receipt.reason === 'id_required' || receipt.reason === 'id_changed'
          ? 'invalid_id' : receipt.reason as AndroidTunnelVerifyReason);
    } catch {
      if (expectedId !== androidTunnelSavedDraftId) return;
      androidTunnelMetadataVerifiedId = '';
      androidTunnelSaveReason = 'network_error';
    } finally {
      renderAndroidCosSetup();
    }
  })();
  androidTunnelStoredAccessInFlight = task;
  try { await task; }
  finally {
    if (androidTunnelStoredAccessInFlight === task)
      androidTunnelStoredAccessInFlight = null;
  }
}
function validTunnelIdShape(id:string):boolean {
  return /^tunnel_(?:[a-z0-9]{4}_)?[a-z0-9]{32}$/.test(id);
}
async function checkAndroidTunnelId():Promise<void> {
  if (!nativeAndroidRuntime || !isTauri()) return;
  const id = ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim();
  const ticket = ++androidTunnelIdCheckTicket;
  androidTunnelValidatedId = '';
  androidTunnelIdChecked = false;
  if (!validTunnelIdShape(id)) {
    androidTunnelIdBusy = false;
    renderAndroidCosSetup();
    return;
  }
  androidTunnelIdBusy = true;
  renderAndroidCosSetup();
  try {
    const receipt = await invoke<unknown>('android_tunnel_check_id',{tunnelId:id});
    if (ticket !== androidTunnelIdCheckTicket ||
        ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim() !== id) return;
    const check = receipt as Partial<AndroidTunnelIdCheck>|null;
    androidTunnelIdChecked = check?.valid === true && check.formatOnly === true &&
      !!check && Object.keys(check).sort().join(',') === 'formatOnly,valid';
    if (androidTunnelIdChecked) {
      androidTunnelValidatedId = id;
    }
  } catch {
    if (ticket !== androidTunnelIdCheckTicket) return;
    androidTunnelIdChecked = false;
  } finally {
    if (ticket === androidTunnelIdCheckTicket) {
      androidTunnelIdBusy = false;
      renderAndroidCosSetup();
    }
  }
}
// Step 5 has COS-inspired permission choices. A confirmed choice completes
// onboarding; actual tool availability is an independent Rust authorization
// decision (four read-only file tools exist; writes/browser/shell stay gated).
const androidMcpReadCapabilities = ['browseFolders','searchFiles','readFiles','fileMetadata'] as const;
const androidMcpWriteCapabilities = ['createFiles','editFiles','moveFiles','deleteFiles'] as const;
const androidMcpCapabilityNames = [
  ...androidMcpReadCapabilities,...androidMcpWriteCapabilities,'browserControl','terminal'
] as const;
type AndroidMcpCapability = typeof androidMcpCapabilityNames[number];
type AndroidMcpPreset = 'read_only'|'files_terminal'|'full'|'custom';
type AndroidMcpCapabilityMap = Record<AndroidMcpCapability,boolean>;
type AndroidMcpAccessStatus = {
  preset:AndroidMcpPreset;
  requested:AndroidMcpCapabilityMap;
  effective:AndroidMcpCapabilityMap;
  available:AndroidMcpCapabilityMap;
};
let androidMcpAccessSnapshot:AndroidMcpAccessStatus|null = null;
let androidMcpAccessBusy = false;
let androidMcpAccessUnavailable = false;
let androidMcpAccessFeedback = '';
let androidMcpAccessRefreshEpoch = 0;
let androidMcpAccessRefreshInFlight:Promise<void>|null = null;
// This editor changes only native command policy. It never starts a shell or
// executes the text in the textarea. No native receipt means no usable rules.
const androidMcpSafeCommandPatterns = ['pwd','ls','stat *','cat *'] as const;
type AndroidMcpCommandRulesMode = 'allowlist'|'denylist';
type AndroidMcpCommandRulesReceipt = {
  enabled:boolean;
  mode:AndroidMcpCommandRulesMode;
  rules:string[];
  available:boolean;
  supportedCommands:string[];
  allowedCommands?:string[];
};
let androidMcpCommandRules:AndroidMcpCommandRulesReceipt|null = null;
let androidMcpCommandRulesBusy = false;
let androidMcpCommandRulesRefreshing = false;
let androidMcpCommandRulesFeedback = '';
let androidMcpCommandRulesEpoch = 0;
function validateAndroidMcpCommandRules(value:unknown):AndroidMcpCommandRulesReceipt {
  if (!value || typeof value !== 'object' || Array.isArray(value))
    throw Error('Native command rules unavailable');
  const data = value as Record<string,unknown>;
  const safeCommands = ['pwd','ls','stat','cat'];
  const validCommands = (list:unknown):list is string[] =>
    Array.isArray(list) && list.length <= 4 && new Set(list).size === list.length &&
    list.every(item => typeof item === 'string' && safeCommands.includes(item));
  if (typeof data.enabled !== 'boolean' ||
      !['allowlist','denylist'].includes(String(data.mode)) ||
      typeof data.available !== 'boolean' ||
      !Array.isArray(data.rules) || data.rules.length > 4 ||
      new Set(data.rules).size !== data.rules.length ||
      !data.rules.every(rule => typeof rule === 'string' &&
        (androidMcpSafeCommandPatterns as readonly string[]).includes(rule)) ||
      !validCommands(data.supportedCommands) ||
      (data.allowedCommands !== undefined && !validCommands(data.allowedCommands)))
    throw Error('Invalid native command rules');
  const supported = data.supportedCommands as string[];
  if (data.rules.some(rule => !supported.includes(rule.split(' ')[0])))
    throw Error('Rule not supported by native terminal');
  return data as AndroidMcpCommandRulesReceipt;
}
function androidMcpCommandRuleLines(input:string):string[]|null {
  if (input.length > 256) return null;
  const lines = input.split(/\r?\n/).map(line => line.trim()).filter(Boolean);
  const supported = androidMcpCommandRules?.supportedCommands || [];
  if (lines.length > 4 || new Set(lines).size !== lines.length ||
      lines.some(rule => !(androidMcpSafeCommandPatterns as readonly string[]).includes(rule) ||
        !supported.includes(rule.split(' ')[0])))
    return null;
  return lines;
}
function androidMcpCommandRulesForm():{enabled:boolean;mode:AndroidMcpCommandRulesMode;rules:string[]}|null {
  const input = $('androidMcpCommandRulesInput') as HTMLTextAreaElement;
  const rules = androidMcpCommandRuleLines(input.value);
  const mode = document.querySelector<HTMLInputElement>(
    'input[name="androidMcpCommandRulesMode"]:checked')?.value;
  if (!rules || (mode !== 'allowlist' && mode !== 'denylist') ||
      (($('androidMcpCommandRulesEnabled') as HTMLInputElement).checked && mode === 'allowlist' &&
        rules.length === 0))
    return null;
  return {enabled:($('androidMcpCommandRulesEnabled') as HTMLInputElement).checked,mode,rules};
}
function renderAndroidMcpFileTools():void {
  const policy = androidMcpAccessSnapshot;
  for (const node of document.querySelectorAll<HTMLElement>('.android-mcp-file-tool[data-capability]')) {
    const capability = node.dataset.capability as AndroidMcpCapability|undefined;
    const state = !policy || androidMcpAccessUnavailable || androidMcpAccessBusy || !capability
      ? 'checking'
      : !policy.available[capability] ? 'unavailable'
      : policy.effective[capability] ? 'available' : 'not_enabled';
    node.dataset.state = state;
    const status = node.querySelector('small');
    if (status) status.textContent = state === 'available' ? 'Available' :
      state === 'unavailable' ? 'Unavailable' : state === 'not_enabled' ? 'Not enabled' :
      'Checking…';
  }
}
function renderAndroidMcpCommandRules():void {
  if (!nativeAndroidRuntime) return;
  const receipt = androidMcpCommandRules;
  const available = !!receipt?.available && !androidMcpCommandRulesBusy &&
    !androidMcpCommandRulesRefreshing && isTauri();
  const enabled = $('androidMcpCommandRulesEnabled') as HTMLInputElement;
  const modes = $('androidMcpCommandRulesModes') as HTMLFieldSetElement;
  const input = $('androidMcpCommandRulesInput') as HTMLTextAreaElement;
  const save = $('androidMcpCommandRulesSave') as HTMLButtonElement;
  enabled.disabled = modes.disabled = input.disabled = !available;
  const draft = androidMcpCommandRulesForm();
  const invalid = available && !draft;
  const changed = !!draft && !!receipt &&
    (draft.enabled !== receipt.enabled || draft.mode !== receipt.mode ||
      draft.rules.join('\n') !== receipt.rules.join('\n'));
  save.disabled = !available || !draft || !changed;
  const status = $('androidMcpCommandRulesStatus');
  status.dataset.state = androidMcpCommandRulesFeedback ? 'error' :
    androidMcpCommandRulesBusy || androidMcpCommandRulesRefreshing ? 'checking' :
      receipt?.available && !invalid && !changed ? 'saved' : 'pending';
  const terminalEffective = androidMcpAccessSnapshot?.effective.terminal === true;
  status.textContent = androidMcpCommandRulesFeedback ||
    (androidMcpCommandRulesBusy ? 'Saving and verifying rules with native Android…' :
    androidMcpCommandRulesRefreshing ? 'Checking native command rules… No command access assumed.' :
    !receipt ? 'Native command rules not verified. Remote terminal tools stay unavailable.' :
    !receipt.available ? 'Native command policy is unavailable. Remote terminal tools stay blocked.' :
    invalid ? 'Invalid rules. Use only pwd, ls, stat * or cat * (once per line); an enabled Allowlist must not be empty.' :
    changed ? 'Unsaved command rules. Press Save to verify with native Android; no command was launched.' :
    !receipt.enabled ? 'Command rule limiting is disabled. No remote command access is assumed.' :
    !terminalEffective ? 'Rules saved. Terminal tool access is not effective; native permission and supported routes are still required.' :
    'Native rules verified. Only supported commands permitted by both these rules and Android terminal access can run.');
}
function paintAndroidMcpCommandRulesReceipt(receipt:AndroidMcpCommandRulesReceipt):void {
  ($('androidMcpCommandRulesEnabled') as HTMLInputElement).checked = receipt.enabled;
  for (const radio of document.querySelectorAll<HTMLInputElement>(
    'input[name="androidMcpCommandRulesMode"]'))
    radio.checked = radio.value === receipt.mode;
  ($('androidMcpCommandRulesInput') as HTMLTextAreaElement).value = receipt.rules.join('\n');
  renderAndroidMcpCommandRules();
}
async function refreshAndroidMcpCommandRules():Promise<void> {
  if (!nativeAndroidRuntime || !isTauri() || androidMcpCommandRulesBusy) return;
  const epoch = ++androidMcpCommandRulesEpoch;
  androidMcpCommandRulesRefreshing = true;
  androidMcpCommandRulesFeedback = '';
  renderAndroidMcpCommandRules();
  try {
    const receipt = validateAndroidMcpCommandRules(
      await invoke<unknown>('android_mcp_command_rules_status'));
    if (epoch !== androidMcpCommandRulesEpoch) return;
    androidMcpCommandRules = receipt;
    paintAndroidMcpCommandRulesReceipt(receipt);
  } catch {
    if (epoch !== androidMcpCommandRulesEpoch) return;
    androidMcpCommandRules = null;
    androidMcpCommandRulesFeedback = 'Native command rules could not be verified. No remote terminal access is assumed.';
  } finally {
    if (epoch === androidMcpCommandRulesEpoch) {
      androidMcpCommandRulesRefreshing = false;
      renderAndroidMcpCommandRules();
    }
  }
}
async function saveAndroidMcpCommandRules():Promise<void> {
  if (!nativeAndroidRuntime || !isTauri() || androidMcpCommandRulesBusy ||
      androidMcpCommandRulesRefreshing || androidMcpCommandRules?.available !== true) return;
  const desired = androidMcpCommandRulesForm();
  if (!desired) return;
  const epoch = ++androidMcpCommandRulesEpoch;
  androidMcpCommandRulesBusy = true;
  androidMcpCommandRulesFeedback = '';
  renderAndroidMcpCommandRules();
  try {
    // A human Save button is the only path to this native policy setter. The
    // textarea is never interpreted as a command and never sent to a terminal.
    await invoke('android_mcp_command_rules_set',{...desired,userInitiated:true});
    const confirmed = validateAndroidMcpCommandRules(
      await invoke<unknown>('android_mcp_command_rules_status'));
    if (epoch !== androidMcpCommandRulesEpoch) return;
    if (!confirmed.available || confirmed.enabled !== desired.enabled ||
        confirmed.mode !== desired.mode || confirmed.rules.join('\n') !== desired.rules.join('\n'))
      throw Error('Native command rules did not match saved request');
    androidMcpCommandRules = confirmed;
    paintAndroidMcpCommandRulesReceipt(confirmed);
    // Native effective policy may change when explicit command rules change.
    void refreshAndroidMcpAccess();
  } catch {
    if (epoch !== androidMcpCommandRulesEpoch) return;
    androidMcpCommandRules = null; // Never keep stale rule authority.
    androidMcpCommandRulesFeedback = 'Save was not verified by native Android. Command tools remain unavailable until a fresh status check.';
  } finally {
    if (epoch === androidMcpCommandRulesEpoch) {
      androidMcpCommandRulesBusy = false;
      renderAndroidMcpCommandRules();
    }
  }
}
function androidMcpPresetMask(preset:AndroidMcpPreset):AndroidMcpCapabilityMap {
  const result = {} as AndroidMcpCapabilityMap;
  for (const key of androidMcpCapabilityNames) {
    result[key] = preset === 'full' ||
      (preset === 'files_terminal' && key !== 'browserControl') ||
      (preset === 'read_only' && (androidMcpReadCapabilities as readonly string[]).includes(key));
  }
  return result;
}
function validateAndroidMcpAccessSnapshot(value:unknown):AndroidMcpAccessStatus {
  if (!value || typeof value !== 'object') throw Error('Invalid native MCP tool permissions');
  const result = value as Record<string,unknown>;
  if (Object.keys(result).sort().join(',') !== 'available,effective,preset,requested' ||
      !['read_only','files_terminal','full','custom'].includes(String(result.preset)))
    throw Error('Invalid MCP permission receipt');
  const maps = ['requested','effective','available'] as const;
  for (const name of maps) {
    const map = result[name];
    if (!map || typeof map !== 'object' || Array.isArray(map) ||
        Object.keys(map).sort().join(',') !== [...androidMcpCapabilityNames].sort().join(',') ||
        androidMcpCapabilityNames.some(key => typeof (map as Record<string,unknown>)[key] !== 'boolean'))
      throw Error('Invalid native MCP capability mask');
  }
  const status = result as AndroidMcpAccessStatus;
  if (androidMcpCapabilityNames.some(key => status.effective[key] &&
    (!status.requested[key] || !status.available[key])))
    throw Error('Native MCP permission receipt conflicts with available capabilities');
  const expected = androidMcpPresetMask(status.preset);
  const emptyDefault = status.preset === 'read_only' &&
    androidMcpCapabilityNames.every(key => !status.requested[key]);
  if (status.preset !== 'custom' && !emptyDefault &&
      androidMcpCapabilityNames.some(key => status.requested[key] !== expected[key]))
    throw Error('Native MCP preset does not match its permissions');
  return status;
}
function renderAndroidMcpAccess():void {
  if (!nativeAndroidRuntime) return;
  const statusNode = document.getElementById('androidMcpAccessStatus');
  if (!statusNode) return;
  const report = androidMcpAccessSnapshot;
  const disabled = androidMcpAccessBusy || !isTauri() || !report;
  // A fresh install labels its default policy read_only but requests NO tools.
  // Do not paint Read only as already selected: the first user tap must create
  // an actual explicit backend grant request instead of doing nothing.
  const hasExplicitChoice = !!report &&
    androidMcpCapabilityNames.some(key => report.requested[key]);
  for (const input of document.querySelectorAll<HTMLInputElement>('input[name="androidMcpAccessPreset"]')) {
    input.checked = hasExplicitChoice && input.value === report?.preset;
    input.disabled = disabled;
    input.closest<HTMLElement>('.android-mcp-preset')
      ?.setAttribute('data-selected',String(input.checked));
  }
  const groups:{id:string;keys:readonly AndroidMcpCapability[]}[] = [
    {id:'androidMcpAllowLook',keys:androidMcpReadCapabilities},
    {id:'androidMcpAllowChange',keys:androidMcpWriteCapabilities},
    {id:'androidMcpAllowBrowser',keys:['browserControl']},
    {id:'androidMcpAllowTerminal',keys:['terminal']},
  ];
  for (const group of groups) {
    const input = document.getElementById(group.id) as HTMLInputElement|null;
    if (!input) continue;
    const values = group.keys.map(key => report?.requested[key] === true);
    input.checked = values.every(Boolean);
    input.indeterminate = values.some(Boolean) && !values.every(Boolean);
    input.disabled = disabled;
  }
  for (const input of document.querySelectorAll<HTMLInputElement>(
    '.android-mcp-capability-list input[data-mcp-capability]')) {
    const key = input.dataset.mcpCapability as AndroidMcpCapability|undefined;
    if (!key || !androidMcpCapabilityNames.includes(key)) {
      input.checked = false;
      input.disabled = true;
      continue;
    }
    input.checked = report?.requested[key] === true;
    input.disabled = disabled;
  }
  renderAndroidMcpFileTools();
  renderAndroidMcpCommandRules();
  const active = report
    ? androidMcpCapabilityNames.filter(key => report.effective[key]).length : 0;
  const requested = report
    ? androidMcpCapabilityNames.filter(key => report.requested[key]).length : 0;
  const available = report
    ? androidMcpCapabilityNames.filter(key => report.available[key]).length : 0;
  statusNode.textContent = androidMcpAccessFeedback ||
    (androidMcpAccessBusy ? 'Saving the permission preference in the Android backend…' :
      androidMcpAccessUnavailable ? 'Cannot verify Android MCP tool permissions. No privileged access is claimed.' :
      !isTauri() ? 'Preview only. A native Android backend is required before any tool permission can be configured.' :
      !report ? 'Checking the native MCP tool permission policy…' :
      `${requested} requested · ${active} permitted · ${available} supported by Android MCP. The actual tools are determined by backend authorization, the approved folder and any required per-call confirmation; selecting a preset never grants system-root access.`);
  statusNode.dataset.verified = String(!!report && active > 0);
  renderAndroidWorkspace();
}
async function refreshAndroidMcpAccess():Promise<void> {
  if (!nativeAndroidRuntime || !isTauri()) {
    renderAndroidMcpAccess();
    return;
  }
  if (androidMcpAccessRefreshInFlight) return androidMcpAccessRefreshInFlight;
  const epoch = ++androidMcpAccessRefreshEpoch;
  const request = (async () => {
    try {
      const response = await invoke<unknown>('android_mcp_permissions_status');
      if (epoch !== androidMcpAccessRefreshEpoch) return;
      androidMcpAccessSnapshot = validateAndroidMcpAccessSnapshot(response);
      androidMcpAccessUnavailable = false;
    } catch {
      if (epoch !== androidMcpAccessRefreshEpoch) return;
      androidMcpAccessSnapshot = null;
      androidMcpAccessUnavailable = true;
    } finally {
      if (epoch === androidMcpAccessRefreshEpoch) renderAndroidMcpAccess();
    }
  })();
  androidMcpAccessRefreshInFlight = request;
  try { await request; }
  finally {
    if (androidMcpAccessRefreshInFlight === request) androidMcpAccessRefreshInFlight = null;
    renderAndroidMcpAccess();
  }
}
async function configureAndroidMcpAccess(
  preset:AndroidMcpPreset, requested:AndroidMcpCapabilityMap, advanceOnPreset = false
):Promise<void> {
  if (!nativeAndroidRuntime || !isTauri() || !androidMcpAccessSnapshot || androidMcpAccessBusy)
    return;
  // Only a direct human click reaches this path. Remote MCP tool calls never
  // invoke the permission configurator or synthesize userInitiated=true.
  androidMcpAccessBusy = true;
  androidMcpAccessFeedback = '';
  ++androidMcpAccessRefreshEpoch;
  renderAndroidMcpAccess();
  let confirmed = false;
  try {
    const receipt = await invoke<unknown>('android_mcp_permissions_configure',
      {preset,requested,userInitiated:true});
    androidMcpAccessSnapshot = validateAndroidMcpAccessSnapshot(receipt);
    confirmed = androidMcpAccessSnapshot.preset === preset &&
      androidMcpCapabilityNames.every(key => androidMcpAccessSnapshot!.requested[key] === requested[key]);
    if (!confirmed) throw Error('Backend did not confirm chosen access preset');
    androidMcpAccessUnavailable = false;
    androidMcpAccessFeedback = '';
  } catch {
    androidMcpAccessSnapshot = null;
    androidMcpAccessUnavailable = true;
    androidMcpAccessFeedback = 'Permission preference was not confirmed by the Android backend. No new access was enabled.';
  } finally {
    androidMcpAccessBusy = false;
    await refreshAndroidMcpAccess();
    // A choice is completed only after the native save AND fresh native
    // status agree. Tool availability is still checked independently.
    const selected = confirmed && !!androidMcpAccessSnapshot &&
      androidMcpAccessSnapshot.preset === preset &&
      androidMcpCapabilityNames.every(key => androidMcpAccessSnapshot!.requested[key] === requested[key]) &&
      androidCosSetupResult('tools').verified;
    if (selected && advanceOnPreset && androidCosSetupSelected === 'tools' &&
        !androidWorkspaceVisible())
      chooseAndroidCosSetupStep('tunnel');
    else
      renderAndroidCosSetup();
  }
}
type AndroidLocalMcpSnapshot = {
  state:'stopped'|'listening';
  localUrl:string|null;
  toolCallsHandled:number;
  remoteVerified:false;
};
let androidLocalMcpSnapshot:AndroidLocalMcpSnapshot|null = null;
let androidLocalMcpBusy = false;
let androidLocalMcpTicket = 0;
let androidLocalMcpRefreshInFlight:Promise<void>|null = null;
let androidLocalMcpUnavailable = false;
let androidLocalMcpFeedback = '';
async function refreshAndroidLocalMcp(freshAfterPending = false):Promise<void> {
  if (!nativeAndroidRuntime) return;
  if (androidLocalMcpRefreshInFlight) {
    await androidLocalMcpRefreshInFlight;
    if (!freshAfterPending) return;
  }
  const request = refreshAndroidLocalMcpOnce();
  androidLocalMcpRefreshInFlight = request;
  try { await request; }
  finally {
    if (androidLocalMcpRefreshInFlight === request) {
      androidLocalMcpRefreshInFlight = null;
      renderAndroidCosSetup();
    }
  }
}
async function refreshAndroidLocalMcpOnce():Promise<void> {
  if (!nativeAndroidRuntime) return;
  const ticket = ++androidLocalMcpTicket;
  if (!isTauri()) {
    androidLocalMcpSnapshot = null;
    renderAndroidCosSetup();
    return;
  }
  try {
    const response = await invoke<AndroidLocalMcpSnapshot>('android_local_mcp_status');
    const validUrl = typeof response?.localUrl === 'string' &&
      /^http:\/\/127\.0\.0\.1:[0-9]{1,5}\/mcp\/[0-9a-f]{64}$/.test(response.localUrl) &&
      Number(response.localUrl.match(/127\.0\.0\.1:(\d+)/)?.[1]) <= 65535;
    if (!response || !['stopped','listening'].includes(response.state) ||
        !Number.isSafeInteger(response.toolCallsHandled) || response.toolCallsHandled < 0 ||
        response.remoteVerified !== false ||
        (response.state === 'listening' && !validUrl) ||
        (response.state === 'stopped' && response.localUrl !== null))
      throw Error('Unverified native MCP listener status');
    if (ticket !== androidLocalMcpTicket) return;
    androidLocalMcpSnapshot = response;
    androidLocalMcpUnavailable = false;
  } catch {
    if (ticket !== androidLocalMcpTicket) return;
    androidLocalMcpSnapshot = null;
    androidLocalMcpUnavailable = true;
  }
  renderAndroidCosSetup();
}
async function refreshAndroidOpenAiTunnel(freshAfterPending = false): Promise<void> {
  if (!nativeAndroidRuntime) return;
  if (androidOpenAiTunnelRefreshInFlight) {
    await androidOpenAiTunnelRefreshInFlight;
    if (!freshAfterPending) return;
  }
  const request = refreshAndroidOpenAiTunnelOnce();
  androidOpenAiTunnelRefreshInFlight = request;
  try { await request; }
  finally {
    if (androidOpenAiTunnelRefreshInFlight === request) {
      androidOpenAiTunnelRefreshInFlight = null;
      renderAndroidCosSetup();
      renderAndroidCoreConnectionPopover();
    }
  }
}
async function refreshAndroidOpenAiTunnelOnce():Promise<void> {
  if (!nativeAndroidRuntime) return;
  const ticket = ++androidOpenAiTunnelTicket;
  if (!isTauri()) {
    androidOpenAiTunnelSnapshot = null;
    renderAndroidCosSetup();
    return;
  }
  try {
    const report = await invoke<AndroidOpenAiTunnelSnapshot>('android_openai_tunnel_status');
    if (ticket !== androidOpenAiTunnelTicket) return;
    if (!report || !['disconnected','connecting','polling','tool_verified','error'].includes(report.state) ||
        typeof report.pollActive !== 'boolean' || !Number.isSafeInteger(report.toolCallsVerified) ||
        report.toolCallsVerified < 0 ||
        (report.corePluginLinked !== undefined && typeof report.corePluginLinked !== 'boolean') ||
        (report.desiredConnected !== undefined && typeof report.desiredConnected !== 'boolean') ||
        (report.state === 'tool_verified' && !report.pollActive))
      throw Error('Invalid native tunnel status');
    // Health and Activity remain optional while an older native build is
    // connected. Untrusted/malformed diagnostics must never be printed raw.
    if (report.health !== undefined) {
      const health = report.health;
      const validTime = (value:unknown) => value === null ||
        (Number.isSafeInteger(value) && Number(value) >= 0);
      if (!health || !validTime(health.verifiedLinkAtMs) ||
          !validTime(health.lastChatgptCallAtMs) ||
          !Number.isSafeInteger(health.pollErrors) || health.pollErrors < 0 ||
          !validTime(health.uptimeSeconds) ||
          (health.route !== null && health.route !== 'api.openai.com:443 · direct') ||
          typeof health.tunnelClient !== 'string' || health.tunnelClient.length > 80 ||
          typeof health.localServer !== 'string' || health.localServer.length > 80 ||
          (health.probe !== undefined && health.probe !== null && health.probe !== 'ok') ||
          (health.toolCount !== undefined && health.toolCount !== null &&
            (!Number.isSafeInteger(health.toolCount) || health.toolCount < 0 || health.toolCount > 100)))
        throw Error('Invalid native tunnel diagnostics');
    }
    if (report.events !== undefined) {
      if (!Array.isArray(report.events) || report.events.length > 64 ||
          !report.events.every((entry:AndroidTunnelEvent) =>
            entry && Number.isSafeInteger(entry.timestampMs) && entry.timestampMs >= 0 &&
            ['tunnel','request','tool','error'].includes(entry.kind) &&
            typeof entry.detail === 'string' && entry.detail.length <= 160 &&
            /^[\x20-\x7E]*$/.test(entry.detail)))
        throw Error('Invalid native tunnel activity');
    }
    androidOpenAiTunnelSnapshot = report;
    androidOpenAiTunnelUnavailable = false;
  } catch {
    if (ticket !== androidOpenAiTunnelTicket) return;
    androidOpenAiTunnelSnapshot = null; // unknown is not disconnected or connected
    androidOpenAiTunnelUnavailable = true;
  }
  renderAndroidCosSetup();
}
function androidOpenAiTunnelVerified(): boolean {
  return androidOpenAiTunnelSnapshot?.state === 'tool_verified' &&
    androidOpenAiTunnelSnapshot.pollActive === true &&
    androidOpenAiTunnelSnapshot.toolCallsVerified > 0;
}
function androidCorePluginVerified(): boolean {
  const tunnel = androidOpenAiTunnelSnapshot;
  return androidOpenAiTunnelVerified() ||
    (tunnel?.corePluginLinked === true && tunnel.pollActive === true &&
      ['polling','tool_verified'].includes(tunnel.state));
}
let androidCosFolderGrant: 'unknown' | 'none' | 'granted' = 'unknown';
let androidCosFolderSource: 'files' | 'project' | null = null;
let androidCosFolderStatusTicket = 0;
let androidCosFolderGrantUnavailable = false;
type AndroidMainStorageStatus = {
  supported:boolean; granted:boolean; readable:boolean; rootPath:string;
};
type AndroidMainStorageEntry = { name:string; isDirectory:boolean; readable:boolean };
type AndroidMainSelection = {path:string[]; selected:boolean};
let androidMainStorage: AndroidMainStorageStatus | null = null;
let androidMainStorageTicket = 0;
let androidMainStorageRefreshInFlight:Promise<void>|null = null;
let androidMainStorageUnavailable = false;
let androidMainSelection:AndroidMainSelection|null = null;
let androidMainSelectionTicket = 0;
let androidMainSelectionRefreshInFlight:Promise<void>|null = null;
let androidMainSelectionUnavailable = false;
let androidMainSelectionBusy = false;
let androidMainFolderPath:string[] = [];
let androidMainFolderOpen = false;
let androidMainFolderBusy = false;
let androidMainFolderEntries:AndroidMainStorageEntry[]|null = null;
let androidMainFolderTicket = 0;
let androidCosFolderFeedback = '';
function androidMainFolderValidPath(path:unknown):path is string[] {
  return Array.isArray(path) && path.length <= 16 &&
    path.every(name => typeof name === 'string' && name.length >= 1 &&
      name.length <= 255 && name !== '.' && name !== '..' &&
      !/[\\/\u0000-\u001f]/.test(name));
}
function androidMainSelectionValid(result:unknown):result is AndroidMainSelection {
  if (!result || typeof result !== 'object') return false;
  const selection = result as AndroidMainSelection;
  return androidMainFolderValidPath(selection.path) &&
    typeof selection.selected === 'boolean' &&
    selection.selected === (selection.path.length !== 0);
}
function androidMainPathString(path:string[]):string {
  const root = androidMainStorageVerified() ? androidMainStorage!.rootPath : '/storage/emulated/0';
  return root + (path.length ? '/' + path.join('/') : '');
}
async function refreshAndroidMainSelection(freshAfterPending = false):Promise<void> {
  if (!nativeAndroidRuntime || androidMainSelectionBusy) return;
  if (androidMainSelectionRefreshInFlight) {
    await androidMainSelectionRefreshInFlight;
    if (!freshAfterPending) return;
  }
  const request = refreshAndroidMainSelectionOnce();
  androidMainSelectionRefreshInFlight = request;
  try { await request; }
  finally {
    if (androidMainSelectionRefreshInFlight === request) {
      androidMainSelectionRefreshInFlight = null;
      renderAndroidCosSetup();
    }
  }
}
async function refreshAndroidMainSelectionOnce():Promise<void> {
  if (!nativeAndroidRuntime) return;
  const ticket = ++androidMainSelectionTicket;
  if (!androidMainStorageVerified() || !isTauri()) {
    androidMainSelection = null;
    renderAndroidCosSetup();
    return;
  }
  try {
    const receipt = await invoke<AndroidMainSelection>('android_main_storage_selection_status');
    if (ticket !== androidMainSelectionTicket) return;
    if (!androidMainSelectionValid(receipt)) throw Error('Invalid saved folder selection');
    androidMainSelection = receipt;
    androidMainSelectionUnavailable = false;
  } catch {
    if (ticket !== androidMainSelectionTicket) return;
    androidMainSelection = null;
    androidMainSelectionUnavailable = true;
  }
  renderAndroidCosSetup();
}
function androidMainStorageVerified():boolean {
  return androidMainStorage?.supported === true &&
    androidMainStorage.granted === true && androidMainStorage.readable === true &&
    typeof androidMainStorage.rootPath === 'string' &&
    /^\/storage\/[a-zA-Z0-9_./-]{1,220}$/.test(androidMainStorage.rootPath);
}
async function refreshAndroidMainStorageStatus(freshAfterPending = false):Promise<void> {
  if (!nativeAndroidRuntime) return;
  if (androidMainStorageRefreshInFlight) {
    await androidMainStorageRefreshInFlight;
    if (!freshAfterPending) return;
  }
  const request = refreshAndroidMainStorageStatusOnce();
  androidMainStorageRefreshInFlight = request;
  try { await request; }
  finally {
    if (androidMainStorageRefreshInFlight === request) {
      androidMainStorageRefreshInFlight = null;
      renderAndroidCosSetup();
    }
  }
}
async function refreshAndroidMainStorageStatusOnce():Promise<void> {
  if (!nativeAndroidRuntime) return;
  const ticket = ++androidMainStorageTicket;
  if (!isTauri()) {
    androidMainStorage = null;
    renderAndroidCosSetup();
    return;
  }
  try {
    const status = await invoke<AndroidMainStorageStatus>('android_main_storage_status');
    if (ticket !== androidMainStorageTicket) return;
    if (!status || typeof status.supported !== 'boolean' ||
        typeof status.granted !== 'boolean' || typeof status.readable !== 'boolean' ||
        typeof status.rootPath !== 'string' || status.rootPath.length > 256 ||
        (status.granted && !status.supported) ||
        (status.readable && !status.granted))
      throw Error('Invalid primary storage status');
    androidMainStorage = status;
    androidMainStorageUnavailable = false;
    if (!androidMainStorageVerified()) {
      androidMainFolderEntries = null;
      androidMainFolderPath = [];
      androidMainFolderOpen = false;
      androidMainSelection = null;
    }
  } catch {
    if (ticket !== androidMainStorageTicket) return;
    androidMainStorage = null;
    androidMainStorageUnavailable = true;
    androidMainFolderEntries = null;
    androidMainFolderOpen = false;
    androidMainSelection = null;
  }
  renderAndroidCosSetup();
  if (androidMainStorageVerified()) void refreshAndroidMainSelection();
}
function renderAndroidCosFolderExplorer():void {
  if (!nativeAndroidRuntime) return;
  $('androidCosFolderExplorer').classList.toggle('hidden',!androidMainFolderOpen);
  $('androidCosFolderCurrentPath').textContent =
    ['Internal storage',...androidMainFolderPath].join(' / ');
  $('androidCosFolderCandidate').textContent = androidMainPathString(androidMainFolderPath);
  ($('androidCosFolderSet') as HTMLButtonElement).disabled =
    !androidMainStorageVerified() || androidMainSelectionBusy ||
    androidMainFolderBusy || !androidMainFolderEntries ||
    !androidMainSelection ||
    androidMainSelection.path.join('/') === androidMainFolderPath.join('/');
  ($('androidCosFolderUp') as HTMLButtonElement).disabled =
    androidMainFolderBusy || androidMainFolderPath.length === 0;
  ($('androidCosFolderReload') as HTMLButtonElement).disabled =
    androidMainFolderBusy || !androidMainStorageVerified();
  const list = $('androidCosFolderEntries');
  list.replaceChildren();
  if (androidMainFolderBusy) {
    list.append(el('p','', 'Reading folders on this phone…'));
    return;
  }
  if (!androidMainFolderEntries) {
    list.append(el('p','', 'Unable to read this location. Return to Internal storage or retry.'));
    return;
  }
  const folders = androidMainFolderEntries.filter(item => item.isDirectory);
  if (!folders.length) {
    list.append(el('p','', 'No accessible subfolders here.'));
    return;
  }
  for (const folder of folders) {
    const button = document.createElement('button');
    button.type = 'button';
    button.disabled = !folder.readable;
    button.textContent = '▱  ' + folder.name + (folder.readable ? '  ›' : '  · Restricted');
    button.addEventListener('click', () => {
      if (!button.disabled) void loadAndroidMainFolder([...androidMainFolderPath,folder.name]);
    });
    list.append(button);
  }
}
async function loadAndroidMainFolder(path:string[]):Promise<void> {
  if (!nativeAndroidRuntime || !isTauri() || !androidMainStorageVerified() ||
      androidMainFolderBusy || !androidMainFolderValidPath(path)) return;
  const ticket = ++androidMainFolderTicket;
  androidMainFolderOpen = true;
  androidMainFolderBusy = true;
  renderAndroidCosFolderExplorer();
  try {
    const result = await invoke<{path:string[];entries:AndroidMainStorageEntry[]}>(
      'android_main_storage_list',{path});
    if (ticket !== androidMainFolderTicket) return;
    if (!result || !Array.isArray(result.path) || result.path.join('/') !== path.join('/') ||
        !Array.isArray(result.entries) || result.entries.length > 128 ||
        result.entries.some(item => !item || typeof item.name !== 'string' ||
          item.name.length < 1 || item.name.length > 255 ||
          item.name === '.' || item.name === '..' ||
          /[\\/\u0000-\u001f]/.test(item.name) ||
          typeof item.isDirectory !== 'boolean' || typeof item.readable !== 'boolean'))
      throw Error('Invalid storage listing');
    androidMainFolderPath = [...path];
    androidMainFolderEntries = result.entries;
  } catch {
    if (ticket !== androidMainFolderTicket) return;
    androidMainFolderEntries = null;
  } finally {
    if (ticket === androidMainFolderTicket) {
      androidMainFolderBusy = false;
      renderAndroidCosFolderExplorer();
    }
  }
}
async function refreshAndroidCosFolderGrant(): Promise<void> {
  if (!nativeAndroidRuntime) return;
  const ticket = ++androidCosFolderStatusTicket;
  const projectId = selectedProjectId;
  if (!isTauri()) {
    androidCosFolderGrant = 'unknown';
    androidCosFolderSource = null;
    renderAndroidCosSetup();
    return;
  }
  try {
    const status = await invoke<{granted:boolean;treeUri:string}>('document_tree_status');
    if (ticket !== androidCosFolderStatusTicket) return;
    if (typeof status?.granted !== 'boolean' || typeof status?.treeUri !== 'string')
      throw Error('Invalid SAF folder status');
    let nextGrant:'unknown'|'none'|'granted' = status.granted && status.treeUri.length > 0
      ? 'granted' : status.granted ? 'unknown' : 'none';
    let nextSource:'files'|'project'|null = nextGrant === 'granted' ? 'files' : null;
    if (nextGrant !== 'granted' && projectId &&
        mobileProjects.some(project => project.id === projectId)) {
      const linked = await invoke<{projectId:string;linked:boolean;granted:boolean;
        treeUri:string;readOnly:boolean}>('document_tree_project_folder_status',{projectId});
      if (ticket !== androidCosFolderStatusTicket || selectedProjectId !== projectId) return;
      if (linked.projectId !== projectId || typeof linked.granted !== 'boolean' ||
          typeof linked.linked !== 'boolean' || typeof linked.treeUri !== 'string' ||
          linked.readOnly !== true) throw Error('Invalid Project folder receipt');
      if (linked.granted && linked.linked && linked.treeUri.length > 0) {
        nextGrant = 'granted';
        nextSource = 'project';
      }
    }
    if (ticket !== androidCosFolderStatusTicket || selectedProjectId !== projectId) return;
    androidCosFolderGrant = nextGrant;
    androidCosFolderSource = nextSource;
    androidCosFolderGrantUnavailable = false;
  } catch {
    if (ticket !== androidCosFolderStatusTicket) return;
    androidCosFolderGrant = 'unknown';
    androidCosFolderSource = null;
    androidCosFolderGrantUnavailable = true;
  }
  renderAndroidCosSetup();
}
function androidCosPermissionProgress(): {
  allowed:number; total:number; pending:string[]; unsupported:string[];
  unknown:string[]; accessibilityWaiting:boolean;
} {
  const names: Record<AndroidPermission,string> = {
    files:'All files access',overlay:'Display over other apps',
    notifications:'Notifications',battery:'Battery optimization'
  };
  const ids: Record<AndroidPermission,string> = {
    files:'storageAccessState',overlay:'overlayAccessState',
    notifications:'notificationAccessState',battery:'batteryAccessState'
  };
  let allowed = 0;
  let total = 0;
  const pending:string[] = [];
  const unsupported:string[] = [];
  const unknown:string[] = [];
  for (const key of androidPermissionKeys) {
    const node = $(ids[key]);
    if (node.dataset.supported === 'false') { unsupported.push(names[key]); continue; }
    if (node.dataset.supported !== 'true') { unknown.push(names[key]); continue; }
    total++;
    if (node.dataset.granted === 'true') allowed++;
    else pending.push(names[key]);
  }
  const accessibility = $('accessibilityPermissionState');
  if (accessibility.dataset.supported === 'false') unsupported.push('Accessibility service');
  else if (accessibility.dataset.supported !== 'true') unknown.push('Accessibility service');
  else {
    total++;
    if (accessibility.dataset.granted === 'true') allowed++;
    else pending.push('Accessibility service');
  }
  return {allowed,total,pending,unsupported,unknown,
    accessibilityWaiting:accessibility.dataset.granted === 'true' &&
      accessibility.dataset.connected !== 'true'};
}
function androidCosAllPermissionsAllowed(): boolean {
  const access = androidCosPermissionProgress();
  return access.total > 0 && access.allowed === access.total &&
    access.unknown.length === 0 && !access.accessibilityWaiting;
}
function androidCosFivePermissionsConfirmed(): boolean {
  const access = androidCosPermissionProgress();
  // A return from Android Settings may have revoked a previously granted
  // permission. Gate setup actions until fresh OS receipts have settled.
  return !androidPermissionRefreshPending && !androidAccessibilityRefreshPending &&
    access.total === 5 && access.allowed === 5 &&
    access.pending.length === 0 && access.unknown.length === 0 &&
    access.unsupported.length === 0 && !access.accessibilityWaiting;
}
function androidOwnedChatgptLinked(): boolean {
  return ownedChatgptStatus?.supported === true &&
    ownedChatgptStatus.running === true && ownedChatgptStatus.sessionLinked === true;
}
function androidCosSetupResult(step: AndroidCosSetupStep): AndroidCosSetupResult {
  const access = androidCosPermissionProgress();
  switch (step) {
    case 'access': {
      // A confirmed OS denial wins over an unrelated pending/failed refresh:
      // this step needs user action, not another status query. Only use
      // Checking/Unable to check when the missing proof could change the result.
      const denied = access.pending.length > 0 || access.unsupported.length > 0;
      const checking = !denied &&
        (androidPermissionRefreshPending || androidAccessibilityRefreshPending ||
          (access.unknown.length > 0 &&
            !androidPermissionStatusUnavailable && !androidAccessibilityStatusUnavailable));
      return {verified:androidCosFivePermissionsConfirmed(),
        checking,
        unavailable:!denied && !checking &&
          (androidPermissionStatusUnavailable || androidAccessibilityStatusUnavailable),
        detail:`${access.allowed}/5 permissions confirmed. ${access.pending.length ? 'Awaiting: ' + access.pending.join(', ') + '. ' : ''}${access.unknown.length ? 'Unable to verify: ' + access.unknown.join(', ') + '. ' : ''}${access.unsupported.length ? 'Unavailable: ' + access.unsupported.join(', ') + '. ' : ''}${access.accessibilityWaiting ? 'Waiting for Android Accessibility service connection. ' : ''}Setup continues to Extensions once Android confirms all five permissions and the Accessibility service is connected.`};
    }
    case 'extension':
      if (!androidCosExtensions)
        return {verified:false,checking:androidCosExtensionBusy || !androidCosExtensionsUnavailable,
          unavailable:androidCosExtensionsUnavailable && !androidCosExtensionBusy,
          detail:androidCosExtensionsUnavailable
            ? 'Unable to read GeckoView extension status. Refresh to check the current installation; no extension is claimed.'
            : 'Checking native GeckoView extension installation. Nothing installs automatically.'};
      return androidCosExtensions.some(extension => extension.enabled)
        ? {verified:true,detail:'GeckoView reports an installed and enabled extension in its own profile. No desktop Chrome pairing is claimed.'}
        : {verified:false,notNeeded:true,detail:androidCosExtensions.length
          ? 'GeckoView extensions are installed but disabled. Extensions are optional for Android ChatGPT login.'
          : 'No GeckoView extension is installed. This is optional for Android ChatGPT login.'};
    case 'browser':
      return androidCosOAuthLinked()
        ? {verified:true,detail:'OpenAI verified the ChatGPT account and approved plan access for Mcp Agent Ai. Bring Session linked the OAuth grant. Website cookies are separate.'}
        : {verified:false,
          checking:androidCosOAuthBusy || (!androidCosOAuthStatus && !androidCosOAuthUnavailable),
          unavailable:androidCosOAuthUnavailable && !androidCosOAuthStatus && !androidCosOAuthBusy,
          detail:androidCosOAuthReadyToBring()
          ? 'OpenAI authorized your ChatGPT plan for Mcp Agent Ai. Tap Bring Session to verify and link this grant.'
          : androidCosOAuthStatus?.state === 'authorizing'
            ? 'Finish Google or ChatGPT sign-in and approve Mcp Agent Ai in the system browser, then return here.'
          : androidCosOAuthUnavailable
            ? 'Could not check the native app authorization. No connected account is assumed.'
          : !androidCosOAuthStatus
            ? 'Checking native Sign in with ChatGPT authorization.'
          : androidCosOAuthStatus.state === 'verified'
            ? 'Account verified but required plan access was not granted. Review permissions and retry.'
          : 'Sign in with ChatGPT through the system browser, then approve Mcp Agent Ai.'};
    case 'folder':
      return androidMainStorageVerified()
        ? {verified:true,detail:'Android confirms main shared storage is readable. Select a folder to save it for this app, or Reset to use Internal storage root. Protected Android app data remains restricted.'}
        : !androidMainStorage
          ? {verified:false,checking:!androidMainStorageUnavailable ||
              !!androidMainStorageRefreshInFlight,
            unavailable:androidMainStorageUnavailable && !androidMainStorageRefreshInFlight,
            detail:androidMainStorageUnavailable
              ? 'Unable to check Android shared storage. Retry the native permission and storage status.'
              : 'Checking the actual Android shared storage and All files access permission.'}
          : !androidMainStorage.granted
            ? {verified:false,detail:'All files access is not granted. Allow it in Step 1 to use the main phone storage.'}
          : {verified:false,detail:'Android reports shared storage is not readable. Retry after checking storage permission in Step 1.'};
    case 'tools': {
      const policy = androidMcpAccessSnapshot;
      if (!policy) return {verified:false,checking:!androidMcpAccessUnavailable ||
          !!androidMcpAccessRefreshInFlight || androidMcpAccessBusy,
        unavailable:androidMcpAccessUnavailable && !androidMcpAccessRefreshInFlight &&
          !androidMcpAccessBusy,
        detail:androidMcpAccessUnavailable
          ? 'Unable to read the native MCP tool access policy. No file, browser or terminal access is assumed.'
          : 'Checking the actual Android tool capabilities and your chosen access preset.'};
      const requested = androidMcpCapabilityNames.filter(key => policy.requested[key]);
      const enabled = requested.filter(key => policy.effective[key]);
      const unsupported = requested.filter(key => !policy.available[key]);
      // Verified here means the user explicitly selected and the backend
      // saved an Access policy, NOT that every requested tool is executable.
      // This distinction matters for Full/Files+Terminal on Android, where
      // unsupported write/browser/terminal routes must remain blocked.
      const verified = requested.length > 0;
      return {verified:verified && !androidMcpAccessBusy,checking:androidMcpAccessBusy,
        detail:requested.length === 0
          ? 'No access preset selected. Choose Full access, Files and terminal, or Read only to finish Step 5.'
          : `Access choice saved. ${enabled.length} of ${requested.length} requested MCP capabilities currently permitted; ${unsupported.length} unavailable. Step 5 confirms your selection, not remote permission to unsupported tools.`};
    }
    case 'tunnel':
      if (androidTunnelSaveBusy)
        return {verified:false,checking:true,detail:'Saving your Core Tunnel ID in private Android storage. This does not verify your OpenAI account.'};
      if (androidTunnelSavedDraftId &&
          androidTunnelSavedDraftId ===
            ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim())
        return {verified:true,detail:'Core Tunnel ID saved in private Android storage. Step 6 is complete. OpenAI account authorization is checked in Step 7, and MCP Plugin tool calls are checked in Step 9.'};
      if (androidTunnelSaveReason)
        return {verified:false,detail:'The Core Tunnel ID was not saved. Check its format and retry Save.'};
      return {verified:false,detail:'Create a Core tunnel and save its ID. This step does not ask for a runtime key; authorization follows in Step 7.'};
    case 'key': {
      const secureKeyStored = androidTunnelCredentials?.stored === true &&
        androidTunnelCredentials.secure === true;
      const runtimeVerified = secureKeyStored && !!androidTunnelSavedDraftId &&
        androidTunnelMetadataVerifiedId === androidTunnelSavedDraftId;
      const runtimeDetail = !androidTunnelSavedDraftId
        ? 'Save your Core Tunnel ID in Step 6 before adding its API key.'
        : runtimeVerified
          ? 'Tunnel API key stored securely. OpenAI confirmed access to this Core Tunnel ID.'
          : androidTunnelSaveReason === 'unauthorized'
            ? 'OpenAI denied tunnel access. Check the project, workspace and Tunnels: Read + Use permissions.'
            : androidTunnelSaveReason === 'not_found'
              ? 'This Tunnel ID was not found for the stored API key.'
              : androidTunnelSaveReason === 'network_error'
                ? 'Unable to reach OpenAI. Reopen Step 7 to check access again.'
                : secureKeyStored
                  ? 'Tunnel API key is stored in Android Keystore. Checking access to your Core Tunnel ID.'
                  : 'Create a Restricted API key with Tunnels: Read + Use, then save it securely.';
      return {
        verified:runtimeVerified &&
          !androidTunnelVerifyBusy && !androidTunnelCredentialsBusy,
        checking:androidTunnelVerifyBusy || androidTunnelCredentialsBusy ||
          (androidTunnelCredentials === null && !androidTunnelCredentialsUnavailable),
        detail:runtimeDetail
      };
    }
    case 'connect': {
      if (!androidTunnelSavedDraftId ||
          androidTunnelSavedDraftId !== ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim())
        return {verified:false,detail:'Save the Core Tunnel ID in Step 6 before tapping Connect.'};
      if (androidTunnelMetadataVerifiedId !== androidTunnelSavedDraftId ||
          androidTunnelCredentials?.stored !== true || androidTunnelCredentials.secure !== true)
        return {verified:false,detail:'Step 7 must authenticate the Tunnel ID with a stored Restricted API key before Connect.'};
      if (androidOpenAiTunnelBusy || androidOpenAiTunnelSnapshot?.state === 'connecting')
        return {verified:false,checking:true,detail:'Connecting the native OpenAI Secure MCP Tunnel…'};
      const tunnel = androidOpenAiTunnelSnapshot;
      if (tunnel?.pollActive === true && ['polling','tool_verified'].includes(tunnel.state))
        return {verified:true,detail:'Secure tunnel connected. ChatGPT tools are checked in Step 9.'};
      return {verified:false,checking:!tunnel && !androidOpenAiTunnelUnavailable,
        unavailable:!tunnel && androidOpenAiTunnelUnavailable,
        detail:tunnel?.state === 'error'
          ? 'Connection interrupted. Check your network or API key, then retry.'
          : 'Disconnected. Tap Connect to use your saved tunnel.'};
    }
    case 'mcpPlugin': {
      const currentId = ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim();
      if (!androidTunnelSavedDraftId || androidTunnelSavedDraftId !== currentId)
        return {verified:false,detail:'Save a Core Tunnel ID in Step 6 before connecting the MCP Plugin.'};
      if (androidTunnelMetadataVerifiedId !== androidTunnelSavedDraftId ||
          androidTunnelCredentials?.stored !== true || androidTunnelCredentials.secure !== true)
        return {verified:false,detail:'Verify the Core Tunnel ID with an authorized Restricted Runtime API key in Step 7 first.'};
      if (androidOpenAiTunnelBusy)
        return {verified:false,checking:true,detail:'Updating the native OpenAI MCP Plugin tunnel. No inbound tool call is verified yet.'};
      if (androidCorePluginVerified()) {
        return {verified:true,detail:androidOpenAiTunnelVerified()
          ? `Core Plugin verified · ${androidOpenAiTunnelSnapshot!.toolCallsVerified} successful tool call(s) this session.`
          : 'Core Plugin linked previously · secure tunnel connected.'};
      }
      const tunnel = androidOpenAiTunnelSnapshot;
      return {verified:false,
        checking:!tunnel && !androidOpenAiTunnelUnavailable,
        unavailable:!tunnel && androidOpenAiTunnelUnavailable,
        detail:tunnel?.state === 'polling'
          ? 'Tunnel connected. Ask ChatGPT to call a Core tool to verify.'
          : tunnel?.state === 'connecting'
            ? 'Connecting your phone to ChatGPT…'
            : tunnel?.state === 'error'
              ? 'Tunnel interrupted. Check Step 8.'
              : !tunnel
                ? androidOpenAiTunnelUnavailable ? 'Unable to check the tunnel.' : 'Checking Core Plugin status…'
                : 'Connect in Step 8, then ask ChatGPT to call a Core tool.'};
    }
    case 'ready':
      const requirements = [
        androidCosSetupResult('access'),androidCosSetupResult('browser'),
        androidCosSetupResult('folder'),androidCosSetupResult('tools'),androidCosSetupResult('tunnel'),
        androidCosSetupResult('key'),androidCosSetupResult('connect'),androidCosSetupResult('mcpPlugin')
      ];
      const pluginReady = requirements[requirements.length - 1];
      const complete = (pluginReady.verified || !!pluginReady.notNeeded) &&
        requirements.every(item => item.verified || item.notNeeded);
      return {verified:complete,
        checking:!complete && requirements.some(item => item.checking),
        unavailable:!complete && requirements.some(item => item.unavailable),
        detail:complete
          ? 'Everything is ready. Return to chat.'
          : 'Finish the pending checks shown below.'};
  }
}
function androidCosFirstUnresolvedStep(results: readonly AndroidCosSetupResult[]): AndroidCosSetupStep {
  const index = results.findIndex((result, i) =>
    i < androidCosSetupOrder.length - 1 && !result.verified && !result.notNeeded);
  return index < 0 ? 'ready' : androidCosSetupOrder[index];
}
function androidCosAllStagesComplete(results:readonly AndroidCosSetupResult[]):boolean {
  return results.length === androidCosSetupOrder.length &&
    results.every(result => result.verified || result.notNeeded) &&
    results[results.length - 1].verified;
}
function rememberAndroidCosSetupCompletion(completed:boolean):void {
  androidCosSetupPreviouslyComplete = completed;
  try {
    if (completed) localStorage.setItem(androidCosSetupCompletedHintKey,'1');
    else localStorage.removeItem(androidCosSetupCompletedHintKey);
  } catch { /* Only a non-authoritative display hint is lost. */ }
}
function renderAndroidCosSetup(): void {
  if (!nativeAndroidRuntime) return;
  const node = $('androidCosSetupWizard');
  if (!node) return;
  const permissionSnapshotSettled =
    !androidPermissionRefreshPending && !androidAccessibilityRefreshPending;
  const allFiveConfirmed = androidCosFivePermissionsConfirmed();
  if (!androidCosSetupScanInFlight && permissionSnapshotSettled &&
      !allFiveConfirmed && androidCosSetupSelected === 'access')
    androidCosAccessAutoAdvanceArmed = true;
  if (!androidCosSetupScanInFlight && permissionSnapshotSettled && allFiveConfirmed &&
      androidCosAccessAutoAdvanceArmed && androidCosSetupSelected === 'access') {
    androidCosAccessAutoAdvanceArmed = false;
    // Step 2 is optional and may already be resolved. Recheck every stage
    // before choosing the next unresolved one (or Ready if all are done).
    if (!$('settingsPanel').classList.contains('hidden') &&
        $('settingsPanel').dataset.settingsView === 'setup')
      void scanAndroidCosSetupOnOpen();
  }
  const autoSelecting = androidCosSetupScanInFlight &&
    !androidCosSetupQuietScan && !androidCosSetupUserSelectedDuringScan;
  $('androidCosSetupChecking').hidden = !autoSelecting;
  // During a *previously native-verified* fast reentry, preserve the last
  // confirmed progress badges until the background OS refresh settles. This
  // snapshot is for presentation only; native actions still use live checks.
  // Any explicit tab choice returns to live state immediately.
  const liveResults = androidCosSetupOrder.map(step => androidCosSetupResult(step));
  // Step 7/8 may become verified *after* the initial scan finished (e.g. an
  // active Core tunnel reconnects or its saved access receipt settles).
  // Remember genuine later native completion too, otherwise the next process
  // restart would miss the cosmetic "already configured" reentry hint.
  if (!androidCosSetupScanInFlight && !androidCosSetupPreviouslyComplete &&
      androidCosAllStagesComplete(liveResults))
    rememberAndroidCosSetupCompletion(true);
  const results = androidCosSetupScanInFlight && androidCosSetupQuietScan &&
    !androidCosSetupUserSelectedDuringScan && androidCosSetupHeldVerifiedResults
      ? androidCosSetupHeldVerifiedResults : liveResults;
  // The first scan can select Step 8 while its native tunnel is still
  // reconnecting. Once that same automatically selected step becomes
  // Verified, immediately follow the next unresolved step (or Step 10).
  // No extra Connect, consent request or fabricated tool proof is involved.
  if (androidCosSetupAutoFollow && !androidCosSetupScanInFlight &&
      !$('settingsPanel').classList.contains('hidden') &&
      $('settingsPanel').dataset.settingsView === 'setup') {
    const currentResult = results[androidCosSetupOrder.indexOf(androidCosSetupSelected)];
    if (currentResult?.verified || currentResult?.notNeeded) {
      const next = androidCosFirstUnresolvedStep(results);
      if (next !== androidCosSetupSelected) {
        androidCosSetupSelected = next;
        $('androidSettingsContent').scrollTop = 0;
      }
    }
  }
  // Users may inspect all ten steps even while earlier steps are pending.
  // Step completion and OS permission-sensitive actions remain independently gated.
  const permissionsSelected = !autoSelecting && androidCosSetupSelected === 'access';
  const permissionSection = $('storageAccessSection') as HTMLElement;
  permissionSection.classList.toggle('android-permissions-step', permissionsSelected);
  permissionSection.inert = !permissionsSelected;
  permissionSection.setAttribute('aria-hidden',String(!permissionsSelected));
  if (!permissionsSelected && permissionSection.contains(document.activeElement))
    (document.activeElement as HTMLElement).blur();
  $('androidCosSetupCurrent').classList.toggle('hidden',permissionsSelected || autoSelecting);
  ($('androidCosSetupCurrent') as HTMLElement).inert = permissionsSelected || autoSelecting;
  const verified = results.filter(result => result.verified).length;
  const notNeeded = results.filter(result => result.notNeeded).length;
  const checking = results.filter(result => !result.verified && !result.notNeeded && result.checking).length;
  const unavailable = results.filter(result =>
    !result.verified && !result.notNeeded && !result.checking && result.unavailable).length;
  const pending = androidCosSetupOrder.length - verified - notNeeded - checking - unavailable;
  $('androidCosSetupProgress').setAttribute('aria-valuemax',String(androidCosSetupOrder.length));
  $('androidCosSetupProgress').setAttribute('aria-valuenow', String(verified + notNeeded));
  $('androidCosSetupProgress').setAttribute('aria-label',
    `${verified} verified, ${notNeeded} not needed, ${pending} pending, ${checking} checking, ${unavailable} unavailable`);
  $('androidCosSetupProgressText').textContent =
    `${verified + notNeeded} of ${androidCosSetupOrder.length} completed · ${verified} verified` +
    (pending || checking || unavailable ? ` · ${pending + checking + unavailable} to go` : ' · All set');
  ($('androidCosSetupProgressFill') as HTMLElement).style.width =
    `${((verified + notNeeded) / androidCosSetupOrder.length) * 100}%`;
  const requiredProgress = androidCosPermissionProgress();
  $('androidRequiredPermissionCount').textContent =
    `${requiredProgress.allowed} of 5 allowed`;
  $('androidRequiredPermissionCount').dataset.complete =
    String(androidCosFivePermissionsConfirmed());
  for (const button of document.querySelectorAll<HTMLButtonElement>('[data-cos-setup-step]')) {
    const step = button.dataset.cosSetupStep as AndroidCosSetupStep;
    const stepIndex = androidCosSetupOrder.indexOf(step);
    const result = results[stepIndex];
    button.dataset.verified = String(result.verified);
    button.setAttribute('aria-current', !autoSelecting && step === androidCosSetupSelected ? 'step' : 'false');
    button.disabled = false;
    button.querySelector('small')!.textContent =
    androidCosStepLabel(result);
  }
  const index = androidCosSetupOrder.indexOf(androidCosSetupSelected);
  const labels = ['Permissions','Extensions','Browser','Folder','Access','Tunnel','API key','Connect','MCP Plugin','Ready'];
  $('androidCosSetupStepTitle').textContent = `${index+1}. ${labels[index]}`;
  const actions: Record<AndroidCosSetupStep,{text:string;target?:string;description:string}> = {
    access:{text:'Manage permissions',target:'storageAccessSection',
      description:'Allow required access. Optional access is your choice.'},
    extension:{text:'Extensions',
      description:''},
    browser:{text:'ChatGPT account session',
      description:''},
    folder:{text:'Phone storage',
      description:''},
    tools:{text:'Review tool access',
      description:'Choose the tools ChatGPT can request.'},
    tunnel:{text:'Save Core Tunnel ID',target:'androidCosTunnelMethod',
      description:'Create a tunnel and save its ID.'},
    key:{text:'Create an API key',
      description:'Save a Restricted Tunnel API key securely.'},
    connect:{text:'Connect secure tunnel',
      description:'Connect the saved secure tunnel.'},
    mcpPlugin:{text:'Verify MCP Plugin',
      description:'Add the plugin and test a Core tool.'},
    ready:{text:'Back to chat',target:'chat',
      description:'Review your setup, then return to chat.'}
  };
  const action = actions[androidCosSetupSelected];
  $('androidCosSetupStepDescription').textContent = action.description;
  $('androidCosSetupStepState').textContent = results[index].detail;
  const extensionSelected = androidCosSetupSelected === 'extension';
  const browserSelected = androidCosSetupSelected === 'browser';
  const folderSelected = androidCosSetupSelected === 'folder';
  const toolsSelected = androidCosSetupSelected === 'tools';
  $('androidCosSetupCurrent').classList.toggle('android-cos-extensions-step',extensionSelected);
  $('androidCosSetupCurrent').classList.toggle('android-cos-browser-step',browserSelected);
  $('androidCosSetupCurrent').classList.toggle('android-cos-folder-step',folderSelected);
  $('androidCosSetupCurrent').classList.toggle('android-cos-key-step',androidCosSetupSelected === 'key');
  $('androidCosSetupCurrent').classList.toggle('android-cos-chatgpt-step',androidCosSetupSelected === 'connect');
  $('androidCosSetupCurrent').classList.toggle('android-cos-ready-step',androidCosSetupSelected === 'ready');
  // Setup uses the bundled OpenAI Secure MCP Tunnel exclusively.
  for (const id of ['androidCosSetupStepTitle','androidCosSetupStepDescription','androidCosSetupStepState'])
    $(id).hidden = extensionSelected || browserSelected || folderSelected || toolsSelected ||
      androidCosSetupSelected === 'tunnel' || androidCosSetupSelected === 'key' ||
      androidCosSetupSelected === 'connect' ||
      androidCosSetupSelected === 'mcpPlugin' || androidCosSetupSelected === 'ready';
  const actionButton = $('androidCosSetupStepAction') as HTMLButtonElement;
  actionButton.textContent = action.text;
  actionButton.hidden = !action.target || ['tunnel','key','connect','mcpPlugin'].includes(androidCosSetupSelected);
  actionButton.disabled = !action.target;
  $('androidCosExtensionControls').classList.toggle('hidden',!extensionSelected);
  const installedProbe = androidCosExtensions?.some(extension => extension.id === androidCosProbeExtensionId) === true;
  ($('androidCosExtensionOpen') as HTMLButtonElement).disabled =
    !isTauri() || androidCosExtensionBusy || !allFiveConfirmed;
  ($('androidCosExtensionInstall') as HTMLButtonElement).disabled =
    !isTauri() || androidCosExtensionBusy || !allFiveConfirmed || !androidCosExtensions || installedProbe;
  $('androidCosExtensionStatus').parentElement!.dataset.state =
    androidCosExtensionFeedback ? 'error' :
      !androidCosExtensions ? 'checking' :
      androidCosExtensions.some(extension => extension.enabled) ? 'installed' : 'empty';
  $('androidCosExtensionStatus').textContent = androidCosExtensionFeedback ||
    (androidCosExtensions
      ? androidCosExtensions.length
        ? `${androidCosExtensions.length} installed · ${androidCosExtensions.filter(extension => extension.enabled).length} enabled (GeckoView only)`
        : 'Not installed · No extensions added yet'
      : 'Checking installed extensions…');
  $('androidCosBrowserChoices').classList.toggle('hidden',
    androidCosSetupSelected !== 'browser');
  ($('androidCosBrowserChoices') as HTMLElement).inert = androidCosSetupSelected !== 'browser';
  const oauth = androidCosOAuthStatus;
  const browserLinked = androidCosOAuthLinked();
  const loginConfirmed = androidCosOAuthReadyToBring();
  const busy = androidCosOAuthBusy;
  $('androidCosBrowserSignInStatus').textContent = androidCosOAuthFeedback ||
    (browserLinked
      ? 'Mcp Agent Ai approved and ChatGPT plan authorization linked. Step 3 complete.'
      : !isTauri()
        ? 'Sign in with ChatGPT requires the installed Android app.'
        : !oauth
          ? 'Checking native Sign in with ChatGPT authorization…'
          : !oauth.supported
            ? 'Native ChatGPT account authorization is unavailable on this device.'
            : loginConfirmed
              ? 'OpenAI verified the account and approved plan permission. Tap Bring Session to link it.'
              : oauth.message);
  $('androidCosBrowserSignInStatus').parentElement!.dataset.state =
    browserLinked || loginConfirmed ? 'verified' :
      androidCosOAuthFeedback || oauth?.state === 'error' ? 'error' : 'waiting';
  ($('androidCosBrowserOpen') as HTMLButtonElement).disabled =
    busy || browserLinked || loginConfirmed ||
    oauth?.supported !== true || !allFiveConfirmed || !isTauri();
  ($('androidCosBrowserBring') as HTMLButtonElement).disabled =
    busy || !loginConfirmed || browserLinked || !allFiveConfirmed || !isTauri();
  $('androidCosAccessPanel').classList.toggle('hidden',
    !toolsSelected && !androidWorkspaceVisible());
  $('androidCosTunnelMethod').classList.toggle('hidden',androidCosSetupSelected !== 'tunnel');
  for (const [step,id] of [
    ['folder','androidCosFolderPanel'],['key','androidCosKeyPanel'],
    ['connect','androidCosConnectPanel'],['mcpPlugin','androidCosMcpPluginPanel'],
    ['ready','androidCosReadyPanel']
  ] as Array<[AndroidCosSetupStep,string]>)
    $(id).classList.toggle('hidden',androidCosSetupSelected !== step &&
      !(step === 'folder' && androidWorkspaceVisible()));
  const mainReady = androidMainStorageVerified();
  $('androidCosMainState').textContent = mainReady ? 'Ready' :
    !androidMainStorage ? 'Checking' : 'Unavailable';
  $('androidCosMainState').dataset.state = mainReady ? 'ready' :
    !androidMainStorage ? 'checking' : 'blocked';
  $('androidCosMainPath').textContent = mainReady
    ? androidMainStorage!.rootPath : 'Main shared storage · Android';
  $('androidCosMainDrive').dataset.ready = String(mainReady);
  $('androidCosSelectedPath').textContent = !mainReady
    ? 'Waiting for Android storage permission'
    : !androidMainSelection
      ? androidMainSelectionUnavailable ? 'Unable to check saved folder · Retry' : 'Checking saved folder…'
      : androidMainPathString(androidMainSelection.path);
  ($('androidCosFolderReset') as HTMLButtonElement).disabled =
    !mainReady || androidMainSelectionBusy || !androidMainSelection?.selected;
  ($('androidCosFolderBrowse') as HTMLButtonElement).disabled =
    !mainReady || androidMainFolderBusy || androidMainSelectionBusy;
  $('androidCosFolderBrowse').querySelector('small')!.textContent =
    androidMainFolderOpen ? 'Hide folder list' : 'Open folders inside Internal storage';
  ($('androidCosFolderNext') as HTMLButtonElement).disabled =
    !mainReady || androidMainSelectionBusy || !androidMainSelection;
  $('androidCosFolderStatus').textContent = androidCosFolderFeedback || results[3].detail;
  renderAndroidCosFolderExplorer();
  const connectTunnel = androidOpenAiTunnelSnapshot;
  const connectActive = ['connecting','polling','tool_verified'].includes(connectTunnel?.state || '');
  const connectKeyStored = androidTunnelCredentials?.stored === true &&
    androidTunnelCredentials.secure === true;
  const connectId = ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim();
  const connectAuthorized = !!androidTunnelSavedDraftId &&
    androidTunnelSavedDraftId === connectId &&
    androidTunnelMetadataVerifiedId === connectId && connectKeyStored;
  $('androidCosChatgptConnectStatus').textContent = androidOpenAiTunnelFeedback || results[7].detail;
  $('androidCosChatgptConnectStatus').dataset.state =
    results[7].verified ? 'connected' :
    androidOpenAiTunnelSnapshot?.state === 'error' ? 'error' : 'pending';
  const connectButton = $('androidCosChatgptConnect') as HTMLButtonElement;
  connectButton.disabled = !isTauri() || !connectAuthorized || connectActive ||
    androidOpenAiTunnelBusy || androidTunnelVerifyBusy ||
    androidTunnelCredentialsBusy || androidCosTunnelKind() !== 'openai';
  connectButton.textContent = androidOpenAiTunnelBusy ? 'Connecting…' :
    connectTunnel?.pollActive && ['polling','tool_verified'].includes(connectTunnel.state)
      ? 'Connected' : 'Connect';
  ($('androidCosTunnelDisconnect') as HTMLButtonElement).disabled =
    !isTauri() || (!connectActive && connectTunnel?.desiredConnected !== true) ||
    androidOpenAiTunnelBusy;
  $('androidCosConnectStatus').textContent = results[7].detail;
  $('androidCosChatgptStatus').textContent = results[5].detail;
  // The Core Plugin card reports its own backend pairing receipt; a stale
  // Step 8 Connect/Disconnect toast must not mask the Verified result.
  $('androidCosMcpPluginStatus').textContent = results[8].detail;
  $('androidCosMcpPluginStatus').dataset.state =
    results[8].verified ? 'connected' :
    androidOpenAiTunnelSnapshot?.state === 'error' ? 'error' : 'pending';
  renderAndroidCosMcpPluginIdentity();
  ($('androidCosMcpPluginOpen') as HTMLButtonElement).disabled =
    !isTauri() || androidCosMcpOpenBusy;
  $('androidCosReadyPanel').querySelector('h4')!.textContent =
    results[9].verified ? 'Ready to go' : 'Finish setup';
  $('androidCosReadyPanel').dataset.state = results[9].verified ? 'verified' : 'pending';
  $('androidCosReadyPanel').querySelector('.android-cos-ready-emblem')!.textContent =
    results[9].verified ? '✓' : '…';
  $('androidCosReadyStatus').textContent = results[9].verified
    ? 'All required setup checks passed.' : 'Finish the pending steps listed above to complete Setup.';
  const checklist = $('androidCosReadyChecklist');
  checklist.replaceChildren(...androidCosSetupOrder.map((step, i) => {
    const item = document.createElement('li');
    const result = results[i];
    item.dataset.state = result.verified ? 'verified' :
      result.notNeeded ? 'not-needed' : result.checking ? 'checking' :
      result.unavailable ? 'unavailable' : 'pending';
    const name = document.createElement('span');
    name.textContent = labels[i];
    const status = document.createElement('strong');
    status.textContent = androidCosStepLabel(result);
    item.append(name,status);
    return item;
  }));
  const tunnel = androidOpenAiTunnelSnapshot;
  const tunnelActive = tunnel?.state === 'connecting' || tunnel?.state === 'polling' ||
    tunnel?.state === 'tool_verified';
  const tunnelKind = androidCosTunnelKind();
  const storedMcpKey = androidTunnelCredentials?.stored === true &&
    androidTunnelCredentials.secure === true;
  const coreMcpId = ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim();
  const coreMcpAuthorized = !!androidTunnelSavedDraftId &&
    androidTunnelSavedDraftId === coreMcpId &&
    androidTunnelMetadataVerifiedId === coreMcpId && storedMcpKey;
  $('androidCosMcpPluginEvidence').textContent = androidOpenAiTunnelVerified() && coreMcpAuthorized
      ? `${tunnel!.toolCallsVerified} inbound MCP tool call(s) verified by the native OpenAI tunnel during this app session.`
      : androidCorePluginVerified() && coreMcpAuthorized
        ? 'A real MCP tool response was acknowledged by OpenAI before. The same Core tunnel is connected again. No new tool call has been claimed for this session.'
      : tunnel?.state === 'polling'
        ? 'OpenAI tunnel is polling. No tools/call response has been accepted yet. Leave the app open and test the plugin from ChatGPT.'
        : 'No confirmed inbound tools/call response. A saved Core ID or OpenAI tunnel heartbeat cannot verify the MCP Plugin.';
  const methodSelect = $('androidOpenAiTunnelMethodSelect') as HTMLSelectElement;
  methodSelect.disabled = androidOpenAiTunnelBusy || !!tunnelActive ||
    androidLocalMcpBusy || androidLocalMcpSnapshot?.state === 'listening';
  const hints:Record<AndroidCosTunnelKind,string> = {
    openai:'ChatGPT reaches this phone through OpenAI. No public port is opened.',
    cloudflared:'Temporary public HTTPS URL, changed on restart. Android runtime not bundled.',
    manual:'Listens on localhost only. Use your own HTTPS tunnel.'
  };
  $('androidOpenAiTunnelMethodHint').textContent = hints[tunnelKind];
  ($('androidOpenAiTunnelRuntime') as HTMLInputElement).value = tunnelKind === 'openai'
    ? 'Native Android runtime (bundled)'
    : tunnelKind === 'cloudflared' ? 'cloudflared not bundled on Android' : 'Native localhost MCP server (bundled)';
  $('androidCosTunnelProgramHint').textContent = tunnelKind === 'openai'
    ? 'Bundled with the app. No executable to select.'
    : tunnelKind === 'cloudflared'
      ? 'Android cloudflared is unavailable. No public URL is generated.'
      : 'Native listener is bundled. Start it in step 6.';
  $('androidCosTunnelLifecycle').textContent = tunnelKind === 'openai'
    ? 'Tunnel connection remains manual. The app session stays in a foreground service after you leave the Activity; the background session does not connect the tunnel automatically.'
    : tunnelKind === 'cloudflared'
      ? 'Not available in this Android build.'
      : 'Start and stop manually. The listener ends when the app process closes.';
  $('androidCosTunnelVerificationHint').textContent = tunnelKind === 'openai'
    ? 'Polling alone is not proof of ChatGPT pairing. A real MCP tool response is required.'
    : tunnelKind === 'cloudflared'
      ? 'No public URL is available until Android cloudflared support is implemented.'
      : 'Local responses cannot prove a ChatGPT connection.';
  $('androidCosAlternativeMethodStatus').hidden = tunnelKind === 'openai';
  $('androidCosAlternativeMethodStatus').textContent = tunnelKind === 'openai' ? '' : results[5].detail;
  $('androidOpenAiTunnelCredentials').classList.toggle('hidden',tunnelKind !== 'openai');
  ($('androidOpenAiTunnelCredentials') as HTMLElement).inert = tunnelKind !== 'openai';
  $('androidLocalMcpControls').classList.toggle('hidden',
    androidCosSetupSelected !== 'tunnel' || tunnelKind !== 'manual');
  ($('androidLocalMcpControls') as HTMLElement).inert =
    androidCosSetupSelected !== 'tunnel' || tunnelKind !== 'manual';
  const local = androidLocalMcpSnapshot;
  ($('androidLocalMcpUrl') as HTMLInputElement).value =
    local?.state === 'listening' ? (local.localUrl || '') : '';
  $('androidLocalMcpStatus').textContent = androidLocalMcpFeedback ||
    (local?.state === 'listening'
      ? `Listening on localhost; ${local.toolCallsHandled} MCP tool call(s) handled. No remote ChatGPT pairing proof.`
      : local?.state === 'stopped'
        ? 'Local MCP server stopped. No endpoint is exposed.' : 'Native MCP server status unknown.');
  ($('androidLocalMcpStart') as HTMLButtonElement).disabled =
    androidLocalMcpBusy || local?.state !== 'stopped' || !isTauri();
  ($('androidLocalMcpStop') as HTMLButtonElement).disabled =
    androidLocalMcpBusy || local?.state !== 'listening' || !isTauri();
  $('androidOpenAiTunnelStatus').textContent = tunnelKind !== 'openai'
    ? tunnelKind === 'manual'
      ? 'Local-only MCP does not create a public URL. Run your own HTTPS tunnel; local requests are not verified as ChatGPT requests.'
      : 'Cloudflare quick tunnel is not bundled. No public URL or remote tool call is verified.'
    : androidOpenAiTunnelFeedback ||
    (androidOpenAiTunnelVerified()
      ? `Verified: ${tunnel!.toolCallsVerified} inbound MCP tool response(s) accepted by OpenAI this session.`
      : tunnel?.state === 'polling'
        ? 'Polling the OpenAI control plane; no remote tool call has been verified.'
        : tunnel?.state === 'connecting'
          ? 'Connecting to the OpenAI control plane; no tool call verified.'
          : tunnel?.state === 'error'
            ? 'Tunnel error. Check the OpenAI tunnel ID, runtime key and network.'
            : tunnel?.state === 'disconnected'
              ? 'Disconnected. No remote tool calls are available.'
              : 'Native tunnel status is unknown; cannot claim a connection.');
  $('androidOpenAiTunnelStatus').hidden = tunnelKind !== 'openai';
  const tunnelId = ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim();
  const tunnelIdStatus = document.getElementById('androidTunnelIdStatus');
  if (tunnelIdStatus) tunnelIdStatus.textContent =
    androidTunnelSaveBusy
      ? 'Saving Core Tunnel ID on this phone…'
      : androidTunnelMetadataVerifiedId && androidTunnelMetadataVerifiedId === tunnelId
        ? 'Core Tunnel ID saved and authenticated with OpenAI. The ChatGPT workspace and actual tool connection are verified separately.'
        : androidTunnelSavedDraftId === tunnelId && !!tunnelId
          ? 'Core Tunnel ID saved. Its existence and account access are not yet verified. Continue to Step 7 for Runtime API key verification.'
          : androidTunnelSaveReason === 'service_error'
            ? 'Could not save this Core Tunnel ID in Android storage. Please retry.'
            : !tunnelId
                    ? 'Paste your Core Tunnel ID, then tap Save. An ID alone is not proof of access.'
                    : !validTunnelIdShape(tunnelId)
                      ? 'Invalid ID format. Copy the complete tunnel_… ID from OpenAI.'
                      : androidTunnelIdBusy
                        ? 'Checking ID format locally in the Android backend…'
                        : androidTunnelIdChecked && androidTunnelValidatedId === tunnelId
                          ? 'ID format accepted. Tap Save to store it and continue to Step 7; this does not verify OpenAI access.'
                          : 'Waiting for native ID format validation. A valid format does not prove ownership.';
  const saveId = document.getElementById('androidTunnelSaveId') as HTMLButtonElement|null;
  if (saveId) {
    saveId.disabled = !isTauri() || !validTunnelIdShape(tunnelId) ||
      !androidTunnelIdChecked || androidTunnelValidatedId !== tunnelId ||
      androidTunnelIdBusy || androidTunnelSaveBusy || androidOpenAiTunnelBusy;
    saveId.textContent = androidTunnelSaveBusy ? 'Saving…' : 'Save';
  }
  const credentialInput = $('androidOpenAiTunnelKey') as HTMLInputElement;
  const secureStored = androidTunnelCredentials?.stored === true &&
    androidTunnelCredentials.secure === true;
  const storedKeyState = document.getElementById('androidTunnelStoredKeyState');
  if (storedKeyState) {
    storedKeyState.textContent = androidTunnelCredentialsFeedback ||
      (androidTunnelCredentialsBusy ? 'Updating secure Android credential storage…'
        : androidTunnelCredentialsUnavailable
          ? 'Android Keystore status unavailable. No stored key is assumed.'
          : !androidTunnelCredentials
            ? 'Checking Android Keystore credential status…'
            : secureStored
              ? '•••••••• stored securely on this phone. Paste a new key to replace it or remove the stored key.'
              : 'No stored tunnel API key. Create a Restricted key with Tunnels: Read + Use and save it here.');
  }
  const saveKeyButton = document.getElementById('androidTunnelSaveKey') as HTMLButtonElement|null;
  if (saveKeyButton) saveKeyButton.disabled = !isTauri() || androidTunnelCredentialsBusy ||
    androidOpenAiTunnelBusy || credentialInput.value.trim().length < 8 || tunnelKind !== 'openai';
  const removeKeyButton = document.getElementById('androidTunnelRemoveStoredKey') as HTMLButtonElement|null;
  if (removeKeyButton) {
    removeKeyButton.hidden = !secureStored;
    removeKeyButton.disabled = !isTauri() || androidTunnelCredentialsBusy ||
      androidTunnelVerifyBusy || androidOpenAiTunnelBusy || !secureStored;
  }
  const step7State = document.getElementById('androidTunnelStep7Status');
  const accessVerified = secureStored && !!androidTunnelSavedDraftId &&
    androidTunnelMetadataVerifiedId === androidTunnelSavedDraftId;
  const accessDenied = ['unauthorized','not_found','network_error','service_error']
    .includes(androidTunnelSaveReason || '');
  if (step7State) {
    step7State.textContent = !androidTunnelSavedDraftId
      ? 'Save a Core Tunnel ID in Step 6 before creating the key.'
      : androidTunnelVerifyBusy
          ? 'Checking Tunnel API key access with OpenAI…'
          : accessVerified
            ? 'Tunnel access confirmed. Continue to Step 8; MCP Plugin tool access is checked in Step 9.'
            : androidTunnelSaveReason === 'unauthorized'
              ? 'OpenAI denied access. Check the project and Tunnels: Read + Use permissions, then reopen Step 7 to retry.'
              : androidTunnelSaveReason === 'not_found'
                ? 'This Tunnel ID was not found for the stored API key. Check Step 6 and your project.'
                : androidTunnelSaveReason === 'network_error'
                  ? 'OpenAI could not be reached. Reopen Step 7 to check access again.'
                  : accessDenied
                    ? 'OpenAI verification could not be confirmed. Check the key and retry.'
                    : secureStored
                      ? 'API key saved on this phone. Tunnel access has not yet been confirmed.'
                      : 'Tunnel access is checked automatically after the key is saved.';
    step7State.dataset.state = accessVerified ? 'verified' : accessDenied ? 'error' : 'pending';
  }
  const newKeyInput = document.getElementById('androidTunnelKeyNeededInput') as HTMLInputElement|null;
  const storeAndVerify = document.getElementById('androidTunnelKeyNeededConfirm') as HTMLButtonElement|null;
  if (newKeyInput)
    newKeyInput.placeholder = secureStored ? '•••••••• stored' : 'Paste your restricted API key';
  if (storeAndVerify) {
    const hasNewKey = !!newKeyInput?.value.trim();
    storeAndVerify.hidden = !hasNewKey;
    storeAndVerify.textContent = secureStored ? 'Replace API key securely' : 'Save API key securely';
    storeAndVerify.disabled = !isTauri() || !androidTunnelSavedDraftId ||
      !newKeyInput || newKeyInput.value.trim().length < 8 ||
      androidTunnelCredentialsBusy || androidTunnelVerifyBusy || androidTunnelSaveBusy;
  }
  const keyStatus = document.getElementById('androidTunnelKeyNeededStatus');
  if (keyStatus)
    keyStatus.textContent = androidTunnelCredentialsFeedback ||
      (androidTunnelCredentialsBusy
        ? 'Updating secure Android credential storage…'
        : androidTunnelCredentialsUnavailable
          ? 'Unable to check Android Keystore. No stored key is assumed.'
          : secureStored
            ? 'A key is stored with secure OS credential storage. Type a new one to replace it, or remove the stored key.'
            : 'Your key is encrypted with Android Keystore and is never displayed again.');
  const keyApiLink = document.getElementById('androidTunnelKeyNeededOpenApiKeys') as HTMLButtonElement|null;
  if (keyApiLink) keyApiLink.disabled = !isTauri() || androidTunnelCredentialsBusy;
  for (const id of ['androidTunnelOpenTunnels','androidTunnelOpenApiKeys']) {
    const link = document.getElementById(id) as HTMLButtonElement|null;
    if (link) link.disabled = !isTauri() || androidTunnelCredentialsBusy;
  }
  ($('androidLocalMcpCopyUrl') as HTMLButtonElement).disabled =
    androidLocalMcpBusy || local?.state !== 'listening' || !local.localUrl;
  ($('androidOpenAiTunnelConnect') as HTMLButtonElement).disabled = tunnelKind !== 'openai' || androidOpenAiTunnelBusy || !!tunnelActive ||
    androidTunnelCredentialsBusy || androidTunnelIdBusy || !androidTunnelIdChecked ||
    androidTunnelValidatedId !== tunnelId ||
    !(credentialInput.value.trim().length >= 8 || secureStored) || !isTauri();
  ($('androidOpenAiTunnelDisconnect') as HTMLButtonElement).disabled = androidOpenAiTunnelBusy || !tunnelActive || !isTauri();
  renderAndroidMcpAccess();
  renderAndroidWorkspace();
}
function chooseAndroidCosSetupStep(step: AndroidCosSetupStep): void {
  if (androidCosSetupScanInFlight) androidCosSetupUserSelectedDuringScan = true;
  androidCosSetupAutoFollow = false;
  if (step === 'access' && androidCosFivePermissionsConfirmed())
    androidCosAccessAutoAdvanceArmed = false;
  if (step !== 'extension') {
    const manager = $('androidCosExtensionManager') as HTMLDialogElement;
    if (manager.open) manager.close();
  }
  androidCosSetupSelected = step;
  renderAndroidCosSetup();
  // The ten navigation tiles can be taller than one phone viewport.
  // After a deliberate tap, bring the selected panel into view; leave native
  // auto-selection and the user's manual scrolling untouched.
  const target = step === 'access' ? $('storageAccessSection') : $('androidCosSetupCurrent');
  if (!target.classList.contains('hidden')) {
    const scroller = $('androidSettingsContent');
    const top = scroller.scrollTop + target.getBoundingClientRect().top -
      scroller.getBoundingClientRect().top - 12;
    scroller.scrollTo({top:Math.max(0,top),behavior:'auto'});
  }
  if (step === 'extension') void refreshAndroidCosExtensions();
  if (step === 'browser') void refreshAndroidCosOAuthStatus();
  if (step === 'folder') {
    void refreshAndroidMainStorageStatus();
    void refreshAndroidCosFolderGrant();
  }
  if (step === 'tools') {
    void refreshAndroidMcpAccess();
    if (androidAutomationShowing()) void refreshAndroidAutomationSettings(true);
  }
  if (step === 'tunnel') {
    void refreshAndroidTunnelCredentials();
    void refreshAndroidTunnelSavedId();
    void checkAndroidTunnelId();
  }
  if (step === 'key') {
    void checkSavedTunnelKeyOnStep7();
  }
  if (step === 'connect') {
    void checkSavedTunnelKeyOnStep7();
    void refreshAndroidOpenAiTunnel(true);
  }
  if (step === 'mcpPlugin') {
    void refreshAndroidTunnelSavedId();
    void refreshAndroidTunnelCredentials();
    void refreshAndroidOpenAiTunnel(true);
  }
}
// Opening Setup inspects *every* stage using the existing native receipts.
// It never presses Connect, requests permissions, accepts an OAuth prompt or
// marks a previously verified tool call as fresh. A manual tab choice made
// while checks are running takes precedence over the automatic selection.
async function scanAndroidCosSetupOnOpen():Promise<void> {
  if (!nativeAndroidRuntime || !isTauri()) return;
  // A Settings reentry or permission callback may arrive while a prior
  // read-only scan is active. That same scan will still publish fresh results;
  // a second overlapping scan would only prolong the 'Checking' state.
  if (androidCosSetupScanInFlight) {
    renderAndroidCosSetup();
    return;
  }
  const epoch = ++androidCosSetupScanEpoch;
  const nativeResults = androidCosSetupOrder.map(step => androidCosSetupResult(step));
  const currentlyComplete = androidCosAllStagesComplete(nativeResults);
  // A previously completed Setup should open straight to Ready, not obscure
  // the wizard with an unnecessary full-screen 'Checking setup status' spinner.
  // A stored hint suppresses only the overlay on cold start; it is never
  // allowed to manufacture a Verified step or a trusted permission receipt.
  androidCosSetupQuietScan = currentlyComplete || androidCosSetupPreviouslyComplete;
  androidCosSetupHeldVerifiedResults = currentlyComplete ? nativeResults : null;
  androidCosSetupScanInFlight = true;
  androidCosSetupUserSelectedDuringScan = false;
  androidCosSetupAutoFollow = true;
  androidCosAccessAutoAdvanceArmed = false;
  if (androidCosSetupQuietScan) androidCosSetupSelected = 'ready';
  renderAndroidCosSetup();
  try {
    await Promise.allSettled([
      refreshStorageAccess(),
      refreshAndroidAccessibility(true),
      refreshAndroidOptionalPermissions(),
      refreshAndroidMainStorageStatus(),
      refreshAndroidCosFolderGrant(),
      refreshAndroidCosExtensions(),
      refreshAndroidCosOAuthStatus(),
      refreshAndroidMcpAccess(),
      refreshAndroidTunnelSavedId(),
      refreshAndroidTunnelCredentials(),
      refreshAndroidOpenAiTunnel(true)
    ]);
    if (epoch !== androidCosSetupScanEpoch) return;
    await refreshAndroidTunnelStoredAccess();
    if (epoch !== androidCosSetupScanEpoch) return;
  } finally {
    if (epoch === androidCosSetupScanEpoch) {
      const freshResults = androidCosSetupOrder.map(step => androidCosSetupResult(step));
      // The UI hint is updated only from fresh native receipts after the
      // scan, so revoked OS permission/tunnel access invalidates it.
      rememberAndroidCosSetupCompletion(androidCosAllStagesComplete(freshResults));
      androidCosSetupHeldVerifiedResults = null;
      androidCosSetupQuietScan = false;
      androidCosSetupScanInFlight = false;
      const panel = $('settingsPanel');
      if (!androidCosSetupUserSelectedDuringScan &&
          !panel.classList.contains('hidden') && panel.dataset.settingsView === 'setup') {
        androidCosSetupSelected = androidCosFirstUnresolvedStep(freshResults);
        $('androidSettingsContent').scrollTop = 0;
      }
      renderAndroidCosSetup();
    }
  }
}

// The native GeckoView profile is the only source of installed-extension
// truth. This is a read-only check; opening Step 2 does not start any tab or
// install an add-on. An unknown/failed check must never display "Installed".
function validateAndroidCosExtensions(value: unknown): AndroidGeckoExtension[] {
  if (!value || typeof value !== 'object') throw Error('Invalid extension receipt');
  const receipt = value as {runtimeStarted?:unknown;extensions?:unknown};
  if (receipt.runtimeStarted !== true || !Array.isArray(receipt.extensions) ||
      receipt.extensions.length > 40) throw Error('Native extension list unavailable');
  const ids = new Set<string>();
  return receipt.extensions.map((entry:unknown) => {
    if (!entry || typeof entry !== 'object') throw Error('Invalid extension entry');
    const item = entry as Record<string,unknown>;
    if (typeof item.id !== 'string' || !item.id.trim() || item.id.length > 200 ||
        typeof item.name !== 'string' || item.name.length > 150 ||
        typeof item.version !== 'string' || item.version.length > 50 ||
        typeof item.enabled !== 'boolean' || typeof item.builtIn !== 'boolean' ||
        ids.has(item.id)) throw Error('Invalid native extension metadata');
    ids.add(item.id);
    const checkedStrings = (field:unknown):string[] => {
      if (field === undefined) return [];
      if (!Array.isArray(field) || field.length > 64 ||
          field.some(value => typeof value !== 'string' || value.length > 300))
        throw Error('Invalid extension permission metadata');
      return field;
    };
    return {id:item.id,name:item.name,version:item.version,
      enabled:item.enabled,builtIn:item.builtIn,
      requiredPermissions:checkedStrings(item.requiredPermissions),
      requiredOrigins:checkedStrings(item.requiredOrigins)};
  });
}
function renderAndroidCosExtensionManager(): void {
  const dialog = $('androidCosExtensionManager') as HTMLDialogElement;
  if (!dialog.open) return;
  const list = $('androidCosExtensionList');
  list.replaceChildren();
  const search = ($('androidCosExtensionSearch') as HTMLInputElement).value.trim().toLocaleLowerCase();
  const visible = androidCosExtensions?.filter(extension =>
    (extension.name + ' ' + extension.id + ' ' + extension.version).toLocaleLowerCase().includes(search)) ?? [];
  $('androidCosExtensionCount').textContent = androidCosExtensions
    ? search ? String(visible.length) + ' of ' + androidCosExtensions.length
      : String(androidCosExtensions.length) + ' installed'
    : 'Unknown';
  ($('androidCosExtensionRefresh') as HTMLButtonElement).disabled = androidCosExtensionBusy;
  $('androidCosExtensionManagerSummary').textContent = androidCosExtensionBusy
    ? 'Checking GeckoView extensions…'
    : androidCosExtensionFeedback ||
      (androidCosExtensions
        ? `${androidCosExtensions.length} installed in this app-owned browser profile`
        : 'Unable to verify installed extensions');
  if (!visible.length) {
    const empty = document.createElement('div');
    empty.className = 'android-cos-extension-empty';
    const illustration = document.createElement('span');
    illustration.innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 3H5a2 2 0 0 0-2 2v4h1a3 3 0 0 1 0 6H3v4a2 2 0 0 0 2 2h4v-1a3 3 0 0 1 6 0v1h4a2 2 0 0 0 2-2v-4h-1a3 3 0 0 1 0-6h1V5a2 2 0 0 0-2-2h-4v1a3 3 0 0 1-6 0V3Z"/></svg>';
    const title = document.createElement('strong');
    title.textContent = !androidCosExtensions ? 'Could not load extensions' :
      search && androidCosExtensions.length ? 'No matching extensions' : 'No extensions installed';
    const description = document.createElement('p');
    description.textContent = !androidCosExtensions ? 'Refresh to check the native browser again.' :
      search && androidCosExtensions.length ? 'Try searching for a different name or extension ID.' :
      'Use Install Extension in Setup to add the included test extension with your approval.';
    empty.append(illustration,title,description);
    list.append(empty);
    return;
  }
  for (const extension of visible) {
    const card = document.createElement('article');
    card.className = 'android-cos-extension-card';
    card.dataset.enabled = String(extension.enabled);
    const header = document.createElement('div');
    header.className = 'android-cos-extension-card-header';
    const icon = document.createElement('span');
    icon.className = 'android-cos-extension-card-icon';
    icon.innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 3H5a2 2 0 0 0-2 2v4h1a3 3 0 0 1 0 6H3v4a2 2 0 0 0 2 2h4v-1a3 3 0 0 1 6 0v1h4a2 2 0 0 0 2-2v-4h-1a3 3 0 0 0 0-6h1V5a2 2 0 0 0-2-2h-4v1a3 3 0 0 0-6 0V3Z"/></svg>';
    const headingGroup = document.createElement('div');
    headingGroup.className = 'android-cos-extension-card-label';
    const heading = document.createElement('strong');
    heading.textContent = extension.name || extension.id;
    const state = document.createElement('span');
    state.className = 'android-cos-extension-card-state';
    state.textContent = extension.enabled ? 'Enabled' : 'Disabled';
    headingGroup.append(heading,state);
    header.append(icon,headingGroup);
    const detail = document.createElement('small');
    detail.textContent = `${extension.version} · ${extension.enabled ? 'Enabled' : 'Disabled'} · ${extension.builtIn ? 'Bundled' : 'GeckoView'}`;
    const id = document.createElement('small');
    id.className = 'android-cos-extension-card-id';
    id.textContent = extension.id;
    const extra = document.createElement('div');
    extra.className = 'android-cos-extension-card-details';
    extra.hidden = true;
    const permissionLine = document.createElement('p');
    permissionLine.textContent = 'Permissions: ' +
      (extension.requiredPermissions.length ? extension.requiredPermissions.join(', ').slice(0,2048) : 'None');
    const originLine = document.createElement('p');
    originLine.textContent = 'Website access: ' +
      (extension.requiredOrigins.length ? extension.requiredOrigins.join(', ').slice(0,2048) : 'None');
    extra.append(permissionLine,originLine);
    const actions = document.createElement('div');
    actions.className = 'android-cos-extension-card-actions';
    const detailsButton = document.createElement('button');
    detailsButton.type = 'button';
    detailsButton.textContent = 'Details';
    detailsButton.setAttribute('aria-expanded','false');
    detailsButton.addEventListener('click',() => {
      extra.hidden = !extra.hidden;
      detailsButton.setAttribute('aria-expanded',String(!extra.hidden));
    });
    actions.append(detailsButton);
    for (const [action,label] of [
      [extension.enabled ? 'disable' : 'enable',extension.enabled ? 'Disable' : 'Enable'],
      ['remove','Remove']
    ] as const) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'outline';
      button.textContent = label;
      button.disabled = androidCosExtensionBusy;
      button.addEventListener('click',() =>
        void manageAndroidCosExtension(extension.id,action));
      actions.append(button);
    }
    card.append(header,detail,id,extra,actions);
    list.append(card);
  }
}
async function refreshAndroidCosExtensions(): Promise<void> {
  if (!nativeAndroidRuntime || !isTauri() || androidCosExtensionBusy) return;
  androidCosExtensionBusy = true;
  const epoch = ++androidCosExtensionRefreshEpoch;
  androidCosExtensionFeedback = '';
  renderAndroidCosSetup();
  renderAndroidCosExtensionManager();
  try {
    const response = await invoke<unknown>('custom_gecko_list_extensions');
    if (epoch !== androidCosExtensionRefreshEpoch) return;
    androidCosExtensions = validateAndroidCosExtensions(response);
    androidCosExtensionsUnavailable = false;
  } catch {
    if (epoch !== androidCosExtensionRefreshEpoch) return;
    androidCosExtensions = null;
    androidCosExtensionsUnavailable = true;
    androidCosExtensionFeedback = 'Unable to check extensions. Select Open Extensions to retry.';
  } finally {
    if (epoch === androidCosExtensionRefreshEpoch) {
      androidCosExtensionBusy = false;
      renderAndroidCosSetup();
      renderAndroidCosExtensionManager();
    }
  }
}
async function manageAndroidCosExtension(id:string,action:'enable'|'disable'|'remove'):Promise<void> {
  if (androidCosExtensionBusy || !isTauri() ||
      !androidCosExtensions?.some(extension => extension.id === id)) return;
  androidCosExtensionBusy = true;
  // An explicit install/enable/disable/remove invalidates prior profile proof.
  androidCosExtensions = null;
  androidCosExtensionsUnavailable = false;
  renderAndroidCosExtensionManager();
  try {
    const result = await invoke<{updated:boolean;id?:string}>('custom_gecko_manage',
      {id,action,userInitiated:true});
    if (result?.updated === true && result.id !== id)
      throw Error('Unexpected native extension identity');
    androidCosExtensionFeedback = result?.updated === true ? '' : 'Extension action cancelled.';
  } catch {
    androidCosExtensionFeedback = 'Extension action failed; check its current state.';
  } finally {
    androidCosExtensionBusy = false;
    await refreshAndroidCosExtensions();
  }
}
if (nativeAndroidRuntime) {
  $('androidCosExtensionOpen').addEventListener('click',() => {
    if (androidCosSetupSelected !== 'extension' || !androidCosFivePermissionsConfirmed()) return;
    const manager = $('androidCosExtensionManager') as HTMLDialogElement;
    if (!manager.open) manager.showModal();
    renderAndroidCosExtensionManager();
    void refreshAndroidCosExtensions();
  });
  $('androidCosExtensionDone').addEventListener('click',() =>
    ($('androidCosExtensionManager') as HTMLDialogElement).close());
  $('androidCosExtensionRefresh').addEventListener('click',() =>
    void refreshAndroidCosExtensions());
  $('androidCosExtensionSearch').addEventListener('input',renderAndroidCosExtensionManager);
  $('androidCosExtensionInstall').addEventListener('click',async () => {
    if (!isTauri() || androidCosExtensionBusy ||
        androidCosSetupSelected !== 'extension' || !androidCosFivePermissionsConfirmed() ||
        !androidCosExtensions || androidCosExtensions.some(ext => ext.id === androidCosProbeExtensionId))
      return;
    androidCosExtensionBusy = true;
    androidCosExtensions = null;
    androidCosExtensionsUnavailable = false;
    renderAndroidCosSetup();
    try {
      // Native prompts for explicit consent. Only the bundled original test
      // WebExtension is supported; never present an arbitrary Chrome/XPI import.
      const receipt = await invoke<{installed:boolean;id?:string}>('custom_gecko_install_probe',
        {userInitiated:true});
      if (!receipt || typeof receipt.installed !== 'boolean' ||
          (receipt.installed && receipt.id !== androidCosProbeExtensionId))
        throw Error('Invalid install receipt');
      androidCosExtensionFeedback = receipt.installed ? '' : 'Installation cancelled.';
    } catch {
      androidCosExtensionFeedback = 'Installation unsuccessful. No extension is marked installed.';
    } finally {
      androidCosExtensionBusy = false;
      await refreshAndroidCosExtensions();
    }
  });
}
if (nativeAndroidRuntime) {
  // Step 5 fine-tuning updates only a user-requested policy. The backend has
  // the final word on effective tools, and does not expose unsupported routes.
  $('androidMcpAccessPresets').addEventListener('change', (event:Event) => {
    const target = event.target;
    if (!(target instanceof HTMLInputElement) || target.name !== 'androidMcpAccessPreset')
      return;
    if (!['read_only','files_terminal','full'].includes(target.value)) {
      renderAndroidMcpAccess();
      return;
    }
    void configureAndroidMcpAccess(target.value as AndroidMcpPreset,
      androidMcpPresetMask(target.value as AndroidMcpPreset), true);
  });
  $('androidMcpAccessPresets').addEventListener('change', (event:Event) => {
    const target = event.target;
    if (!(target instanceof HTMLInputElement) ||
        !target.closest('.android-mcp-capability-list') || !androidMcpAccessSnapshot) return;
    const key = target.dataset.mcpCapability as AndroidMcpCapability|undefined;
    if (!key || !androidMcpCapabilityNames.includes(key)) return;
    void configureAndroidMcpAccess('custom',{
      ...androidMcpAccessSnapshot.requested,[key]:target.checked
    });
  });
  const groups:{id:string;keys:readonly AndroidMcpCapability[]}[] = [
    {id:'androidMcpAllowLook',keys:androidMcpReadCapabilities},
    {id:'androidMcpAllowChange',keys:androidMcpWriteCapabilities},
    {id:'androidMcpAllowBrowser',keys:['browserControl']},
    {id:'androidMcpAllowTerminal',keys:['terminal']},
  ];
  for (const group of groups) {
    $(group.id).addEventListener('change', (event:Event) => {
      const target = event.target;
      if (!(target instanceof HTMLInputElement) || !androidMcpAccessSnapshot) return;
      const requested = {...androidMcpAccessSnapshot.requested};
      for (const key of group.keys) requested[key] = target.checked;
      void configureAndroidMcpAccess('custom',requested);
    });
  }
  // Run programs: keep the terminal MCP permission checkbox independent of
  // the native command-rule policy. Never treat a textarea edit as execution.
  $('androidMcpRunProgramsGroup').addEventListener('toggle', () => {
    if (($('androidMcpRunProgramsGroup') as HTMLDetailsElement).open &&
        ($('androidMcpRunProgramsGroup') as HTMLDetailsElement)
          .closest<HTMLDetailsElement>('.android-mcp-fine-tune')?.open)
      void refreshAndroidMcpCommandRules();
  });
  // An already-expanded Run programs group may have been hidden when the user
  // closed Fine-tune. Reopening must recheck native policy, not trust its old UI.
  document.querySelector<HTMLDetailsElement>(
    '#androidMcpAccessPresets .android-mcp-fine-tune')?.addEventListener('toggle',event => {
    if ((event.currentTarget as HTMLDetailsElement).open &&
        ($('androidMcpRunProgramsGroup') as HTMLDetailsElement).open)
      void refreshAndroidMcpCommandRules();
  });
  for (const id of ['androidMcpCommandRulesEnabled','androidMcpCommandRulesInput'])
    $(id).addEventListener(id.endsWith('Input') ? 'input' : 'change',() => {
      androidMcpCommandRulesFeedback = '';
      renderAndroidMcpCommandRules();
    });
  $('androidMcpCommandRulesModes').addEventListener('change',() => {
    androidMcpCommandRulesFeedback = '';
    renderAndroidMcpCommandRules();
  });
  $('androidMcpCommandRulesSave').addEventListener('click',() => {
    void saveAndroidMcpCommandRules();
  });
  renderAndroidMcpCommandRules();
  renderAndroidMcpAccess();
  // The Android onboarding exposes only the Core tunnel. Previously used
  // manual/cloudflared method preferences and unverified localStorage ID
  // drafts never override a native, authenticated Save receipt.
  ($('androidOpenAiTunnelMethodSelect') as HTMLSelectElement).value = 'openai';
  try {
    localStorage.removeItem('mcp-agent-ai-android-tunnel-method-v1');
    localStorage.removeItem('mcp-agent-ai-android-core-tunnel-id-v1');
  } catch { /* Optional legacy preference cleanup. */ }
  $('androidOpenAiTunnelMethodSelect').addEventListener('change', () => {
    // COS method selection does not equate to a successful transport.
    try { localStorage.setItem('mcp-agent-ai-android-tunnel-method-v1', androidCosTunnelKind()); }
    catch { /* No persistent preference available. */ }
    androidOpenAiTunnelFeedback = '';
    renderAndroidCosSetup();
  });
  $('androidLocalMcpCopyUrl').addEventListener('click', async () => {
    if (androidLocalMcpSnapshot?.state !== 'listening' || !androidLocalMcpSnapshot.localUrl) return;
    const url = androidLocalMcpSnapshot.localUrl;
    try {
      await navigator.clipboard.writeText(url);
      androidLocalMcpFeedback = 'Local MCP URL copied. Treat it as a secret.';
    } catch {
      androidLocalMcpFeedback = 'Clipboard unavailable. URL selected for manual copying.';
    }
    renderAndroidCosSetup();
    if (androidLocalMcpFeedback.startsWith('Clipboard unavailable'))
      ($('androidLocalMcpUrl') as HTMLInputElement).select();
  });
  $('androidLocalMcpStart').addEventListener('click', async () => {
    if (androidCosTunnelKind() !== 'manual' || !isTauri() || androidLocalMcpBusy ||
        androidLocalMcpSnapshot?.state !== 'stopped') return;
    androidLocalMcpBusy = true;
    androidLocalMcpFeedback = 'Starting loopback-only MCP server…';
    renderAndroidCosSetup();
    try {
      await invoke('android_local_mcp_start', {userInitiated:true});
      androidLocalMcpFeedback = '';
    } catch {
      androidLocalMcpFeedback = 'Could not start the local MCP server. No endpoint or permission is claimed.';
    } finally {
      androidLocalMcpBusy = false;
      await refreshAndroidLocalMcp();
    }
  });
  $('androidLocalMcpStop').addEventListener('click', async () => {
    if (!isTauri() || androidLocalMcpBusy ||
        androidLocalMcpSnapshot?.state !== 'listening') return;
    androidLocalMcpBusy = true;
    androidLocalMcpFeedback = 'Stopping local MCP server…';
    renderAndroidCosSetup();
    try {
      await invoke('android_local_mcp_stop', {userInitiated:true});
      androidLocalMcpFeedback = '';
    } catch {
      androidLocalMcpFeedback = 'Unable to confirm shutdown. Check the server status again.';
    } finally {
      androidLocalMcpBusy = false;
      await refreshAndroidLocalMcp();
    }
  });
  for (const field of ['androidOpenAiTunnelId','androidOpenAiTunnelKey']) {
    $(field).addEventListener('input', () => { androidOpenAiTunnelFeedback = ''; renderAndroidCosSetup(); });
  }
  $('androidOpenAiTunnelId').addEventListener('input',() => {
    androidTunnelMetadataVerifiedId = '';
    androidTunnelSaveReason = null;
    void checkAndroidTunnelId();
  });
  $('androidTunnelSaveId').addEventListener('click', () => {
    void saveAndroidCoreTunnelId(($('androidOpenAiTunnelId') as HTMLInputElement).value.trim());
  });
  $('androidTunnelKeyNeededInput').addEventListener('input',() => {
    androidTunnelCredentialsFeedback = '';
    renderAndroidCosSetup();
  });
  $('androidTunnelKeyNeededOpenApiKeys').addEventListener('click',async () => {
    if (!isTauri() || androidCosSetupSelected !== 'key') return;
    try {
      const receipt = await invoke<{opened:boolean;kind:string}>(
        'android_tunnel_open_platform_page',{kind:'api_keys',userInitiated:true});
      if (!receipt || receipt.opened !== true || receipt.kind !== 'api_keys')
        throw Error('OpenAI Platform browser did not open');
    } catch {
      androidTunnelCredentialsFeedback =
        'Unable to open API keys. Open OpenAI Platform in your Android browser and choose a Restricted key with Tunnels: Read + Use.';
      renderAndroidCosSetup();
    }
  });
  $('androidTunnelKeyNeededConfirm').addEventListener('click',async () => {
    if (androidCosSetupSelected !== 'key' || !androidTunnelSavedDraftId ||
        androidTunnelVerifyBusy || androidTunnelSaveBusy || androidTunnelCredentialsBusy) return;
    const input = $('androidTunnelKeyNeededInput') as HTMLInputElement;
    const apiKey = input.value.trim();
    input.value = '';
    if (apiKey.length < 8 || apiKey.length > 512) {
      androidTunnelCredentialsFeedback =
        'Paste the full Restricted Tunnel Runtime API key from OpenAI Platform.';
      renderAndroidCosSetup();
      return;
    }
    // A changed credential invalidates any earlier metadata proof immediately.
    androidTunnelMetadataVerifiedId = '';
    androidTunnelSaveReason = null;
    androidTunnelCredentialsBusy = true;
    androidTunnelCredentialsFeedback = '';
    renderAndroidCosSetup();
    let keyStored = false;
    try {
      const receipt = validateAndroidTunnelCredentials(await invoke<unknown>(
        'android_tunnel_credentials_store',{apiKey,userInitiated:true}));
      if (!receipt.stored) throw Error('Key storage not confirmed');
      androidTunnelCredentials = receipt;
      androidTunnelCredentialsUnavailable = false;
      keyStored = true;
    } catch {
      androidTunnelCredentialsFeedback =
        'Secure key storage could not be confirmed. The Core Tunnel ID remains unverified.';
    } finally {
      androidTunnelCredentialsBusy = false;
      await refreshAndroidTunnelCredentials();
      renderAndroidCosSetup();
    }
    if (keyStored) await verifyAndroidSavedCoreTunnel();
  });
  for (const [button,kind] of [
    ['androidTunnelOpenTunnels','tunnels'], ['androidTunnelOpenApiKeys','api_keys']
  ] as const) {
    $(button).addEventListener('click',async () => {
      if (!isTauri() || androidCosSetupSelected !== 'tunnel') return;
      try {
        const result = await invoke<{opened:boolean;kind:string}>('android_tunnel_open_platform_page',
          {kind,userInitiated:true});
        if (!result || result.opened !== true || result.kind !== kind)
          throw Error('Unexpected Android browser receipt');
        androidTunnelCredentialsFeedback = '';
      } catch {
        androidTunnelCredentialsFeedback =
          'Unable to open OpenAI Platform in your Android browser. Check your browser and network.';
      }
      renderAndroidCosSetup();
    });
  }
  $('androidTunnelSaveKey').addEventListener('click',async () => {
    if (!isTauri() || androidCosSetupSelected !== 'tunnel' || androidTunnelCredentialsBusy) return;
    const input = $('androidOpenAiTunnelKey') as HTMLInputElement;
    const apiKey = input.value.trim();
    if (apiKey.length < 8 || apiKey.length > 512) return;
    // Remove from WebView memory immediately. Only the native user-initiated
    // store call may save ciphertext using a non-exportable Android Keystore key.
    input.value = '';
    androidTunnelCredentialsBusy = true;
    androidTunnelCredentialsFeedback = '';
    ++androidTunnelCredentialsEpoch;
    renderAndroidCosSetup();
    try {
      const receipt = validateAndroidTunnelCredentials(await invoke<unknown>(
        'android_tunnel_credentials_store',{apiKey,userInitiated:true}));
      if (!receipt.stored) throw Error('Native credential was not stored');
      androidTunnelCredentials = receipt;
      androidTunnelCredentialsUnavailable = false;
      androidTunnelCredentialsFeedback = '';
    } catch {
      androidTunnelCredentials = null;
      androidTunnelCredentialsUnavailable = true;
      androidTunnelCredentialsFeedback = 'Secure tunnel API key save could not be verified. No credential is assumed stored.';
    } finally {
      androidTunnelCredentialsBusy = false;
      await refreshAndroidTunnelCredentials();
      renderAndroidCosSetup();
    }
  });
  $('androidTunnelRemoveStoredKey').addEventListener('click',async () => {
    if (!isTauri() || androidCosSetupSelected !== 'key' ||
        androidTunnelCredentialsBusy || !androidTunnelCredentials?.stored) return;
    androidTunnelCredentialsBusy = true;
    ++androidTunnelCredentialsEpoch;
    androidTunnelCredentialsFeedback = 'Removing saved tunnel API key and disconnecting this tunnel…';
    renderAndroidCosSetup();
    try {
      const receipt = validateAndroidTunnelCredentials(await invoke<unknown>(
        'android_tunnel_credentials_remove',{userInitiated:true}));
      if (receipt.stored) throw Error('Native key still present');
      androidTunnelCredentials = receipt;
      androidTunnelCredentialsUnavailable = false;
      androidTunnelMetadataVerifiedId = '';
      androidTunnelSaveReason = 'key_required';
      ($('androidOpenAiTunnelKey') as HTMLInputElement).value = '';
      ($('androidTunnelKeyNeededInput') as HTMLInputElement).value = '';
      androidTunnelCredentialsFeedback = 'Saved tunnel key removed from this phone. To revoke it completely, also delete the key in OpenAI Platform.';
      await refreshAndroidOpenAiTunnel(true);
    } catch {
      androidTunnelCredentials = null;
      androidTunnelCredentialsUnavailable = true;
      androidTunnelCredentialsFeedback = 'Could not confirm removal from secure Android storage. Check again.';
    } finally {
      androidTunnelCredentialsBusy = false;
      await refreshAndroidTunnelCredentials();
      renderAndroidCosSetup();
    }
  });
  $('androidOpenAiTunnelConnect').addEventListener('click', async () => {
    if (!isTauri() || androidOpenAiTunnelBusy || androidCosTunnelKind() !== 'openai') return;
    const tunnelId = ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim();
    const keyInput = $('androidOpenAiTunnelKey') as HTMLInputElement;
    const apiKey = keyInput.value.trim();
    if (!androidTunnelIdChecked || tunnelId !== androidTunnelValidatedId ||
        (!androidTunnelCredentials?.stored && apiKey.length < 8) ||
        androidTunnelCredentialsBusy) return;
    keyInput.value = ''; // never store the key in UI, sessionStorage or debug logs
    androidOpenAiTunnelBusy = true;
    ++androidOpenAiTunnelTicket; // Prior tunnel receipt belongs to the old session.
    androidOpenAiTunnelSnapshot = null;
    androidOpenAiTunnelUnavailable = false;
    androidOpenAiTunnelFeedback = 'Starting outbound OpenAI tunnel; tool access is not verified.';
    renderAndroidCosSetup();
    try {
      if (apiKey.length >= 8)
        await invoke('android_openai_tunnel_connect', {tunnelId, apiKey, userInitiated:true});
      else
        await invoke('android_openai_tunnel_connect_stored', {tunnelId,userInitiated:true});
      androidOpenAiTunnelFeedback = '';
    } catch {
      androidOpenAiTunnelFeedback = 'Tunnel start rejected. Check tunnel ID and runtime permissions; no tool call is verified.';
    } finally {
      androidOpenAiTunnelBusy = false;
      await refreshAndroidOpenAiTunnel(true);
    }
  });
  $('androidOpenAiTunnelDisconnect').addEventListener('click', async () => {
    if (!isTauri() || androidOpenAiTunnelBusy) return;
    androidOpenAiTunnelBusy = true;
    ++androidOpenAiTunnelTicket;
    androidOpenAiTunnelSnapshot = null;
    androidOpenAiTunnelUnavailable = false;
    androidOpenAiTunnelFeedback = 'Stopping tunnel…';
    renderAndroidCosSetup();
    try {
      await invoke('android_openai_tunnel_disconnect', {userInitiated:true});
      androidOpenAiTunnelFeedback = '';
    } catch {
      androidOpenAiTunnelFeedback = 'Could not verify tunnel shutdown. Check status again.';
    } finally {
      androidOpenAiTunnelBusy = false;
      await refreshAndroidOpenAiTunnel(true);
    }
  });
  $('androidCosChatgptConnect').addEventListener('click', async () => {
    if (androidCosSetupSelected !== 'connect' || !isTauri() ||
        androidOpenAiTunnelBusy || androidTunnelVerifyBusy || androidTunnelCredentialsBusy ||
        androidCosTunnelKind() !== 'openai') return;
    const id = androidTunnelSavedDraftId;
    if (!id || id !== ($('androidOpenAiTunnelId') as HTMLInputElement).value.trim() ||
        androidTunnelMetadataVerifiedId !== id || !androidTunnelCredentials?.stored ||
        !androidTunnelCredentials.secure ||
        ['connecting','polling','tool_verified'].includes(androidOpenAiTunnelSnapshot?.state || '')) return;
    // Connecting from Step 8 is a setup action. Follow to Step 9 once the
    // native status confirms a connection, even if Step 8 was opened manually.
    androidCosSetupAutoFollow = true;
    const priorState = androidOpenAiTunnelSnapshot?.state;
    androidOpenAiTunnelBusy = true;
    ++androidOpenAiTunnelTicket;
    androidOpenAiTunnelSnapshot = null;
    androidOpenAiTunnelFeedback = 'Connecting the native OpenAI Secure MCP Tunnel…';
    renderAndroidCosSetup();
    try {
      // The Rust native tunnel rejects a second connection while in Error.
      // A fresh user-initiated retry explicitly resets that prior session.
      if (priorState === 'error')
        await invoke('android_openai_tunnel_disconnect',{userInitiated:true});
      await invoke('android_openai_tunnel_connect_stored',{tunnelId:id,userInitiated:true});
      androidOpenAiTunnelFeedback = '';
    } catch {
      androidOpenAiTunnelFeedback = 'Secure MCP Tunnel connection failed. Recheck the Core ID, Restricted API key and OpenAI permissions.';
    } finally {
      androidOpenAiTunnelBusy = false;
      await refreshAndroidOpenAiTunnel(true);
    }
  });
  $('androidCosTunnelDisconnect').addEventListener('click', async () => {
    if (androidCosSetupSelected !== 'connect' || !isTauri() || androidOpenAiTunnelBusy ||
        (!['connecting','polling','tool_verified'].includes(androidOpenAiTunnelSnapshot?.state || '') &&
          androidOpenAiTunnelSnapshot?.desiredConnected !== true)) return;
    androidOpenAiTunnelBusy = true;
    ++androidOpenAiTunnelTicket;
    androidOpenAiTunnelSnapshot = null;
    androidOpenAiTunnelFeedback = 'Disconnecting the native Secure MCP Tunnel…';
    renderAndroidCosSetup();
    try {
      await invoke('android_openai_tunnel_disconnect',{userInitiated:true});
      androidOpenAiTunnelFeedback = '';
    } catch {
      androidOpenAiTunnelFeedback = 'Unable to confirm tunnel disconnect. Reopen Step 8 to refresh status.';
    } finally {
      androidOpenAiTunnelBusy = false;
      await refreshAndroidOpenAiTunnel(true);
    }
  });
  $('androidCosMcpPhoneName').addEventListener('input', () => {
    const input = $('androidCosMcpPhoneName') as HTMLInputElement;
    const label = input.value.trim();
    if (validAndroidCosMcpPhoneName(label)) {
      try { localStorage.setItem(androidCosMcpPhoneStorageKey,label); }
      catch { /* Copyable identity remains usable without storage. */ }
    }
    $('androidCosMcpPluginCopyStatus').textContent = '';
    renderAndroidCosMcpPluginIdentity();
  });
  for (const [button,field,description] of [
    ['androidCosMcpCopyName','androidCosMcpName','Name'],
    ['androidCosMcpCopyDescription','androidCosMcpDescription','Description']
  ] as const) {
    $(button).addEventListener('click',async () => {
      if (androidCosSetupSelected !== 'mcpPlugin') return;
      const input = $('androidCosMcpPhoneName') as HTMLInputElement;
      if (!validAndroidCosMcpPhoneName(input.value.trim())) return;
      const target = $(field) as HTMLInputElement|HTMLTextAreaElement;
      const value = target.value;
      if (!value) return;
      try {
        await navigator.clipboard.writeText(value);
        $('androidCosMcpPluginCopyStatus').textContent = `${description} copied to clipboard.`;
      } catch {
        target.focus({preventScroll:true});
        target.select();
        $('androidCosMcpPluginCopyStatus').textContent =
          `Clipboard unavailable. ${description} is selected for manual copying.`;
      }
    });
  }
  $('androidCosMcpPluginOpen').addEventListener('click',async () => {
    if (androidCosSetupSelected !== 'mcpPlugin' || !isTauri() || androidCosMcpOpenBusy) return;
    androidCosMcpOpenBusy = true;
    renderAndroidCosSetup();
    try {
      const receipt = await invoke<{opened:boolean;kind:string}>('android_tunnel_open_platform_page',
        {kind:'chatgpt_plugins',userInitiated:true});
      if (receipt?.opened !== true || receipt.kind !== 'chatgpt_plugins')
        throw Error('ChatGPT Plugins browser not confirmed');
      $('androidCosMcpPluginCopyStatus').textContent =
        'ChatGPT Plugins opened. Return here after adding the Core Plugin.';
    } catch {
      $('androidCosMcpPluginCopyStatus').textContent =
        'Unable to open ChatGPT Plugins. Open https://chatgpt.com/plugins in your browser.';
    } finally {
      androidCosMcpOpenBusy = false;
      renderAndroidCosSetup();
    }
  });
  $('androidCosSetupSteps').addEventListener('click', (event:MouseEvent) => {
    const button = (event.target as Element).closest<HTMLButtonElement>('[data-cos-setup-step]');
    const step = button?.dataset.cosSetupStep as AndroidCosSetupStep | undefined;
    if (step && androidCosSetupOrder.includes(step)) chooseAndroidCosSetupStep(step);
  });
  $('androidCosBrowserOpen').addEventListener('click', async () => {
    if (!isTauri() || androidCosSetupSelected !== 'browser' ||
        !androidCosFivePermissionsConfirmed() || androidCosOAuthBusy ||
        androidCosOAuthStatus?.supported !== true ||
        androidCosOAuthReadyToBring() || androidCosOAuthLinked()) return;
    androidCosOAuthBusy = true;
    ++androidCosOAuthEpoch;
    androidCosOAuthStatus = null;
    androidCosOAuthUnavailable = false;
    androidCosOAuthFeedback = '';
    renderAndroidCosSetup();
    try {
      // Native PKCE creates a private loopback callback and opens OpenAI's
      // official system-browser approval for this named Android app.
      await invoke('android_chatgpt_oauth_start',{userInitiated:true});
    } catch {
      androidCosOAuthFeedback = 'Could not open the OpenAI authorization screen. Retry Sign in to ChatGPT.';
    } finally {
      androidCosOAuthBusy = false;
      await refreshAndroidCosOAuthStatus(true);
      renderAndroidCosSetup();
    }
  });
  $('androidCosBrowserBring').addEventListener('click', async () => {
    if (!isTauri() || androidCosSetupSelected !== 'browser' ||
        !androidCosFivePermissionsConfirmed() || androidCosOAuthBusy ||
        !androidCosOAuthReadyToBring() || androidCosOAuthLinked()) return;
    androidCosOAuthBusy = true;
    ++androidCosOAuthEpoch;
    androidCosOAuthFeedback = 'Verifying the approved OpenAI account and ChatGPT plan permission…';
    renderAndroidCosSetup();
    try {
      await invoke('android_chatgpt_oauth_bring',{userInitiated:true});
      await refreshAndroidCosOAuthStatus(true);
      if (!androidCosOAuthLinked()) throw Error('Linked session could not be confirmed');
      androidCosOAuthFeedback = '';
    } catch {
      androidCosOAuthFeedback = 'Bring Session could not verify OpenAI plan permission. Check approval and retry.';
    } finally {
      androidCosOAuthBusy = false;
      await refreshAndroidCosOAuthStatus(true);
      if (androidCosSetupSelected === 'browser' && androidCosOAuthLinked())
        chooseAndroidCosSetupStep('folder');
      else renderAndroidCosSetup();
    }
  });
  $('androidCosFolderBrowse').addEventListener('click', () => {
    if (!androidMainStorageVerified() || androidMainFolderBusy) return;
    if (androidMainFolderOpen) {
      androidMainFolderOpen = false;
      renderAndroidCosSetup();
    } else void loadAndroidMainFolder([]);
  });
  $('androidCosFolderUp').addEventListener('click', () => {
    if (!androidMainFolderBusy && androidMainFolderPath.length)
      void loadAndroidMainFolder(androidMainFolderPath.slice(0,-1));
  });
  $('androidCosFolderReload').addEventListener('click', () => {
    if (!androidMainFolderBusy) void loadAndroidMainFolder([...androidMainFolderPath]);
  });
  async function saveAndroidMainFolder(reset:boolean):Promise<void> {
    if (!nativeAndroidRuntime || !isTauri() || !androidMainStorageVerified() ||
        androidMainSelectionBusy || !androidMainSelection ||
        (!reset && (androidMainFolderBusy || !androidMainFolderEntries))) return;
    const path = reset ? [] : [...androidMainFolderPath];
    if (!androidMainFolderValidPath(path)) return;
    androidMainSelectionBusy = true;
    ++androidMainSelectionTicket; // Ignore stale pre-save status receipts.
    androidCosFolderFeedback = reset ? 'Resetting to Internal storage…' : 'Saving selected folder…';
    renderAndroidCosSetup();
    try {
      const result = await invoke<AndroidMainSelection>(
        reset || path.length === 0 ? 'android_main_storage_reset' : 'android_main_storage_select',
        reset || path.length === 0 ? {} : {path});
      if (!androidMainSelectionValid(result) || result.path.join('/') !== path.join('/'))
        throw Error('Android did not confirm the chosen folder');
      androidMainSelection = result;
      androidMainFolderOpen = false;
      androidMainFolderEntries = null;
      androidMainFolderPath = [];
      androidCosFolderFeedback = result.selected
        ? 'Selected folder saved: ' + androidMainPathString(result.path)
        : 'Folder reset to /storage/emulated/0.';
    } catch {
      androidMainSelection = null;
      androidCosFolderFeedback = 'Folder change could not be verified. Check the saved folder status.';
    } finally {
      androidMainSelectionBusy = false;
      await refreshAndroidMainSelection();
      renderAndroidCosSetup();
    }
  }
  $('androidCosFolderSet').addEventListener('click', () => void saveAndroidMainFolder(false));
  $('androidCosFolderReset').addEventListener('click', () => void saveAndroidMainFolder(true));
  $('androidCosFolderNext').addEventListener('click', () => {
    if (androidCosSetupSelected === 'folder' && androidMainStorageVerified())
      chooseAndroidCosSetupStep('tools');
  });
  $('androidCosSetupStepAction').addEventListener('click', () => {
    const targets:Record<AndroidCosSetupStep,string|null> = {
      extension:null,
      browser:null,
      folder:null,access:'storageAccessSection',tools:'androidCosAccessPanel',
      tunnel:'androidCosTunnelMethod',key:null,connect:null,
      mcpPlugin:null,
      ready:'chat'
    };
    const target = targets[androidCosSetupSelected];
    if (target === 'chat') { setSettings(false); return; }
    if (target) $(target).scrollIntoView({behavior:'auto',block:'start'});
  });
}
let androidPermissionRefreshId = 0;
async function refreshStorageAccess(freshAfterPending = false):Promise<void> {
  if (!nativeAndroidRuntime) return;
  if (androidPermissionRefreshInFlight) {
    await androidPermissionRefreshInFlight;
    if (!freshAfterPending) return;
  }
  // Focus and visibilitychange can fire for the same Android return event.
  // One request at a time prevents an older verified OS receipt being dropped
  // solely because a redundant later request failed.
  const request = refreshStorageAccessOnce();
  androidPermissionRefreshInFlight = request;
  try { await request; }
  finally {
    if (androidPermissionRefreshInFlight === request) {
      androidPermissionRefreshInFlight = null;
      renderAndroidCosSetup();
    }
  }
}
async function refreshStorageAccessOnce():Promise<void> {
  if (!nativeAndroidRuntime) return;
  androidPermissionRefreshPending = true;
  if (!androidCosSetupScanInFlight) {
    // The Setup scanner already awaits each of these receipts explicitly.
    // A second forced refresh used to make a completed Setup flash Checking.
    void refreshAndroidMainStorageStatus(true);
    void refreshAndroidOpenAiTunnel(true);
    void refreshAndroidLocalMcp(true);
    void refreshAndroidMcpAccess();
    void refreshAndroidAccessibility(true);
    void refreshAndroidOptionalPermissions();
  }
  // Status checks are read-only. Snapshot/capture/window discovery are never
  // triggered by entering Settings or returning from Android Settings.
  const refreshId = ++androidPermissionRefreshId;
  const statusIds: Record<AndroidPermission, string> = {
    files: 'storageAccessState', overlay: 'overlayAccessState',
    notifications: 'notificationAccessState', battery: 'batteryAccessState',
  };
  const buttons = [...document.querySelectorAll<HTMLButtonElement>('[data-permission-request]')];
  if (!isTauri()) {
    for (const key of androidPermissionKeys) {
      $(statusIds[key]).textContent = 'Preview only';
      $(statusIds[key]).dataset.supported = 'false';
      $(statusIds[key]).dataset.granted = 'false';
    }
    for (const button of buttons) button.disabled = true;
    androidPermissionRefreshPending = false;
    renderAndroidCosSetup();
    return;
  }
  for (const key of androidPermissionKeys) $(statusIds[key]).textContent = 'Checking…';
  for (const button of buttons) button.disabled = true;
  try {
    const result = await invoke<AndroidPermissionSnapshot>('android_permissions_status');
    if (refreshId !== androidPermissionRefreshId) return;
    for (const key of androidPermissionKeys) {
      const state = result?.[key];
      if (!state || typeof state.supported !== 'boolean' || typeof state.granted !== 'boolean')
        throw Error('Invalid native permission status');
      $(statusIds[key]).textContent = !state.supported ? 'Not supported' :
        key === 'battery' ? state.granted ? 'Unrestricted' : 'Optimized' :
        state.granted ? 'Allowed' : 'Not allowed';
      $(statusIds[key]).dataset.granted = String(state.supported && state.granted);
      $(statusIds[key]).dataset.supported = String(state.supported);
      const button = buttons.find(item => item.dataset.permissionRequest === key);
      if (button) {
        // A single action per persistent permission: Android must confirm a
        // grant before this button changes to disabled "Allowed". Revocation
        // on a later OS status refresh makes the same Allow action usable again.
        button.disabled = !state.supported || state.granted;
        button.textContent = !state.supported ? 'Not supported' : state.granted ? 'Allowed' : 'Allow';
      }
    }
    androidPermissionStatusUnavailable = false;
    androidPermissionRefreshPending = false;
    renderAndroidCosSetup();
  } catch {
    if (refreshId !== androidPermissionRefreshId) return;
    for (const key of androidPermissionKeys) {
      $(statusIds[key]).textContent = 'Unknown';
      $(statusIds[key]).dataset.granted = 'false';
      $(statusIds[key]).dataset.supported = 'unknown';
    }
    for (const button of buttons) button.disabled = true;
    androidPermissionStatusUnavailable = true;
    androidPermissionRefreshPending = false;
    renderAndroidCosSetup();
  }
}
let permissionRequestInProgress = false;
$('storageAccessSection').addEventListener('click', async (event: MouseEvent) => {
  const button = (event.target as Element).closest<HTMLButtonElement>('[data-optional-permission-request]');
  if (!button || button.disabled || androidOptionalPermissionRequestBusy ||
      !nativeAndroidRuntime || !isTauri()) return;
  const permission = button.dataset.optionalPermissionRequest;
  if (!androidOptionalPermissionKeys.includes(permission as AndroidOptionalPermission)) return;
  androidOptionalPermissionRequestBusy = true;
  button.disabled = true;
  $('androidOptionalPermissionFeedback').textContent = 'Waiting for Android permission…';
  try {
    const response = await invoke<{opened:boolean}>('android_optional_permissions_request',
      {permission,userInitiated:true});
    if (!response || typeof response.opened !== 'boolean')
      throw Error('Invalid Android permission request receipt');
    // Opening a system prompt does not prove that Android granted permission.
    $('androidOptionalPermissionFeedback').textContent = response.opened
      ? 'Confirm in Android, then return here.'
      : 'Checking the current Android permission…';
  } catch {
    $('androidOptionalPermissionFeedback').textContent =
      'Android could not open that permission prompt. You can retry.';
  } finally {
    androidOptionalPermissionRequestBusy = false;
    void refreshAndroidOptionalPermissions();
  }
});
$('storageAccessSection').addEventListener('click', async (event: MouseEvent) => {
  const button = (event.target as Element).closest<HTMLButtonElement>('[data-permission-request]');
  if (!button || button.disabled || permissionRequestInProgress || !nativeAndroidRuntime || !isTauri()) return;
  const permission = button.dataset.permissionRequest;
  if (!androidPermissionKeys.includes(permission as AndroidPermission)) return;
  permissionRequestInProgress = true;
  button.disabled = true;
  try {
    await invoke('android_permissions_request', { permission });
  } catch {
    // OS grant remains false unless confirmed by the subsequent status refresh.
  } finally {
    permissionRequestInProgress = false;
    void refreshStorageAccess(true);
  }
});
document.addEventListener('visibilitychange', () => {
  if (nativeAndroidRuntime && !document.hidden && !$('settingsPanel').classList.contains('hidden')) {
    void refreshStorageAccess(true);
    void refreshAndroidCosFolderGrant();
    void refreshOwnedChatgptStatus();
    void refreshAndroidCosOAuthStatus();
    if (androidCosSetupSelected === 'extension') void refreshAndroidCosExtensions();
  }
});
window.addEventListener('focus', () => {
  if (nativeAndroidRuntime && isTauri() && !$('settingsPanel').classList.contains('hidden')) {
    void refreshStorageAccess(true);
    void refreshAndroidCosFolderGrant();
    void refreshOwnedChatgptStatus();
    void refreshAndroidCosOAuthStatus();
    if (androidCosSetupSelected === 'extension') void refreshAndroidCosExtensions();
  }
});
// Poll the native OpenAI authorization while Setup is visible. The OAuth
// identity/plan comes from the Rust/Kotlin bridge, never private WebView cookies.
let lastAndroidChatgptSetupPollAt = 0;
if (nativeAndroidRuntime) window.setInterval(() => {
  if (document.hidden || !isTauri() || androidOptionalPermissionRequestBusy ||
      $('settingsPanel').classList.contains('hidden') ||
      $('settingsPanel').dataset.settingsView !== 'setup' ||
      androidCosSetupSelected !== 'access') return;
  void refreshAndroidOptionalPermissions();
}, 4500);
if (nativeAndroidRuntime) window.setInterval(() => {
  if (document.hidden || !isTauri() || androidCosOAuthBusy ||
      $('settingsPanel').classList.contains('hidden') ||
      $('settingsPanel').dataset.settingsView !== 'setup') return;
  if (androidCosSetupSelected !== 'browser') {
    if (Date.now() - lastAndroidChatgptSetupPollAt < 45_000) return;
    lastAndroidChatgptSetupPollAt = Date.now();
  }
  void refreshAndroidCosOAuthStatus();
}, 1800);
// Poll only native status while Setup OR Workspace Health is visible. Both
// pages observe the same runtime, including reconnect after process restart.
let lastAndroidTunnelSetupPollAt = 0;
if (nativeAndroidRuntime) window.setInterval(() => {
  if (document.hidden || !isTauri() || androidOpenAiTunnelBusy ||
      $('settingsPanel').classList.contains('hidden') ||
      !['setup','workspace'].includes($('settingsPanel').dataset.settingsView || '')) return;
  if (Date.now() - lastAndroidTunnelSetupPollAt < 3_000) return;
  lastAndroidTunnelSetupPollAt = Date.now();
  void refreshAndroidOpenAiTunnel();
}, 1500);
// Android (not the WebView) owns service binding. While Setup is visible,
// observe the native backend even if the user has moved beyond Permissions:
// system rebind can happen after an OS settings return or process restart.
// A last verified badge is never treated as a perpetual grant; every poll
// reads current OS consent and the isolated service-process liveness receipt.
// Keep the faster watch for Step 1, and use a slower cadence on other steps.
let lastAndroidAccessibilitySetupPollAt = 0;
if (nativeAndroidRuntime) window.setInterval(() => {
  if (document.hidden || !isTauri() ||
      $('settingsPanel').classList.contains('hidden') ||
      $('settingsPanel').dataset.settingsView !== 'setup' ||
      androidAccessibilityRefreshPending) return;
  const interval = androidCosSetupSelected === 'access' ? 2_500 : 12_000;
  if (Date.now() - lastAndroidAccessibilitySetupPollAt < interval) return;
  lastAndroidAccessibilitySetupPollAt = Date.now();
  void refreshAndroidAccessibility();
}, 2500);
$('toolPanelToggle').addEventListener('click', () => {
  const open = $('toolPanelMenu').classList.contains('hidden');
  closeToolMenu();
  if (open) {
    closeAndroidViewMenu();
    closeSidebar();
    $('toolPanelMenu').classList.remove('hidden');
    $('toolPanelBackdrop').classList.remove('hidden');
    $('toolPanelToggle').setAttribute('aria-expanded', 'true');
    $('toolPanelMenu').querySelector('button')?.focus();
  }
});
$('androidSubagentsToggle').addEventListener('click',()=>{
  if(workspacePanel==='subagents'&&!$('toolViewPanel').classList.contains('hidden'))
    closeToolPanel();
  else openToolPanel('subagents',true);
});
$('toolPanelBackdrop').addEventListener('click', closeToolMenu);
$('toolPanelMenu').addEventListener('click', event => {
  const target = (event.target as Element).closest<HTMLButtonElement>('[data-tool-view]');
  if (target) openToolPanel(target.dataset.toolView || '', event.isTrusted);
});
$('toolViewClose').addEventListener('click', closeToolPanel);
$('filesUp').addEventListener('click', () => void listFiles(filesCurrentPath.split('/').slice(0, -1).join('/')));
$('filesReload').addEventListener('click', () => void listFiles(filesCurrentPath));
$('terminalForm').addEventListener('submit', async event => {
  event.preventDefault();
  if (nativeAndroidRuntime) {
    $('androidTermuxStatus').textContent = 'In-app shell is unavailable. Open Termux to use a real terminal.';
    return;
  }
  const cmd = $('terminalInput').value.trim();
  if (!cmd) return;
  $('terminalInput').value = '';
  if (cmd === 'clear') { $('terminalOutput').replaceChildren(); return; }
  $('terminalForm').querySelector('button').disabled = true;
  try {
    if (nativeAndroidRuntime && isTauri() && $('terminalNativeMode').checked) {
      const parsed = /^(pwd|ls|stat|cat)(?: ([A-Za-z0-9._-]{1,64}\.txt))?$/.exec(cmd);
      if (!parsed || ((parsed[1] === 'pwd' || parsed[1] === 'ls') !== !parsed[2]))
        throw Error('Only pwd, ls, stat name.txt or cat name.txt are permitted.');
      if (!window.confirm(`Run isolated Android read-only command "${cmd}" in terminal-public? No shell or phone-wide access.`)) return;
      const bytes = crypto.getRandomValues(new Uint8Array(16));
      bytes[6] = (bytes[6] & 15) | 64;
      bytes[8] = (bytes[8] & 63) | 128;
      const hex = Array.from(bytes, byte => byte.toString(16).padStart(2,'0')).join('');
      const id = `${hex.slice(0,8)}-${hex.slice(8,12)}-${hex.slice(12,16)}-${hex.slice(16,20)}-${hex.slice(20)}`;
      activeNativeTerminalId = id;
      $('terminalNativeCancel').classList.remove('hidden');
      const result = await invoke<{requestId:string;output:string;readOnly:boolean;scope:string}>(
        'android_terminal_run', {requestId:id,command:parsed[1],name:parsed[2] || null,userInitiated:true});
      if (activeNativeTerminalId !== id) return;
      if (result.requestId !== id || result.readOnly !== true || result.scope !== 'terminal-public')
        throw Error('Native Android command did not return a valid scope/ID receipt');
      terminalLine(cmd, result.output || '(no output)');
      return;
    }
    const result = await api('/api/terminal', { command: cmd });
    terminalLine(cmd, String(result.output || '(no output)'));
  } catch (error) { terminalLine(cmd, String(error.message), true); }
  finally {
    activeNativeTerminalId = null;
    $('terminalNativeCancel').classList.add('hidden');
    $('terminalForm').querySelector('button').disabled = false;
  }
});

function renderModules() {
  const moduleInfo = {
    workspace: 'Read files inside the startup workspace',
    notes: 'Save and retrieve short local notes',
    external_agents: 'Run external AI agents and their configured tools',
  };
  const moduleLabels = {
    workspace: 'Workspace access',
    notes: 'Local notes',
    external_agents: 'External agents',
  };
  const moduleDetails = {
    workspace: 'Read-only file listing and reading inside the configured workspace. No access outside the selected folder.',
    notes: 'The agent can save and retrieve short notes locally when this module is enabled.',
    external_agents: 'External tools may execute through your installed Hermes configuration. Review those permissions before use.',
  };
  const wrapper = $('modules');
  wrapper.replaceChildren();
  for (const [id, enabled] of Object.entries(state.modules)) {
    const supported = !Array.isArray(state.availableModules) || state.availableModules.includes(id);
    const card = el('div', 'module-card');
    const row = el('label', 'module');
    const description = el('span');
    description.append(el('strong', '', moduleLabels[id] || id), el('small', '', moduleInfo[id] || 'Optional capability'));
    const check = el('input');
    check.type = 'checkbox';
    check.setAttribute('aria-label', `Enable ${moduleLabels[id] || id}`);
    check.checked = enabled;
    check.disabled = !supported;
    if (!supported) description.append(el('small', 'unavailable-hint', 'Coming to Android'));
    check.addEventListener('change', async () => {
      check.disabled = true;
      try { await api('/api/module', { id, enabled: check.checked }); await refresh(); announce(`${id} ${check.checked ? 'enabled' : 'disabled'}`, true); }
      catch (error) { check.checked = !check.checked; announce(error.message); }
      finally { check.disabled = false; }
    });
    row.append(description, check);
    card.append(row);
    if (enabled) card.append(el('div', 'module-details', moduleDetails[id] || 'This capability is currently enabled.'));
    wrapper.append(card);
  }
}

function renderSkills() {
  const wrapper = $('skills');
  wrapper.replaceChildren();
  if (state?.skillsAvailable === false || state.platform === 'android') {
    wrapper.append(el('p', '', 'Skills are not yet available in the Android preview.'));
    $('skillForm').classList.add('hidden');
    return;
  }
  $('skillForm').classList.remove('hidden');
  if (!state.skills.length) wrapper.append(el('p', '', 'No custom skills installed.'));
  for (const skill of state.skills) {
    const row = el('div', 'skill-row');
    const text = el('strong', '', skill.id);
    const toggle = el('input');
    toggle.type = 'checkbox';
    toggle.checked = skill.enabled;
    toggle.setAttribute('aria-label', `Enable ${skill.id}`);
    toggle.addEventListener('change', async () => {
      try { await api('/api/skill', { action: toggle.checked ? 'enable' : 'disable', id: skill.id }); await refresh(); announce('Skill updated', true); }
      catch (error) { toggle.checked = !toggle.checked; announce(error.message); }
    });
    const remove = el('button', 'remove', '✕');
    remove.title = `Remove ${skill.id}`;
    remove.addEventListener('click', async () => {
      if (!window.confirm(`Remove skill ${skill.id}?`)) return;
      try { await api('/api/skill', { action: 'remove', id: skill.id }); await refresh(); announce('Skill removed', true); }
      catch (error) { announce(error.message); }
    });
    row.append(text, toggle, remove);
    wrapper.append(row);
  }
}

let appStateRefreshEpoch = 0;
async function refresh() {
  // Older /api/state responses must not restore keys/providers from before
  // a more recent credential change or session refresh.
  const epoch = ++appStateRefreshEpoch;
  const latest = await api('/api/state') as AppState;
  if (epoch !== appStateRefreshEpoch) return;
  if (!Array.isArray(latest?.providers) || !Array.isArray(latest?.hasKeys))
    throw Error('Invalid provider state receipt');
  state = latest;
  const android = state.platform === 'android' || nativeAndroidRuntime;
  if (android) {
    // Retain genuine native provider state: API keys are session-only and
    // ChatGPT OAuth is an independent, text-only account transport.
    // The built-in account route always exists on Android, including legacy
    // state adapters that only returned API-key provider names.
    state.providers = ['chatgpt-browser',...state.providers.filter(provider =>
      ['openai','anthropic','openrouter'].includes(provider))];
    for (const option of [...$('provider').options])
      if (!state.providers.includes(option.value)) option.remove();
    if (!$('provider').value) {
      $('provider').value = 'chatgpt-browser';
      $('model').value = 'chatgpt.com';
    }
  }
  if (Array.isArray(state.providers)) {
    for (const option of [...$('provider').options]) {
      if (!state.providers.includes(option.value) &&
          !(option.value === 'chatgpt-browser' && !nativeAndroidRuntime)) option.remove();
    }
    if (!$('provider').value) $('provider').value = state.providers[0] || '';
  }
  $('provider').dispatchEvent(new Event('change'));
  const windows = state.platform === 'windows';
  // Native apps keep the conversation free of provider credential controls.
  document.documentElement.classList.toggle('native-android', android);
  document.documentElement.classList.toggle('native-windows', windows);
  // Transport validation, rather than an arbitrary Android textarea cap,
  // determines the maximum request payload. Long text remains editable.
  $('prompt').removeAttribute('maxlength');
  $('prompt').rows = android ? 1 : 2;
  // The Android composer shows the draft character count without a fake UI
  // hard limit; the native transport validates bytes at the send boundary.
  $('model').maxLength = 120;
  $('keyToggle').textContent = state.hasKeys?.includes($('provider').value) ? 'Key in memory' : 'Configure API key';
  $('moduleSection').classList.toggle('hidden', android || !Object.keys(state.modules).length);
  $('skillsSection').classList.toggle('hidden', android || state.skillsAvailable === false);
  $('workspaceSection').classList.toggle('hidden', android || !Object.keys(state.modules).length);
  $('externalSection').classList.toggle('hidden', android || !state.providers.includes('hermes'));
  $('workspace').textContent = state.workspace;
  renderThreads();
  syncStandingRulesSession();
  renderModules();
  renderSkills();
  if (extraView === 'skills') renderMobileSkills();
  if (workspacePanel === 'review') renderReview();
  updateWelcomeConnection();
  updateSidebarConnection();
  if (android && nativeAndroidRuntime) void refreshNativeProjectCatalog();
  if (android && nativeAndroidRuntime) updateAndroidComposerModel();
  if (android && nativeAndroidRuntime) renderAndroidAgentModel();
}

function setBusy(busy) {
  working = busy;
  $('speechDictation').disabled = busy || speechDictationBusy;
  const canQueue = nativeAndroidRuntime && busy && !pendingModelToolApproval &&
    !androidDuringTurnPending;
  $('send').disabled = pendingModelToolApproval !== null || (busy && !canQueue);
  $('cancelRequest').classList.toggle('hidden', !busy || !nativeAndroidRuntime || !isTauri() ||
    $('provider').value === 'chatgpt-browser');
  $('cancelRequest').disabled = !busy || !activeChatRequestId;
  // Keep the send arrow mounted through a request. textContent previously
  // destroyed the icon; after completion only the text node was restored.
  const sendText = $('send').querySelector('.send-label');
  if (sendText) sendText.textContent = busy && !canQueue ? 'Queued' : 'Send';
  $('send').setAttribute('aria-label',busy && !canQueue ? 'Follow-up already queued' :
    busy ? 'Send a follow-up after the current response' : 'Send message');
  $('newChat').disabled = busy;
  $('newChatTop').disabled = busy;
  $('provider').disabled = busy;
  $('model').disabled = busy;
  $('prompt').readOnly = busy && (!nativeAndroidRuntime || !!androidDuringTurnPending);
  $('keyToggle').disabled = busy || ['ollama', 'hermes', 'chatgpt-browser'].includes($('provider').value);
  $('settingsToggle').disabled = busy;
  $('sidebarCustomize').disabled = busy;
  if (state) renderThreads();
}
type AndroidDuringTurnPending = {
  prompt:string;
  sessionId:string|null;
  provider:string;
  mode:'steer'|'queue';
};
let androidDuringTurnPending:AndroidDuringTurnPending|null = null;
function renderAndroidDuringTurn():void {
  if (!nativeAndroidRuntime) return;
  const queued = androidDuringTurnPending;
  $('androidDuringTurnPending').classList.toggle('hidden',!queued);
  $('androidDuringTurnPendingText').textContent = !queued ? '' :
    queued.mode === 'steer'
      ? 'Follow-up accepted. This Android transport will send it as the next turn after the current reply is verified.'
      : 'Follow-up saved. It will return to your composer when the current reply ends.';
  $('androidGeneralDuringTurnHint').textContent =
    androidGeneralPreferences.messageDuringTurn === 'steer'
      ? 'Android OAuth cannot inject into a running response; Send accepts your follow-up now, then dispatches it after verified completion.'
      : 'Store one follow-up while ChatGPT works; review and send it once the current turn finishes.';
}
function acceptAndroidDuringTurn(prompt:string):void {
  if (!nativeAndroidRuntime || !working || androidDuringTurnPending ||
      pendingModelToolApproval || androidComposeMode !== 'normal' || androidPlanArmed) return;
  if (new TextEncoder().encode(prompt).length > 16_384) {
    announce('Follow-up is too long. Keep it under 16,384 bytes.'); return;
  }
  androidDuringTurnPending = {
    prompt,sessionId:selectedSession,provider:$('provider').value,
    mode:androidGeneralPreferences.messageDuringTurn
  };
  $('prompt').value = '';
  updateCharCount();
  renderAndroidDuringTurn();
  setBusy(true);
  announce(androidDuringTurnPending.mode === 'steer'
    ? 'Follow-up accepted while ChatGPT works. Android sends it as the next turn after a verified reply.'
    : 'Follow-up saved while ChatGPT works. It will return as a draft after this turn.',true);
}
function finishAndroidDuringTurn(verified:boolean,sessionId:string|null):void {
  const queued = androidDuringTurnPending;
  if (!nativeAndroidRuntime || !queued) return;
  androidDuringTurnPending = null;
  renderAndroidDuringTurn();
  const sameConversation = queued.provider === $('provider').value &&
    (queued.sessionId === sessionId || (queued.sessionId === null && verified));
  if (!sameConversation || !verified || $('prompt').value.trim()) {
    // A failed/uncertain turn can have reached OpenAI. Never dispatch its
    // follow-up automatically until the user has resolved that outcome.
    if (!$('prompt').value.trim()) {
      $('prompt').value = queued.prompt;
      updateCharCount();
    }
    announce('Follow-up kept as a draft. Check the previous turn before sending again.',true);
    return;
  }
  $('prompt').value = queued.prompt;
  updateCharCount();
  if (queued.mode === 'steer') {
    // The OAuth Responses transport has one verified request at a time.
    // A second outbound call is created only after the first was committed.
    queueMicrotask(() => {
      if (!working && $('prompt').value.trim() === queued.prompt &&
          selectedSession === sessionId && $('provider').value === queued.provider) {
        if (queued.provider === 'chatgpt-browser')
          void submitAuthorizedChatgpt(queued.prompt,true);
        else $('chatForm').requestSubmit();
      }
    });
  } else {
    announce('The current turn finished. Your follow-up is ready; tap Send to continue.',true);
  }
}
$('androidDuringTurnCancel').addEventListener('click',() => {
  if (!androidDuringTurnPending) return;
  const removed = androidDuringTurnPending;
  androidDuringTurnPending = null;
  if (!$('prompt').value.trim()) {
    $('prompt').value = removed.prompt;
    updateCharCount();
  }
  renderAndroidDuringTurn();
  setBusy(working);
  announce('Follow-up removed from the waiting slot and restored to your composer.',true);
});
type PendingModelToolApproval = {
  reviewId: string; requestId: string; name: string;
  arguments: Record<string, unknown>; provider: string; model: string;
  previous: string | null; originalPrompt: string;
};
let pendingModelToolApproval: PendingModelToolApproval | null = null;
function clearModelToolApproval() {
  pendingModelToolApproval = null;
  $('chatToolApprovalPanel').classList.add('hidden');
  $('chatToolApprovalName').textContent = '';
  $('chatToolApprovalArguments').textContent = '';
  $('chatToolApprovalStatus').textContent = '';
  $('chatToolApprove').disabled = false;
  $('chatToolReject').disabled = false;
  $('send').disabled = working;
}
$('chatToolReject').addEventListener('click', async () => {
  const pending = pendingModelToolApproval;
  if (!pending || working) return;
  $('chatToolApprove').disabled = true;
  $('chatToolReject').disabled = true;
  try {
    const result = await api('/api/chat-tool-review', {
      action:'reject',reviewId:pending.reviewId,requestId:pending.requestId,userInitiated:true,
    });
    if (result.rejected !== true || result.executed !== false)
      throw Error('Rejection was not confirmed by the native model-tool coordinator');
    clearModelToolApproval();
    const previous = state.sessions.find(session => session.id === pending.previous);
    selectedSession = pending.previous;
    displayMessages(previous || null);
    $('prompt').value = pending.originalPrompt;
    updateCharCount();
    announce('MCP tool proposal rejected. No tool was executed or conversation saved.', true);
  } catch (error) {
    $('chatToolApprovalStatus').textContent = `Tool review could not be rejected: ${error.message}. It will expire without execution.`;
    $('chatToolApprove').disabled = false;
    $('chatToolReject').disabled = false;
  }
});
$('chatToolApprove').addEventListener('click', async () => {
  const pending = pendingModelToolApproval;
  if (!pending || working || !nativeAndroidRuntime || !isTauri()) return;
  const args = JSON.stringify(pending.arguments,null,2);
  const localTool = pending.name.startsWith('android_');
  if (!window.confirm(`Approve ONE ${localTool ? 'app-owned Android tool' : 'remote MCP'} call?\n\nTool: ${pending.name}\nArguments: ${args.slice(0,3000)}\n\n${localTool ? 'Browser page data or app state may be included in the AI continuation. Android may ask for additional consent.' : 'Remote side effects cannot necessarily be undone.'}`)) return;
  activeChatRequestId = pending.requestId;
  setBusy(true);
  startAndroidChatWork(pending.requestId,pending.originalPrompt,
    `Executing approved ${localTool ? 'Android' : 'MCP'} tool`);
  const originalChat=pending.previous ? state.sessions.find(session=>session.id===pending.previous) : null;
  displayMessages({messages:[...(originalChat?.messages || []),
    {role:'user',text:pending.originalPrompt}]},true);
  $('chatToolApprove').disabled = true;
  $('chatToolReject').disabled = true;
  $('chatToolApprovalStatus').textContent = `Executing one approved ${localTool ? 'Android' : 'MCP'} tool and requesting the final AI answer…`;
  try {
    const result = await api('/api/chat-tool-review', {
      action:'approve',reviewId:pending.reviewId,requestId:pending.requestId,userInitiated:true,
    });
    if (typeof result.sessionId !== 'string' || !result.sessionId ||
        typeof result.answer !== 'string' || !Array.isArray(result.actions) || result.actions.length !== 1 ||
        result.actions[0]?.approvedByUser !== true || result.actions[0]?.name !== pending.name)
      throw Error('Approved tool completion receipt is incomplete; no success claimed');
    selectedSession = result.sessionId;
    clearModelToolApproval();
    $('prompt').value = '';
    updateCharCount();
    await refresh();
    let assignmentError: string | null = null;
    if (!pending.previous && selectedProjectId && !projectAssignments[result.sessionId]) {
      if (nativeProjectMode) {
        try { await updateNativeProjects('assign',
          {chatId:result.sessionId,projectId:selectedProjectId}); }
        catch (error) { assignmentError = String(error.message); }
      } else {
        projectAssignments[result.sessionId] = selectedProjectId;
        if (!saveProjectLabels()) {
          delete projectAssignments[result.sessionId];
          assignmentError = 'Project label could not be persisted';
        }
      }
    }
    const savedChat=state.sessions.find(session => session.id === selectedSession);
    finishAndroidChatWork(savedChat,result.actions);
    displayMessages(savedChat);
    verifiedAiConnection = {provider:pending.provider,model:pending.model};
    updateSidebarConnection();
    announce(assignmentError
      ? `Final response saved after one approved tool, but project assignment failed: ${assignmentError}`
      : `One approved ${localTool ? 'Android' : 'MCP'} tool result was used by the AI; final response saved.`, !assignmentError);
  } catch (error) {
    endAndroidChatWork();
    clearModelToolApproval();
    // A later failed turn does not erase an earlier independently successful
    // reply for the same unchanged provider/model. Credential updates below
    // explicitly invalidate that proof.
    updateSidebarConnection();
    const previous = state.sessions.find(session => session.id === pending.previous);
    selectedSession = pending.previous;
    displayMessages(previous || null);
    $('prompt').value = pending.originalPrompt;
    updateCharCount();
    announce(`Approved tool turn not completed: ${error.message}. The remote tool may already have had effects; do not retry blindly.`);
  } finally {
    endAndroidChatWork();
    activeChatRequestId = null;
    setBusy(false);
  }
});
$('cancelRequest').addEventListener('click', async () => {
  const requestId = activeChatRequestId;
  if (!requestId || !working || !nativeAndroidRuntime || !isTauri()) return;
  $('cancelRequest').disabled = true;
  announce('Cancelling the current AI request…');
  try {
    const response = await api('/api/chat-cancel', { requestId });
    if (response.cancelRequested) announce('AI request cancellation requested.', true);
  } catch (error) {
    if (working && activeChatRequestId === requestId) {
      $('cancelRequest').disabled = false;
      announce(`Could not cancel request: ${error.message}`);
    }
  }
});

// Android composer uses native Goal and API-model effort support. Browser OAuth
// still has no verified per-request reasoning override.
type AndroidComposeMenu = 'androidComposeAddMenu'|'androidComposeModeMenu'|'androidComposeMoreMenu'|'androidComposeReasoningMenu';
const androidComposeMenus: AndroidComposeMenu[] =
  ['androidComposeAddMenu','androidComposeModeMenu','androidComposeMoreMenu','androidComposeReasoningMenu'];
let androidComposeMode: 'normal'|'goal' = 'normal';
type AndroidReasoningEffort = 'auto'|'low'|'medium'|'high';
let androidReasoningEffort:AndroidReasoningEffort='auto';
function androidReasoningSupported():boolean {
  return nativeAndroidRuntime && ['openai','anthropic'].includes($('provider').value);
}
function renderAndroidReasoning():void {
  if(!nativeAndroidRuntime)return;
  const supported=androidReasoningSupported();
  const button=$('androidComposeReasoning') as HTMLButtonElement;
  button.title=supported?`Reasoning: ${androidReasoningEffort}`:
    'ChatGPT OAuth reasoning is controlled by your account; no per-message override is available';
  button.setAttribute('aria-label',button.title);
  button.dataset.effort=androidReasoningEffort;
  $('androidComposeReasoningHint').textContent=supported
    ? 'Sent only to compatible API models. An incompatible model rejects unsupported effort.'
    : 'This ChatGPT session does not expose a verified per-message reasoning override.';
  for(const option of document.querySelectorAll<HTMLButtonElement>('[data-reasoning-effort]')){
    option.disabled=!supported;
    option.setAttribute('aria-pressed',String(option.dataset.reasoningEffort===androidReasoningEffort));
  }
}
// An explicit UI preference, NOT permission authorization. Never pass this
// flag to the native bridge, providers, MCP tool review or Goal execution.
const androidBypassUiStorageKey = 'mcp-agent-ai-android-bypass-ui-only-v1';
let androidBypassUiRequested = false;
try { if (nativeAndroidRuntime) androidBypassUiRequested = localStorage.getItem(androidBypassUiStorageKey) === 'on'; }
catch { androidBypassUiRequested = false; }
function renderAndroidBypassPreference() {
  if (!nativeAndroidRuntime) return;
  $('androidBypassToggle').setAttribute('aria-checked',String(androidBypassUiRequested));
  $('androidBypassToggle').title = androidBypassUiRequested ?
    'On (UI preference only; bypass backend not implemented)' : 'Off (UI preference only)';
  $('androidBypassNotice').textContent = androidBypassUiRequested ?
    'On is only a saved UI preference. Backend pending: NO permissions are bypassed; every tool approval and Android grant is still required.' :
    'Backend pending — bypass is not active. MCP approvals, Android permissions and safety checks remain required.';
}
renderAndroidBypassPreference();
$('androidBypassToggle').addEventListener('click', () => {
  if (!nativeAndroidRuntime) return;
  androidBypassUiRequested = !androidBypassUiRequested;
  try { localStorage.setItem(androidBypassUiStorageKey, androidBypassUiRequested ? 'on' : 'off'); }
  catch { /* Preference can stay in memory when WebView storage is unavailable. */ }
  renderAndroidBypassPreference();
});
let androidPlanArmed = false;
let androidPlanBusy = false;
function setAndroidPlanMode(armed: boolean) {
  if (!nativeAndroidRuntime) return;
  androidPlanArmed = armed;
  $('androidComposePlan').setAttribute('aria-pressed', String(armed));
  $('androidComposePlan').title = armed ? 'Cancel Plan mode; keep draft' : 'Plan an editable sequence, separate from Goal/Sub-agents';
  if (!armed) $('androidPlanEditor').classList.add('hidden');
  closeAndroidComposerMenus();
}
function openAndroidPlanEditor() {
  if (!nativeAndroidRuntime) return;
  if (!androidPlanArmed) setAndroidPlanMode(true);
  const objective = String($('prompt').value).trim();
  if (!$('androidPlanObjective').value.trim() && objective)
    $('androidPlanObjective').value = objective;
  $('androidPlanEditor').classList.remove('hidden');
  $('androidPlanObjective').focus({preventScroll:true});
}
$('androidComposePlan').addEventListener('click', () => {
  if (!nativeAndroidRuntime || androidPlanBusy) return;
  if (androidPlanArmed) { setAndroidPlanMode(false); return; }
  // Plan has a dedicated editor: do not send anything or navigate to Sub-agents.
  setAndroidPlanMode(true);
  openAndroidPlanEditor();
});
$('androidPlanClose').addEventListener('click', () => {
  if (!androidPlanBusy) setAndroidPlanMode(false);
});
$('androidPlanUseDraft').addEventListener('click', () => {
  if (!nativeAndroidRuntime || androidPlanBusy || working || pendingModelToolApproval) return;
  const objective = String($('androidPlanObjective').value).trim();
  const lines = String($('androidPlanSteps').value).split(/\r?\n/u)
    .map((s:string)=>s.trim()).filter(Boolean);
  if (!objective || lines.length < 1 || lines.length > 16 ||
      new TextEncoder().encode(objective).length > 1000 ||
      lines.some(s => new TextEncoder().encode(s).length > 240)) {
    $('androidPlanStatus').textContent = 'Enter an objective and 1–16 short steps before applying the plan.';
    return;
  }
  const draft = `Objective: ${objective}\n\nPlan:\n${lines.map((line,i)=>`${i+1}. ${line.replace(/^\d+[.)]\s*/u,'')}`).join('\n')}`;
  if (new TextEncoder().encode(draft).length > 20_000) {
    $('androidPlanStatus').textContent = 'Plan is too large for a chat draft.';
    return;
  }
  $('prompt').value = draft;
  updateCharCount();
  setAndroidPlanMode(false);
  announce('Plan copied into your chat draft. Review it and press Send yourself; no AI request or Goal execution occurred.',true);
});
$('androidPlanGenerate').addEventListener('click', async () => {
  if (nativeAndroidRuntime) {
    $('androidPlanStatus').textContent =
      'Use the manual Plan editor or ask ChatGPT in chat to draft a plan. Direct AI provider drafting is disabled.';
    return;
  }
  if (!nativeAndroidRuntime || !isTauri() || !androidPlanArmed ||
      androidPlanBusy || working || pendingModelToolApproval) {
    $('androidPlanStatus').textContent = 'AI Plan drafting requires an available Android native provider.';
    return;
  }
  const selected = mobileAiDelegateSelection();
  const objective = String($('androidPlanObjective').value).trim();
  if (!selected || !objective || new TextEncoder().encode(objective).length > 1000 ||
      /[\x00-\x1f\x7f]/u.test(objective)) {
    $('androidPlanStatus').textContent = 'Select a configured OpenAI, OpenRouter or Anthropic model; enter a printable objective (max 1000 UTF-8 bytes).';
    return;
  }
  if (!window.confirm(`Draft a Plan using ${selected.provider} / ${selected.model}? AI provider billing may apply. Nothing will be sent to chat, created as a Goal, or executed until you approve the next action.`)) return;
  const requestId = createChatRequestId();
  const initialSteps = String($('androidPlanSteps').value);
  androidPlanBusy = true;
  $('androidPlanGenerate').disabled = true;
  $('androidPlanUseDraft').disabled = true;
  $('androidPlanStatus').textContent = 'Requesting one bounded, review-only Plan draft…';
  try {
    const receipt = await api('/api/mobile-goal-draft', {
      provider:selected.provider,model:selected.model,objective,requestId,userInitiated:true,
    });
    const plan = validateReviewedGoalDraft(receipt,requestId);
    const current = mobileAiDelegateSelection();
    if (!androidPlanArmed || $('androidPlanEditor').classList.contains('hidden') ||
        $('androidPlanObjective').value.trim() !== objective ||
        String($('androidPlanSteps').value) !== initialSteps ||
        !current || current.provider !== selected.provider || current.model !== selected.model)
      throw Error('Plan or provider changed while drafting; stale suggestions discarded');
    $('androidPlanSteps').value = plan.steps.map(step=>step.description).join('\n');
    $('androidPlanStatus').textContent =
      `Received ${plan.steps.length} suggested steps for review and editing. No Goal was created and no tools executed. Select Use in chat to copy a reviewed plan.`;
  } catch (error) {
    $('androidPlanStatus').textContent =
      `Plan draft not applied: ${String(error).slice(0,180)}. No Goal or tool execution confirmed.`;
  } finally {
    androidPlanBusy = false;
    $('androidPlanGenerate').disabled = false;
    $('androidPlanUseDraft').disabled = false;
  }
});
type RuleDraft = {enabled:boolean;text:string};
const emptyRule = (): RuleDraft => ({enabled:false,text:''});
const savedRuleVersions = new Map<string,RuleDraft>();
const draftRuleVersions = new Map<string,string>();
let androidRulesLoadedContext: string | undefined;
let androidRulesObservedContext: string | undefined;
let androidRulesBusy = false;
let androidRulesGeneration = 0;
function realRulesSession(): string | null {
  return selectedSession && state.sessions.some(s => s.id === selectedSession) ? selectedSession : null;
}
function rulesKey(id: string): string { return `session:${id}`; }
function updateRulesEditor() {
  const id = realRulesSession();
  const ready = !!id && nativeAndroidRuntime && isTauri() &&
    androidRulesLoadedContext === id && !androidRulesBusy;
  const saved = id ? savedRuleVersions.get(rulesKey(id)) || emptyRule() : emptyRule();
  const draft = id ? draftRuleVersions.get(rulesKey(id)) ?? saved.text : '';
  if ($('androidStandingRulesText').value !== draft) $('androidStandingRulesText').value = draft;
  $('androidStandingRulesText').disabled = !ready;
  $('androidStandingRulesSave').disabled = !ready || !draft.trim() ||
    (saved.enabled && saved.text === draft.trim());
  $('androidStandingRulesReset').disabled = !ready || (!saved.enabled && !saved.text);
}
function standingRulesStatus(message: string) {
  $('androidStandingRulesStatus').textContent = message;
}
function syncStandingRulesSession() {
  const id = realRulesSession();
  $('androidStandingRulesSession').textContent = `Chat session: ${id ? id.slice(0,8) : 'None'}`;
  const context = id || 'none';
  if (androidRulesObservedContext !== context) {
    androidRulesObservedContext = context;
    ++androidRulesGeneration;
    androidRulesLoadedContext = undefined;
    if (!id) standingRulesStatus('Start a chat and receive a saved reply before setting rules for it.');
    if (nativeAndroidRuntime && !$('androidComposeMoreMenu').classList.contains('hidden'))
      void loadStandingRules();
  }
  updateRulesEditor();
}
function rememberRulesSnapshot(value: any, id: string) {
  if (typeof value?.enabled !== 'boolean' || typeof value?.text !== 'string' ||
      new TextEncoder().encode(value.text).length > 4000)
    throw Error('Invalid scoped rules receipt');
  const key = rulesKey(id);
  const previous = savedRuleVersions.get(key);
  const draft = draftRuleVersions.get(key);
  savedRuleVersions.set(key,{enabled:value.enabled,text:value.text});
  if (!previous || draft === undefined || previous.text === draft)
    draftRuleVersions.set(key,value.text);
}
async function loadStandingRules() {
  if (!nativeAndroidRuntime || androidRulesBusy) return;
  const id = realRulesSession();
  if (!id) {
    androidRulesLoadedContext = 'none';
    standingRulesStatus('No saved chat selected. Set rules becomes available after the first saved reply.');
    updateRulesEditor(); return;
  }
  if (androidRulesLoadedContext === id) { updateRulesEditor(); return; }
  if (!isTauri()) {
    standingRulesStatus('Standing rules require the installed Android app; cannot save from a browser preview.');
    updateRulesEditor();
    return;
  }
  const generation = ++androidRulesGeneration;
  androidRulesBusy = true;
  standingRulesStatus('Loading private rules...');
  try {
    const result = await api('/api/standing-rules', {action:'get',sessionId:id});
    if (generation !== androidRulesGeneration || id !== realRulesSession()) return;
    if (result?.sessionId !== id) throw Error('Rules response belongs to another chat');
    rememberRulesSnapshot(result.session,id);
    androidRulesLoadedContext = id;
    standingRulesStatus(result.session.enabled ?
      'Rules are set for this chat only. Edit and select Set rules to replace, or Reset rules to remove.' :
      'No rules set for this chat. Enter instructions and select Set rules.');
  } catch (error) {
    if (generation === androidRulesGeneration)
      standingRulesStatus(`Could not load rules: ${String(error).slice(0,130)}. Saving is disabled.`);
  } finally {
    androidRulesBusy = false;
    updateRulesEditor();
    if (generation !== androidRulesGeneration && !$('androidComposeMoreMenu').classList.contains('hidden'))
      void loadStandingRules();
  }
}
$('androidStandingRulesText').addEventListener('input', () => {
  const id = realRulesSession();
  if (!id || androidRulesBusy || androidRulesLoadedContext !== id) return;
  draftRuleVersions.set(rulesKey(id),String($('androidStandingRulesText').value));
  updateRulesEditor();
  standingRulesStatus('Unsaved draft for this chat. Select Set rules to apply.');
});
$('androidStandingRulesForm').addEventListener('submit', async event => {
  event.preventDefault();
  const id = realRulesSession();
  if (!nativeAndroidRuntime || !isTauri() || !id || androidRulesLoadedContext !== id || androidRulesBusy) return;
  const text = String($('androidStandingRulesText').value).trim();
  if (new TextEncoder().encode(text).length > 4000 || /[\x00-\x08\x0b-\x1f\x7f\u202a-\u202e\u2066-\u2069]/u.test(text) ||
      !text) {
    standingRulesStatus('Enter nonempty printable rules, at most 4000 UTF-8 bytes.');
    return;
  }
  const generation = ++androidRulesGeneration;
  androidRulesBusy = true;
  $('androidStandingRulesSave').disabled = true;
  standingRulesStatus('Saving to private Android storage...');
  try {
    const receipt = await api('/api/standing-rules', {action:'save',userInitiated:true,scope:'session',sessionId:id,enabled:true,text});
    if (receipt?.enabled !== true || receipt?.text !== text ||
        receipt?.sessionId !== id || receipt?.scope !== 'session')
      throw Error('Native save receipt did not match your draft');
    if (generation !== androidRulesGeneration || id !== realRulesSession()) {
      standingRulesStatus('Original chat save acknowledged; selection changed. Reload before editing.');
      return;
    }
    savedRuleVersions.set(rulesKey(id),{enabled:true,text});
    draftRuleVersions.set(rulesKey(id),text);
    standingRulesStatus('Rules set for this chat only. Other chat sessions are unchanged.');
  } catch (error) {
    if (generation === androidRulesGeneration)
      standingRulesStatus(`Rules not confirmed saved: ${String(error).slice(0,140)}`);
  } finally {
    androidRulesBusy = false;
    updateRulesEditor();
    if (generation !== androidRulesGeneration && !$('androidComposeMoreMenu').classList.contains('hidden'))
      void loadStandingRules();
  }
});
$('androidStandingRulesReset').addEventListener('click',async () => {
  const id = realRulesSession();
  if (!id || !nativeAndroidRuntime || !isTauri() || androidRulesLoadedContext !== id || androidRulesBusy ||
      !window.confirm('Reset saved rules for this chat only? Other chats will not be changed.')) return;
  const generation = ++androidRulesGeneration;
  androidRulesBusy = true;
  updateRulesEditor();
  standingRulesStatus('Removing this chat’s saved rules...');
  try {
    const receipt = await api('/api/standing-rules',{action:'reset',sessionId:id,userInitiated:true});
    if (receipt?.sessionId !== id || receipt?.scope !== 'session' || receipt?.reset !== true ||
        receipt?.enabled !== false || receipt?.text !== '') throw Error('Invalid reset receipt');
    if (generation !== androidRulesGeneration || realRulesSession() !== id) return;
    savedRuleVersions.set(rulesKey(id),emptyRule());
    draftRuleVersions.set(rulesKey(id),'');
    standingRulesStatus('Rules reset for this chat only. Other chats are unchanged.');
  } catch(error) {
    if (generation === androidRulesGeneration)
      standingRulesStatus(`Reset not confirmed: ${String(error).slice(0,140)}`);
  } finally {
    androidRulesBusy = false;
    updateRulesEditor();
    if (generation !== androidRulesGeneration && !$('androidComposeMoreMenu').classList.contains('hidden'))
      void loadStandingRules();
  }
});
function closeAndroidComposerMenus() {
  for (const id of androidComposeMenus) $(id).classList.add('hidden');
  for (const id of ['androidComposeAdd','androidComposeMode','androidComposeMore','androidComposeReasoning'])
    $(id).setAttribute('aria-expanded','false');
}
function updateAndroidComposerModel() {
  if (!nativeAndroidRuntime) return;
  renderAndroidReasoning();
  const provider = String($('provider').value || '').trim();
  const model = String($('model').value || '').trim();
  const name = provider === 'chatgpt-browser' ? 'ChatGPT' :
    model ? model : 'Set model';
  $('androidComposerModelText').textContent = name;
  $('androidComposerModel').setAttribute('aria-label', 'ChatGPT connection setup');
  $('androidComposerModel').title = 'ChatGPT · Open Setup → Connect to check your account. Context percentage unavailable.';
}
$('androidComposerModel').addEventListener('click', () => {
  if (!nativeAndroidRuntime) return;
  closeAndroidComposerMenus();
  setSettings(true);
  setAndroidSettingsView('setup');
});
for (const [button,menu] of [
  ['androidComposeAdd','androidComposeAddMenu'],
  ['androidComposeMode','androidComposeModeMenu'],
  ['androidComposeMore','androidComposeMoreMenu'],
  ['androidComposeReasoning','androidComposeReasoningMenu'],
] as const) {
  $(button).addEventListener('click', () => {
    const open = $(menu).classList.contains('hidden');
    closeAndroidComposerMenus();
    $(menu).classList.toggle('hidden',!open);
    $(button).setAttribute('aria-expanded', String(open));
    if (open && menu === 'androidComposeMoreMenu') void loadStandingRules();
  });
}
document.addEventListener('pointerdown', event => {
  if (nativeAndroidRuntime && !(event.target as Element).closest(
    '.android-compose-controls, #androidComposeMore, #androidComposeMoreMenu'))
    closeAndroidComposerMenus();
});
$('androidComposeModeMenu').addEventListener('click', event => {
  const button = (event.target as Element).closest<HTMLButtonElement>('[data-compose-mode]');
  if (!button || button.disabled) return;
  const mode = button.dataset.composeMode;
  if (mode !== 'goal' && mode !== 'normal') return;
  androidComposeMode = mode;
  setAndroidPlanMode(false);
  $('androidComposeModeLabel').textContent = mode === 'goal' ? 'Goal' : 'Normal';
  $('androidComposeMode').setAttribute('aria-label',`Chat mode: ${mode === 'goal' ? 'Goal' : 'Normal'}`);
  $('androidComposeMode').dataset.mode = mode;
  for (const choice of document.querySelectorAll<HTMLButtonElement>('[data-compose-mode]'))
    choice.setAttribute('aria-pressed',String(choice.dataset.composeMode === mode));
  closeAndroidComposerMenus();
  if (mode === 'goal') announce('Goal selected: sending opens the native Goal editor for your review. Nothing starts automatically.',true);
});
function openGoalPlannerFromComposer() {
  if (!nativeAndroidRuntime) return;
  const objective = $('prompt').value.trim();
  if (!isTauri()) { announce('Goal planning needs the installed Android app.'); return; }
  if (objective && new TextEncoder().encode(objective).length > 1000) {
    announce('Goal draft objective must fit 1000 UTF-8 bytes. Shorten your message before planning.');
    return;
  }
  closeAndroidComposerMenus();
  openToolPanel('subagents');
  if (objective) {
    $('mobileGoalDraftObjective').value = objective;
    updateMobileGoalDraftButton();
  }
  $('mobileGoalDraftObjective').focus({preventScroll:true});
  announce('Review your Goal objective, draft and approval steps, then explicitly Create. No task has run.',true);
}
$('androidComposeReasoningMenu').addEventListener('click',event=>{
  const effort=(event.target as Element).closest<HTMLButtonElement>('[data-reasoning-effort]');
  if(!effort || effort.disabled || !androidReasoningSupported())return;
  const value=effort.dataset.reasoningEffort;
  if(value!=='auto'&&value!=='low'&&value!=='medium'&&value!=='high')return;
  androidReasoningEffort=value;
  renderAndroidReasoning();
  closeAndroidComposerMenus();
});
$('androidComposeAddMenu').addEventListener('click', event => {
  const action = (event.target as Element).closest<HTMLButtonElement>('[data-compose-add]')?.dataset.composeAdd;
  if (!action) return;
  closeAndroidComposerMenus();
  if (action === 'folder') $('projectChoosePhoneFolder').click();
  if (action === 'files') {
    openToolPanel('files');
    announce('Files panel opened. You can inspect your selected Android folder. Direct image/file attachments to the model are not supported yet.', true);
  }
  if (action === 'skills') {
    const prompt = $('prompt') as HTMLTextAreaElement;
    const prefix = 'Please add this skill: ';
    const insertion = prompt.value.trim() ? '\n' + prefix : prefix;
    const start = prompt.selectionStart ?? prompt.value.length;
    const end = prompt.selectionEnd ?? start;
    prompt.setRangeText(insertion,start,end,'end');
    updateCharCount();
    prompt.focus({preventScroll:true});
  }
});

type OwnedBrowserTurn = {
  requestId: string;
  prompt: string;
  previous: string | null;
  startedSend: boolean;
};
// Kept in memory rather than silently retrying a request across a restart.
// An ambiguous send is an unresolved remote effect, never an API fallback.
let pendingOwnedBrowserTurn: OwnedBrowserTurn | null = null;
let ownedBrowserTurnChecking = false;
function checkedOwnedBrowserTurn(receipt: any, requestId: string):
  {requestId:string;state:string;accepted:boolean;completed:boolean;answer?:string} {
  const states = ['prepared','ready','submitted','answering','completed','uncertain',
    'failed_before_send','cancelled','not_started'];
  if (!receipt || receipt.requestId !== requestId || !states.includes(receipt.state) ||
      typeof receipt.accepted !== 'boolean' || typeof receipt.completed !== 'boolean' ||
      (receipt.completed && (receipt.state !== 'completed' || !receipt.accepted ||
        typeof receipt.answer !== 'string' || !receipt.answer.trim() ||
        new TextEncoder().encode(receipt.answer).length > 100_000)) ||
      (!receipt.completed && receipt.answer !== undefined))
    throw Error('ChatGPT browser returned an invalid or ambiguous turn receipt; no answer was saved');
  return receipt;
}
async function completeOwnedBrowserTurn(turn: OwnedBrowserTurn) {
  // The host independently re-observes the owned ChatGPT page and checks its
  // exact turn receipt before calling Rust's guarded session storage.
  const result = await invoke<any>('chatgpt_browser_finalize', {requestId:turn.requestId});
  if (typeof result?.sessionId !== 'string' || !/^[0-9a-f-]{36}$/i.test(result.sessionId) ||
      typeof result.answer !== 'string' || !result.answer.trim() ||
      !Array.isArray(result.actions) || result.actions.length)
    throw Error('Browser completion could not be verified and saved');
  if (pendingOwnedBrowserTurn?.requestId !== turn.requestId) return;
  pendingOwnedBrowserTurn = null;
  // An unresolved Send can outlive the user's current tab or editor draft.
  // Reconciliation must save the verified answer without hijacking another
  // selected chat or erasing any draft typed while awaiting a receipt.
  const stillOnOriginal = selectedSession === turn.previous &&
    $('provider').value === 'chatgpt-browser';
  const stillSameDraft = $('prompt').value.trim() === turn.prompt;
  if (stillOnOriginal && stillSameDraft) {
    $('prompt').value = '';
    updateCharCount();
  }
  if (stillOnOriginal) selectedSession = result.sessionId;
  await refresh();
  if (stillOnOriginal)
    displayMessages(state.sessions.find(s => s.id === selectedSession) || null);
  verifiedAiConnection = {provider:'chatgpt-browser',model:'chatgpt.com'};
  updateSidebarConnection();
  announce(stillOnOriginal ? 'ChatGPT website reply verified and saved from the owned browser.' :
    'Earlier ChatGPT browser reply verified and saved; your current chat/draft was not changed.', true);
}
async function observeOwnedBrowserTurn(turn: OwnedBrowserTurn, repeated = false) {
  if (ownedBrowserTurnChecking || pendingOwnedBrowserTurn?.requestId !== turn.requestId) return;
  ownedBrowserTurnChecking = true;
  try {
    const limit = repeated ? 50 : 1;
    for (let i = 0; i < limit; i++) {
      const receipt = checkedOwnedBrowserTurn(await invoke<any>('chatgpt_browser_observe',
        {requestId:turn.requestId}), turn.requestId);
      if (receipt.completed) { await completeOwnedBrowserTurn(turn); return; }
      if (receipt.state === 'failed_before_send' || receipt.state === 'cancelled') {
        if (receipt.accepted) throw Error('Contradictory browser turn status');
        // Even a pre-click refusal can leave the native coordinator holding an
        // exact prepared draft. Only explicit verified cancellation frees it.
        try {
          const withdrawn = checkedOwnedBrowserTurn(await invoke<any>('chatgpt_browser_cancel',
            {requestId:turn.requestId}), turn.requestId);
          if (withdrawn.state === 'cancelled' && !withdrawn.accepted) {
            pendingOwnedBrowserTurn = null;
            announce('ChatGPT browser confirmed this draft was not sent and withdrew it.',true);
          } else throw Error('Could not confirm native draft withdrawal');
        } catch {
          announce('ChatGPT reported no Send, but native draft withdrawal remains uncertain. Check the owned browser before another attempt.');
        }
        return;
      }
      if (!repeated) break;
      await new Promise<void>(resolve => setTimeout(resolve, 1000));
    }
    announce('ChatGPT send/reply is not yet confirmed. Do not submit this prompt again. Use Settings → Check pending ChatGPT turn to reconcile.');
  } catch (error) {
    announce(`ChatGPT outcome unverified: ${String(error).slice(0,160)}. Do not send this prompt again; use Check pending ChatGPT turn.`);
  } finally {
    ownedBrowserTurnChecking = false;
    showOwnedChatgptStatus(ownedChatgptStatus);
  }
}
async function submitOwnedBrowserChat(prompt: string) {
  if (!isTauri()) { announce('ChatGPT built-in browser needs the installed native app.'); return; }
  if (pendingOwnedBrowserTurn) {
    announce('A previous ChatGPT Send is still unresolved. Do not retry; check that turn under Settings → Setup.');
    return;
  }
  if (new TextEncoder().encode(prompt).length > 16_384) {
    announce('ChatGPT built-in browser prompt must be 1–16,384 UTF-8 bytes.'); return;
  }
  const old = selectedSession ? state.sessions.find(s => s.id === selectedSession) : null;
  if (selectedSession && (!old || old.provider !== 'chatgpt-browser' || old.model !== 'chatgpt.com')) {
    announce('Start a new chat to change the conversation provider or model.'); return;
  }
  let websitePrompt = prompt;
  let websiteRulesIncluded = false;
  if (nativeAndroidRuntime) {
    // The website has no app-owned system-prompt API. Explicitly include the
    // user's enabled standing rules in the exact message being prepared, so
    // the site's verified transcript and the app's saved user turn agree.
    // No rule is silently considered a privileged ChatGPT system instruction.
    setBusy(true);
    try {
      const identity = old?.id ?? null;
      const rules = await api('/api/standing-rules', {action:'get',sessionId:identity});
      if (rules?.sessionId !== identity) throw Error('Standing rules returned for a different chat');
      const activeTexts: string[] = [];
      // The previous global scope is retired. Only the exact saved chat may
      // contribute rules; an unsent conversation has no local Rules identity.
      for (const [label,record] of [['This chat',rules.session]] as const) {
        if (typeof record?.enabled !== 'boolean' || typeof record?.text !== 'string' ||
            new TextEncoder().encode(record.text).length > 4000)
          throw Error('Standing rules response invalid; browser Send blocked');
        if (record.enabled) {
          if (!record.text.trim() || (label === 'This chat' && !identity))
            throw Error('Invalid session-specific rules; browser Send blocked');
          activeTexts.push(`${label}:\n${record.text}`);
        }
      }
      if (activeTexts.length) {
        websitePrompt = `[My saved Mcp Agent Ai rules — visible user text, not system instructions]\n${activeTexts.join('\n\n')}\n[/Rules]\n\n[My message]\n${prompt}`;
        websiteRulesIncluded = true;
      }
      if (new TextEncoder().encode(websitePrompt).length > 16_384)
        throw Error('Message plus enabled rules exceed the 16,384-byte ChatGPT website limit');
      if ($('prompt').value.trim() !== prompt || selectedSession !== (old?.id ?? null) ||
          $('provider').value !== 'chatgpt-browser' || pendingOwnedBrowserTurn)
        throw Error('Chat or draft changed while loading standing rules; nothing was sent');
    } catch (error) {
      setBusy(false);
      announce(`Could not prepare ChatGPT website message with the current rules: ${String(error).slice(0,145)}. Nothing was sent.`);
      return;
    }
  }
  const turn: OwnedBrowserTurn = {requestId:createChatRequestId(),prompt,previous:selectedSession,startedSend:false};
  pendingOwnedBrowserTurn = turn;
  setBusy(true);
  displayMessages({messages:[...(old?.messages || []),{role:'user',text:prompt}]},true);
  announce('Preparing the exact message in your owned ChatGPT browser; nothing has been sent.');
  try {
    const prepared = checkedOwnedBrowserTurn(await invoke<any>('chatgpt_browser_prepare',
      {requestId:turn.requestId,prompt:websitePrompt,sessionId:turn.previous}), turn.requestId);
    if (prepared.state === 'failed_before_send' && !nativeAndroidRuntime) {
      // Windows adapter proved pre-insertion rejection and released its
      // journal; the host independently removed its reservation.
      pendingOwnedBrowserTurn = null;
      announce('Owned ChatGPT page refused the draft before Send. No message was sent.');
      return;
    }
    if (!['prepared','ready'].includes(prepared.state) || prepared.accepted || prepared.completed)
      throw Error('ChatGPT browser did not return a safe prepared draft receipt');
    if (!window.confirm(websiteRulesIncluded ?
      'Send ONE message including your saved standing rules and this prompt through the signed-in ChatGPT website? The rules will be visible as USER message text in ChatGPT and the saved conversation, not as system instructions. A remote Send cannot be undone.' :
      'Send this exact message ONCE using the signed-in, app-owned ChatGPT website? The message is visible in your Mcp Agent Ai composer. A remote Send cannot be undone.')) {
      const cancelled = checkedOwnedBrowserTurn(await invoke<any>('chatgpt_browser_cancel',
        {requestId:turn.requestId}), turn.requestId);
      if (cancelled.state !== 'cancelled' || cancelled.accepted)
        throw Error('Draft cancellation was not verified; check the owned browser before sending');
      pendingOwnedBrowserTurn = null;
      if (old) displayMessages(old); else displayMessages(null);
      announce('ChatGPT draft preparation cancelled before Send.',true);
      return;
    }
    turn.startedSend = true;
    // Exactly one call; even a timeout is potentially submitted and cannot be retried.
    const sent = checkedOwnedBrowserTurn(await invoke<any>('chatgpt_browser_send',
      {requestId:turn.requestId,userInitiated:true}),turn.requestId);
    if (sent.state === 'cancelled' && !sent.accepted) {
      pendingOwnedBrowserTurn = null;
      announce('Native ChatGPT Send confirmation declined; the prepared draft was withdrawn.',true);
      return;
    }
    if (sent.state === 'failed_before_send' && !sent.accepted) {
      const cancelled = checkedOwnedBrowserTurn(await invoke<any>('chatgpt_browser_cancel',
        {requestId:turn.requestId}), turn.requestId);
      if (cancelled.state !== 'cancelled' || cancelled.accepted)
        throw Error('Native browser could not verify the failed Send was withdrawn');
      pendingOwnedBrowserTurn = null;
      announce('The ChatGPT Send control refused this draft before dispatch. No remote message was confirmed.');
      return;
    }
    announce('ChatGPT Send requested once; waiting for the exact new user and assistant receipt.');
    await observeOwnedBrowserTurn(turn,true);
  } catch (error) {
    verifiedAiConnection = null;
    updateSidebarConnection();
    if (!turn.startedSend) {
      try {
        const receipt = checkedOwnedBrowserTurn(await invoke<any>('chatgpt_browser_cancel',
          {requestId:turn.requestId}), turn.requestId);
        if (receipt.state === 'cancelled' && !receipt.accepted) pendingOwnedBrowserTurn = null;
      } catch { /* Cannot claim the prepared draft was cleared. */ }
    }
    announce(`${turn.startedSend ? 'ChatGPT Send outcome uncertain' : 'ChatGPT preparation failed'}: ${String(error).slice(0,160)}. ${turn.startedSend ? 'Do not retry; check the pending turn in Settings.' : 'No confirmed Send occurred.'}`);
  } finally {
    if (pendingOwnedBrowserTurn && selectedSession === turn.previous && old) displayMessages(old);
    if (pendingOwnedBrowserTurn && selectedSession === turn.previous && !old) displayMessages(null);
    setBusy(false);
    showOwnedChatgptStatus(ownedChatgptStatus);
  }
}

let authorizedChatgptPending = false;
function authorizedChatgptFailureReason(receipt: any): string {
  const safe = typeof receipt?.reason === 'string' ? receipt.reason : '';
  const descriptions: Record<string,string> = {
    http_401:'OpenAI rejected the current ChatGPT authorization (HTTP 401). Open Step 3 to review account access.',
    http_403:'OpenAI denied this account or model access (HTTP 403). Review approved plan permissions.',
    http_404:'The requested OpenAI model or endpoint was not available (HTTP 404).',
    http_400:'OpenAI rejected the request as invalid (HTTP 400). No automatic retry.',
    http_422:'OpenAI could not process the request input (HTTP 422).',
    http_429:'OpenAI reported a temporary rate or usage limit (HTTP 429).',
    http_5xx:'OpenAI returned a server error. The request outcome cannot be confirmed.',
    http_other:'OpenAI rejected this request with an unclassified HTTP status.',
    usage_limit:'The ChatGPT plan usage limit has been reached for this request.',
    usage_unavailable:'ChatGPT plan usage is not currently available for this account.',
    subscription_sharing_usage_limit_exceeded:'The ChatGPT plan usage allowance for this integration was exceeded. Review ChatGPT usage settings.',
    model_unavailable:'The selected ChatGPT model was not available for this plan.',
    model_not_found:'OpenAI could not find the selected account model.',
    provider_failure:'OpenAI reported a failed model response.',
    response_incomplete:'OpenAI did not complete this response.',
    response_cancelled:'OpenAI cancelled the response before completion.',
    stream_incomplete:'OpenAI response stream ended without the required completed event.',
    stream_failure:'OpenAI returned an unsuccessful response event.',
    invalid_stream:'The Responses API returned a stream that could not be safely verified.',
    invalid_stream_json:'The Responses API returned JSON instead of the required SSE stream. This is not a confirmed chat reply.',
    invalid_stream_html:'The network returned HTML instead of a Responses API stream; check network restrictions.',
    invalid_stream_missing:'The Responses API response did not declare a usable content type.',
    invalid_stream_other:'The Responses API used an unsupported response format rather than SSE.',
    invalid_completion:'The completed response receipt did not pass native verification.',
    empty_answer:'OpenAI completed without a usable assistant answer.',
    invalid_request:'The native request failed validation before dispatch.',
    busy:'Another ChatGPT request or authorization operation is active.',
    network_timeout:'The ChatGPT connection timed out before a completed response was confirmed.',
    network_error:'The network connection failed before response completion.',
    unknown_failure:'The native request failed without a classified, safe reason.',
    unknown:'The native response did not identify a safe, specific cause.'
  };
  return descriptions[safe] || 'The native response did not identify a safe, specific cause.';
}
// Android ChatGPT plan requests use official OAuth (not WebView cookies).
// This path does not require Step 8 Secure Tunnel or Step 9 MCP Plugin.
async function submitAuthorizedChatgpt(prompt: string,preapprovedFollowUp = false) {
  if (!nativeAndroidRuntime || !isTauri() || working || authorizedChatgptPending) return;
  // Setup may never have been opened in this Activity lifetime. The native
  // Keystore OAuth grant survives app restarts, while renderer status does not.
  // Read the account authorization before rejecting Send as signed-out.
  if (!androidCosOAuthLinked()) await refreshAndroidCosOAuthStatus(true);
  if (!androidCosOAuthLinked()) {
    announce('ChatGPT plan authorization is missing. Open Settings → Setup → Step 3 to approve Mcp Agent Ai. Nothing was sent.');
    return;
  }
  if (new TextEncoder().encode(prompt).length > 16_384) {
    announce('ChatGPT message must be at most 16,384 UTF-8 bytes. Nothing was sent.');
    return;
  }
  const previous = selectedSession;
  const old = previous ? state.sessions.find(session => session.id === previous) : null;
  if (previous && (!old || old.provider !== 'chatgpt-browser' || old.model !== 'chatgpt.com' ||
      !old.actions?.length || !old.actions.every((action: any) =>
        action?.type === 'chatgpt_oauth_turn' && action?.nativeConfirmed === true &&
        action?.mcpToolsUsed === false))) {
    announce('This conversation belongs to the older website transport. Start New chat to use your ChatGPT plan. Nothing was sent.');
    return;
  }
  // The user's own Send tap authorizes this exact chat turn. No additional
  // modal or Workspace tool controls are required for ordinary conversation.
  const requestId = createChatRequestId();
  authorizedChatgptPending = true;
  let verifiedCompletion = false;
  let completedSessionId = previous;
  setBusy(true);
  startAndroidChatWork(requestId,prompt,'Waiting for a confirmed ChatGPT reply');
  $('prompt').value = '';
  updateCharCount();
  displayMessages({messages:[...(old?.messages || []),{role:'user',text:prompt}]},true);
  announce(''); // The working indicator is already visible in chat.
  try {
    const result = await invoke<any>('chatgpt_oauth_chat',
      {requestId,prompt,sessionId:previous,userInitiated:true});
    if (result?.requestId !== requestId || result?.state !== 'completed' ||
        result?.saved !== true || result?.status !== 'confirmed' ||
        typeof result.sessionId !== 'string' || !/^[0-9a-f-]{36}$/i.test(result.sessionId) ||
        typeof result.answer !== 'string' || !result.answer.trim() ||
        new TextEncoder().encode(result.answer).length > 20_000 ||
        !Array.isArray(result.actions) || result.actions.length !== 0)
      throw Error(result?.state === 'failed_before_dispatch'
        ? `No request was sent. ${authorizedChatgptFailureReason(result)}`
        : result?.state === 'uncertain'
          ? `Request outcome uncertain. ${authorizedChatgptFailureReason(result)} Do not resend the same message.`
          : 'No confirmed completed ChatGPT response was returned.');
    const stillOnOriginal = selectedSession === previous &&
      $('provider').value === 'chatgpt-browser';
    if (stillOnOriginal && $('prompt').value.trim() === prompt) {
      $('prompt').value = '';
      updateCharCount();
    }
    if (stillOnOriginal) selectedSession = result.sessionId;
    await refresh();
    const savedChat=state.sessions.find(s => s.id === result.sessionId);
    finishAndroidChatWork(savedChat,result.actions);
    if (stillOnOriginal) displayMessages(savedChat || null);
    verifiedSiwcChat = true;
    verifiedAiConnection = {provider:'chatgpt-browser',model:'chatgpt.com'};
    updateSidebarConnection();
    verifiedCompletion = true;
    completedSessionId = result.sessionId;
    announce(''); // Finished-turn OS notifications follow the user's preference.
    void sendAndroidFinishedNotification(result.answer,requestId);
  } catch (error) {
    endAndroidChatWork();
    if (old) displayMessages(old); else displayMessages(null);
    if (!$('prompt').value.trim() && !androidDuringTurnPending) {
      $('prompt').value = prompt;
      updateCharCount();
    }
    announce(`ChatGPT plan request not confirmed: ${String(error).slice(0,200)}. No automatic retry.`,true);
  } finally {
    endAndroidChatWork();
    authorizedChatgptPending = false;
    setBusy(false);
    finishAndroidDuringTurn(verifiedCompletion,completedSessionId);
  }
}
type DirectAndroidChatAction = {action:'android_browser_start'|'android_app_start'|
  'android_browser_stop'|'android_app_stop';arguments:{url:string}|{query:string}|Record<string,never>};
/** Only explicit user-authored instructions are eligible for native direct
 * actions. The native core independently re-parses the same original prompt;
 * renderer-provided action or package data is never sufficient authorization. */
function directAndroidChatAction(prompt:string):DirectAndroidChatAction|null {
  const text = prompt.trim();
  if (text.length < 8 || text.length > 2048 || /[\r\n\x00-\x1F\x7F]/.test(text)) return null;
  if (/^(?:stop browser|tutup browser|close browser|stop webview|tutup pelayar)$/i.test(text))
    return {action:'android_browser_stop',arguments:{}};
  if (/^(?:stop app|stop aplikasi|tutup aplikasi|stop automasi app|stop app automation)$/i.test(text))
    return {action:'android_app_stop',arguments:{}};
  const command = /^(?:(?:tolong|please|sila)\s+)?(?:buka|bukak|bukakan|open|launch|jalankan|visit|lawat)\s+(?:(?:app|aplikasi)\s+)?(.+)$/iu.exec(text);
  const target = (command ? command[1] : text).trim();
  if (/^https:\/\/[^\s@/#?]+(?:[/?#][^\s]*)?$/i.test(target))
    return {action:'android_browser_start',arguments:{url:'https://'+target.slice(8)}};
  if (!command || target.length < 2 || target.length > 80 ||
      /[:/?#\\<>\[\]{}]/.test(target)) return null;
  return {action:'android_app_start',arguments:{query:target}};
}
async function submitDirectAndroidChat(prompt:string,task:DirectAndroidChatAction):Promise<void> {
  if (!nativeAndroidRuntime || !isTauri() || working) return;
  const previousChatId = selectedSession;
  const old = previousChatId ? state.sessions.find(s => s.id === previousChatId) : null;
  const nativeSession = old?.provider === 'local-automation' && old.model === 'native-agent'
    ? previousChatId : null;
  const oldNative = nativeSession ? old : null;
  const requestId = createChatRequestId();
  let verified = false;
  let completedId = nativeSession;
  activeChatRequestId = requestId;
  setBusy(true);
  startAndroidChatWork(requestId,prompt,task.action==='android_browser_start'
    ? 'Opening isolated browser' : task.action==='android_app_start'
      ? 'Opening installed Android app' : 'Stopping Android automation');
  $('prompt').value = '';
  updateCharCount();
  displayMessages({messages:[...(oldNative?.messages || []),{role:'user',text:prompt}]},true);
  announce(task.action === 'android_browser_start'
    ? 'Starting an isolated browser for your chat link…'
    : task.action === 'android_app_start'
      ? 'Finding the installed app and starting its blue Android indicator…'
      : 'Stopping the previously authorized Android automation…',true);
  try {
    const result = await api('/api/chat-local-action',{
      prompt,action:task.action,arguments:task.arguments,requestId,
      sessionId:nativeSession,userInitiated:true
    });
    if (typeof result?.sessionId !== 'string' || !/^[\da-f-]{36}$/i.test(result.sessionId) ||
        typeof result.answer !== 'string' || !result.answer.trim() ||
        !Array.isArray(result.actions) || result.actions.length !== 1 ||
        result.actions[0]?.name !== task.action)
      throw Error('Native Android automation did not return a confirmed chat receipt');
    selectedSession = result.sessionId;
    await refresh();
    const savedChat=state.sessions.find(s => s.id === result.sessionId);
    finishAndroidChatWork(savedChat,result.actions);
    displayMessages(savedChat || null);
    verified = true;
    completedId = result.sessionId;
    announce(task.action === 'android_browser_start'
      ? 'Headless app-owned browser started from chat. Chrome was not opened.'
      : task.action === 'android_app_start'
        ? 'Android app launch confirmed. The blue indicator follows Accessibility status.'
        : 'Native Stop confirmed for this Android session.',true);
    void sendAndroidFinishedNotification(result.answer,requestId);
  } catch (error) {
    endAndroidChatWork();
    selectedSession = previousChatId;
    if (old) displayMessages(old); else displayMessages(null);
    if (!$('prompt').value.trim()) {
      $('prompt').value = prompt;
      updateCharCount();
    }
    announce(`Android chat automation could not be confirmed: ${String(error).slice(0,170)}. Review status before retrying.`,true);
  } finally {
    endAndroidChatWork();
    activeChatRequestId = null;
    setBusy(false);
    finishAndroidDuringTurn(verified,completedId);
  }
}
$('chatForm').addEventListener('submit', async event => {
  event.preventDefault();
  if (working && nativeAndroidRuntime && !pendingModelToolApproval) {
    const followUp = $('prompt').value.trim();
    if (followUp) acceptAndroidDuringTurn(followUp);
    return;
  }
  if (working || pendingModelToolApproval) return;
  const prompt = $('prompt').value.trim();
  if (!prompt) return;
  if (nativeAndroidRuntime && androidPlanArmed) {
    openAndroidPlanEditor();
    return;
  }
  if (nativeAndroidRuntime && androidComposeMode === 'goal') {
    openGoalPlannerFromComposer();
    return;
  }
  const provider = $('provider').value;
  const model = $('model').value.trim();
  renderAndroidReasoning();
  const directTask = nativeAndroidRuntime && isTauri() ? directAndroidChatAction(prompt) : null;
  if (directTask) {
    await submitDirectAndroidChat(prompt,directTask);
    return;
  }
  if (provider === 'chatgpt-browser') {
    if (nativeAndroidRuntime) await submitAuthorizedChatgpt(prompt);
    else await submitOwnedBrowserChat(prompt);
    return;
  }
  if (!provider) {
    announce(nativeAndroidRuntime ? 'Select a configured AI provider.' :
      'Select a provider in Connections & tools');
    setSettings(true);
    return;
  }
  if (provider !== 'hermes' && !model) {
    announce(nativeAndroidRuntime ? 'Enter an API model ID in Setup → Step 5 Access. Nothing sent.' :
      'Enter a model ID for the selected provider');
    if (nativeAndroidRuntime) {
      setSettings(true); setAndroidSettingsView('setup'); chooseAndroidCosSetupStep('tools');
    } else { setSettings(true); $('model').focus(); }
    return;
  }
  if (!['ollama', 'hermes'].includes(provider) && !state.hasKeys?.includes(provider)) {
    announce(nativeAndroidRuntime ? 'Add a separate model API key in Setup → Step 5 Access. Nothing sent.' :
      'Add an API key in Connections & tools to send messages');
    if (nativeAndroidRuntime) {
      setSettings(true); setAndroidSettingsView('setup'); chooseAndroidCosSetupStep('tools');
    } else {
      setSettings(true);
      $('keyBox').classList.remove('hidden');
      $('keyToggle').setAttribute('aria-expanded', 'true');
      $('apiKey').focus();
    }
    return;
  }
  const allowMcpTools = nativeAndroidRuntime && isTauri() && mobileMcpConnected &&
    mobileMcpTools.length > 0 && $('mobileMcpModelOptIn').checked;
  const allowLocalTools = nativeAndroidRuntime && isTauri() && !allowMcpTools &&
    ['openai','anthropic','openrouter'].includes(provider) && !!model &&
    !!state.hasKeys?.includes(provider);
  $('mobileMcpModelOptIn').checked = false; // never persistent across chat turns
  activeChatRequestId = nativeAndroidRuntime && isTauri() ? createChatRequestId() : null;
  // A provider/model switch creates a fresh conversation and preserves the
  // existing one in history. Never attach API tool receipts to OAuth history.
  const oldSelection = selectedSession ? state.sessions.find(s => s.id === selectedSession) : null;
  const previous = oldSelection && oldSelection.provider === provider && oldSelection.model === model
    ? selectedSession : null;
  if (oldSelection && !previous) selectedSession = null;
  let verifiedCompletion = false;
  let completedSessionId = previous;
  setBusy(true);
  if (nativeAndroidRuntime) startAndroidChatWork(
    activeChatRequestId || createChatRequestId(),prompt,
    allowLocalTools ? 'Waiting for Android tool decision' :
      allowMcpTools ? 'Waiting for MCP tool decision' : 'Waiting for AI response');
  if (nativeAndroidRuntime) announce('');
  else announce('Sending request to agent…');
  const before = state?.sessions?.find(session => session.id === previous);
  if (nativeAndroidRuntime) {
    $('prompt').value = '';
    updateCharCount();
  }
  displayMessages({ messages: [...(before?.messages || []), { role: 'user', text: prompt }] }, true);
  if (activeChatRequestId && !allowMcpTools && !allowLocalTools) {
    stopChatStreamPolling();
    const requestId = activeChatRequestId;
    streamProgressTimer = setTimeout(() => void pollChatStream(requestId), 240);
  }
  try {
    const result = await api('/api/chat', { prompt, provider, model,
      sessionId: previous, allowMcpTools, allowLocalTools,
      ...(androidReasoningSupported() && androidReasoningEffort!=='auto'
        ? {reasoningEffort:androidReasoningEffort} : {}),
      ...(activeChatRequestId ? { requestId: activeChatRequestId } : {}) });
    if (result.pendingTool) {
      endAndroidChatWork();
      const proposal = result.pendingTool;
      if (typeof proposal.reviewId !== 'string' || typeof proposal.requestId !== 'string' ||
          proposal.requestId !== activeChatRequestId || typeof proposal.name !== 'string' ||
          proposal.name.length > 64 || !proposal.arguments || Array.isArray(proposal.arguments) ||
          typeof proposal.arguments !== 'object' || JSON.stringify(proposal.arguments).length > 8192)
        throw Error('AI proposed an invalid tool; no tool was executed');
      pendingModelToolApproval = {reviewId:proposal.reviewId,requestId:proposal.requestId,
        name:proposal.name,arguments:proposal.arguments,provider,model,previous,
        originalPrompt:prompt};
      $('chatToolApprovalName').textContent = proposal.name;
      $('chatToolApprovalArguments').textContent = JSON.stringify(proposal.arguments,null,2);
      $('chatToolApprovalPanel').classList.remove('hidden');
      displayMessages({messages:[...(before?.messages || []),{role:'user',text:prompt}]});
      $('send').disabled = true;
      updateSidebarConnection();
      announce(`AI proposed one ${proposal.name.startsWith('android_') ? 'Android' : 'MCP'} tool call. It has not executed. Review and approve or reject the exact arguments.`,true);
      return;
    }
    selectedSession = result.sessionId;
    if (!nativeAndroidRuntime || $('prompt').value.trim() === prompt) {
      $('prompt').value = '';
      updateCharCount();
    }
    await refresh();
    let assignmentError: string | null = null;
    if (!previous && selectedProjectId && !projectAssignments[result.sessionId]) {
      if (nativeProjectMode && nativeAndroidRuntime && isTauri()) {
        try {
          await updateNativeProjects('assign',
            {chatId: result.sessionId, projectId: selectedProjectId});
        } catch (error) {
          assignmentError = String(error.message);
        }
      } else {
        projectAssignments[result.sessionId] = selectedProjectId;
        if (!saveProjectLabels()) {
          delete projectAssignments[result.sessionId];
          assignmentError = 'Local project label could not be saved';
        }
      }
    }
    const savedChat=state.sessions.find(session => session.id === selectedSession);
    finishAndroidChatWork(savedChat,result.actions);
    displayMessages(savedChat);
    verifiedAiConnection = { provider, model };
    updateSidebarConnection();
    verifiedCompletion = true;
    completedSessionId = result.sessionId;
    if (assignmentError) announce(`Response received; project assignment failed: ${assignmentError}`);
    else if (!nativeAndroidRuntime) announce(result.actions.length ?
      `${result.actions.length} tool action(s) recorded` : 'Response received',true);
    if (typeof result.answer === 'string' && result.answer.trim() && activeChatRequestId)
      void sendAndroidFinishedNotification(result.answer,activeChatRequestId);
  } catch (error) {
    endAndroidChatWork();
    // A failed later chat does not undo the evidence of the last successful
    // reply. A changed provider/model/key still invalidates its use in Setup.
    updateSidebarConnection();
    if (before) displayMessages(before);
    else displayMessages(null);
    if (nativeAndroidRuntime && !$('prompt').value.trim() && !androidDuringTurnPending) {
      $('prompt').value = prompt;
      updateCharCount();
    }
    announce(`Request failed: ${error.message}`);
  } finally {
    endAndroidChatWork();
    stopChatStreamPolling();
    activeChatRequestId = null;
    setBusy(false);
    finishAndroidDuringTurn(verifiedCompletion,completedSessionId);
  }
});

$('prompt').addEventListener('keydown', event => {
  // The Android Send button or Ctrl/Meta+Enter submits; regular Enter inserts
  // a newline. The removed General shortcut no longer changes this behavior.
  const androidNewline = nativeAndroidRuntime && !event.ctrlKey && !event.metaKey;
  if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && !androidNewline) {
    event.preventDefault();
    $('chatForm').requestSubmit();
  }
});

function newConversation() {
  if (working) return;
  ++speechContextRevision;
  if (nativeAndroidRuntime) {
    androidComposeMode = 'normal';
    setAndroidPlanMode(false);
    $('androidComposeModeLabel').textContent = 'Normal';
    $('androidComposeMode').dataset.mode = 'normal';
    $('androidComposeMode').setAttribute('aria-label','Chat mode: Normal');
    for (const button of document.querySelectorAll<HTMLButtonElement>('[data-compose-mode]'))
      button.setAttribute('aria-pressed',String(button.dataset.composeMode === 'normal'));
    closeAndroidComposerMenus();
  }
  closeAndroidViewMenu();
  selectedSession = null;
  displayMessages(null);
  renderThreads();
  announce('');
  closeSidebar();
  setSettings(false);
  closeToolPanel();
  closeExtrasPanel();
  $('prompt').focus();
}
$('newChat').addEventListener('click', newConversation);
$('newChatTop').addEventListener('click', newConversation);

let sidebarOpen = false;
function renderSidebar(open: boolean) {
  sidebarOpen = open;
  const sidebar = $('sidebar') as HTMLElement;
  const main = document.querySelector<HTMLElement>('.main')!;
  if (!open && sidebar.contains(document.activeElement)) $('sidebarToggle').focus({ preventScroll: true });
  if (nativeAndroidRuntime && open && main.contains(document.activeElement))
    $('sidebarToggle').focus({ preventScroll: true });
  sidebar.classList.toggle('open', open);
  sidebar.inert = nativeAndroidRuntime && !open;
  if (nativeAndroidRuntime) {
    // Chat stays mounted with its draft/session untouched, but cannot receive
    // focus or clicks while the full-screen navigation is open.
    main.inert = open;
    document.querySelector('.shell')?.classList.toggle('android-sidebar-open', open);
    if (!open) {
      sidebar.classList.remove('is-searching');
      if ($('sidebarSearch').value) { $('sidebarSearch').value = ''; renderThreads(); }
    }
  }
  $('sidebarBackdrop').classList.toggle('hidden', nativeAndroidRuntime || !open);
  $('sidebarToggle').setAttribute('aria-expanded', String(open));
  $('sidebarToggle').setAttribute('aria-label', open ? 'Close conversations' : 'Open conversations');
}
function closeSidebar() { renderSidebar(false); }
function toggleSidebar() {
  closeAndroidViewMenu();
  if (nativeAndroidRuntime && !$('settingsPanel').classList.contains('hidden')) {
    setAndroidSettingsNavOpen($('settingsPanel').dataset.settingsNavOpen !== 'true');
    return;
  }
  const opened = !sidebarOpen;
  renderSidebar(opened);
  if (opened) setSettings(false);
  if (opened) closeToolPanel();
  if (opened) closeExtrasPanel();
  if (opened && nativeAndroidRuntime) void refreshNativeProjectCatalog();
}
$('sidebarToggle').addEventListener('click', toggleSidebar);
$('sidebarBackdrop').addEventListener('click', closeSidebar);
if (nativeAndroidRuntime) {
  // Keep one gesture owner for chat and Settings. Lock direction before
  // intercepting touchmove so vertical lists still scroll normally.
  type SidebarSwipe = {
    id: number; x: number; y: number; opening: boolean;
    surface: 'chat' | 'settings'; width: number;
    direction: 'pending' | 'horizontal' | 'vertical'; startedAt: number;
  };
  let swipe: SidebarSwipe | null = null;
  const settingsPanel = $('settingsPanel') as HTMLElement;
  const settingsNav = $('androidSettingsNav') as HTMLElement;
  const shell = document.querySelector<HTMLElement>('.shell')!;
  const chatSidebar = $('sidebar') as HTMLElement;
  function clearSwipePreview() {
    shell.classList.remove('android-sidebar-dragging');
    settingsPanel.classList.remove('android-settings-swiping');
    chatSidebar.style.removeProperty('--android-sidebar-swipe-x');
    settingsNav.style.removeProperty('--android-settings-swipe-x');
  }
  function cancelSwipe() { swipe = null; clearSwipePreview(); }
  document.addEventListener('touchstart', event => {
    cancelSwipe();
    if (event.touches.length !== 1 ||
        !$('androidViewMenu').classList.contains('hidden') ||
        !$('androidComposeModeMenu').classList.contains('hidden')) return;
    const target = event.target;
    if (!(target instanceof Element) ||
        target.closest('input, textarea, select, [contenteditable="true"]')) return;
    const touch = event.touches[0];
    const settingsVisible = !settingsPanel.classList.contains('hidden');
    const settingsCompact = window.matchMedia('(max-width: 699px)').matches;
    if (settingsVisible && !settingsCompact) return;
    const surface: SidebarSwipe['surface'] = settingsVisible ? 'settings' : 'chat';
    const opening = settingsVisible ? settingsPanel.dataset.settingsNavOpen !== 'true' : !sidebarOpen;
    const drawer = settingsVisible ? settingsNav : chatSidebar;
    const inDrawer = settingsVisible ? !!target.closest('#androidSettingsNav') : !!target.closest('#sidebar');
    const inContent = settingsVisible ? !!target.closest('#androidSettingsContent') :
      !!target.closest('.main, .topbar');
    // Let the user start anywhere in the left two-thirds of content. The
    // Android system owns its first few edge pixels for Back navigation.
    // A button tap stays a tap unless a deliberate horizontal drag occurs.
    const startLimit = Math.max(180, window.innerWidth * .7);
    if (opening ? (!inContent || touch.clientX < 10 || touch.clientX > startLimit) : !inDrawer) return;
    swipe = {id: touch.identifier, x: touch.clientX, y: touch.clientY,
      opening, surface, width: drawer.getBoundingClientRect().width,
      direction: 'pending', startedAt: performance.now()};
  }, {capture: true, passive: true});
  document.addEventListener('touchmove', event => {
    const start = swipe;
    if (!start) return;
    if (event.touches.length !== 1) { cancelSwipe(); return; }
    const touch = Array.from(event.touches).find(item => item.identifier === start.id);
    if (!touch) { cancelSwipe(); return; }
    const dx = touch.clientX - start.x, dy = touch.clientY - start.y;
    if (start.direction === 'pending' && Math.max(Math.abs(dx), Math.abs(dy)) >= 8)
      start.direction = Math.abs(dx) > Math.abs(dy) * 1.08 ? 'horizontal' : 'vertical';
    if (start.direction === 'vertical') { cancelSwipe(); return; }
    if (start.direction !== 'horizontal') return;
    if (event.cancelable) event.preventDefault();
    // A short thumb swipe reveals a useful portion immediately instead of
    // moving a full-screen drawer only a few pixels.
    const x = Math.max(-start.width, Math.min(0,
      (start.opening ? -start.width : 0) + dx * 1.65));
    if (start.surface === 'settings') {
      settingsPanel.classList.add('android-settings-swiping');
      settingsNav.style.setProperty('--android-settings-swipe-x', `${x}px`);
    } else {
      shell.classList.add('android-sidebar-dragging');
      chatSidebar.style.setProperty('--android-sidebar-swipe-x', `${x}px`);
    }
  }, {capture: true, passive: false});
  document.addEventListener('touchend', event => {
    const start = swipe;
    if (!start) return;
    const touch = Array.from(event.changedTouches).find(item => item.identifier === start.id);
    cancelSwipe();
    if (!touch || event.touches.length) return;
    const dx = touch.clientX - start.x, dy = Math.abs(touch.clientY - start.y);
    const distance = Math.abs(dx), elapsed = Math.max(1, performance.now() - start.startedAt);
    const shouldSwitch = start.direction !== 'vertical' &&
      (distance >= 42 || (distance >= 34 && distance / elapsed >= .35)) &&
      distance > dy * 1.08 && (start.opening ? dx > 0 : dx < 0);
    if (!shouldSwitch) return;
    if (start.surface === 'settings') setAndroidSettingsNavOpen(start.opening);
    else if (start.opening) toggleSidebar();
    else closeSidebar();
  }, {capture: true, passive: true});
  document.addEventListener('touchcancel', cancelSwipe, {capture: true, passive: true});
}

function updateCharCount() {
  $('charCount').textContent = String($('prompt').value.length);
}
$('prompt').addEventListener('input', updateCharCount);
$('speechDictation').addEventListener('click', async () => {
  if (!nativeAndroidRuntime || !isTauri() || speechDictationBusy || working ||
      pendingModelToolApproval !== null) return;
  // Android's recognition UI may use an external/system online provider. The
  // app does not receive microphone audio and never sends the resulting text.
  if (!window.confirm('Open the Android speech recognizer? Its speech service may process audio online. Only the resulting text will be inserted into this draft for you to review. Nothing will be sent automatically.')) return;
  const prompt = $('prompt') as HTMLTextAreaElement;
  const before = prompt.value;
  const selection = [prompt.selectionStart, prompt.selectionEnd];
  const session = selectedSession;
  const contextRevision = speechContextRevision;
  const requestId = createChatRequestId();
  const button = $('speechDictation') as HTMLButtonElement;
  speechDictationBusy = true;
  button.disabled = true;
  button.setAttribute('aria-busy', 'true');
  announce('Checking Android speech recognizer. No recording has started in this app.');
  try {
    const status = await invoke<{supported:boolean;available:boolean;listening:boolean;source:string;autoSent:boolean}>(
      'speech_dictation_status');
    if (status?.source !== 'Android speech recognizer' || status.autoSent !== false ||
        typeof status.supported !== 'boolean' || typeof status.available !== 'boolean' ||
        typeof status.listening !== 'boolean' || !status.supported || !status.available || status.listening)
      throw Error('Android speech recognition is unavailable or already active');
    if (speechContextRevision !== contextRevision || selectedSession !== session ||
        prompt.value !== before || working || pendingModelToolApproval) {
      announce('Draft or chat changed; speech recognition was not opened.');
      return;
    }
    announce('Android speech recognizer is open. You can cancel in Android.');
    const result = await invoke<{requestId:string;recognized:boolean;cancelled:boolean;text:string;source:string;autoSent:boolean}>(
      'speech_dictation_recognize', {requestId,userInitiated:true});
    if (result?.requestId !== requestId || result.source !== 'Android speech recognizer' ||
        result.autoSent !== false || typeof result.recognized !== 'boolean' ||
        typeof result.cancelled !== 'boolean' || result.recognized === result.cancelled ||
        typeof result.text !== 'string' ||
        (result.recognized ? !result.text.trim() || new TextEncoder().encode(result.text).length > 2000 ||
          /[\x00-\x1f\x7f\u061c\u200b-\u200f\u202a-\u202e\u2060-\u206f\ufeff]/u.test(result.text)
          : result.text !== '')) throw Error('Android returned an invalid speech receipt');
    if (result.cancelled) { announce('Dictation cancelled. Draft unchanged.'); return; }
    if (speechContextRevision !== contextRevision || selectedSession !== session ||
        prompt.value !== before || working || pendingModelToolApproval) {
      announce('Dictation returned after draft/chat changed. Nothing inserted; no message sent.');
      return;
    }
    const left = before.slice(0,selection[0]);
    const right = before.slice(selection[1]);
    const leading = left && !/\s$/.test(left) ? ' ' : '';
    const trailing = right && !/^\s/.test(right) ? ' ' : '';
    const inserted = leading + result.text + trailing;
    const next = left + inserted + right;
    if (prompt.maxLength >= 0 && next.length > prompt.maxLength) {
      announce('Dictated text exceeds composer limit. Existing draft preserved.');
      return;
    }
    prompt.value = next;
    prompt.setSelectionRange(left.length + inserted.length, left.length + inserted.length);
    updateCharCount();
    prompt.focus();
    announce('Speech inserted into draft. Review or edit before pressing Send.',true);
  } catch (error) {
    announce(`Dictation unavailable: ${String(error.message).slice(0,180)}. Draft unchanged.`);
  } finally {
    speechDictationBusy = false;
    button.disabled = working;
    button.setAttribute('aria-busy', 'false');
  }
});
document.addEventListener('keydown', event => {
  if (event.key === 'Escape') {
    if (nativeAndroidRuntime && !$('settingsPanel').classList.contains('hidden') &&
        $('settingsPanel').dataset.settingsNavOpen !== 'true') {
      returnToAndroidSettingsDirectory();
      return;
    }
    if (nativeAndroidRuntime && !['androidComposeAddMenu','androidComposeModeMenu','androidComposeMoreMenu']
      .every(id => $(id).classList.contains('hidden'))) { closeAndroidComposerMenus(); return; }
    if (!$('androidViewMenu').classList.contains('hidden')) {
      closeAndroidViewMenu();
      $('sidebarQuickSearch').focus({ preventScroll: true });
      return;
    }
    closeToolPanel();
    closeExtrasPanel();
    closeSidebar();
    setSettings(false);
    $('keyBox').classList.add('hidden');
    $('keyToggle').setAttribute('aria-expanded', 'false');
  }
  if (nativeAndroidRuntime && (event.ctrlKey || event.metaKey)) {
    const key = event.key.toLowerCase();
    if (key === 'k') { event.preventDefault(); openAndroidChatSearch(); return; }
    if (key === 'n') { event.preventDefault(); newConversation(); return; }
    if (key === '+' || key === '=') { event.preventDefault(); setAndroidViewZoom(androidViewZoom + .1); return; }
    if (key === '-') { event.preventDefault(); setAndroidViewZoom(androidViewZoom - .1); return; }
    if (key === '0') { event.preventDefault(); setAndroidViewZoom(1); return; }
  }
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
    event.preventDefault();
    newConversation();
  }
});

const androidSettingsLabels: Record<string, string> = {
  workspace: 'Workspace', general: 'General', usage: 'Usage', setup: 'Setup',
  plugins: 'Plugins', agents: 'Agents & automation', appearance: 'Appearance', activity: 'Activity',
};
type AndroidAppearance = {theme:'dark'|'oled';density:'comfortable'|'compact';reduceMotion:boolean};
const androidAppearanceStorageKey='mcp-agent-ai-android-appearance-v1';
function readAndroidAppearance():AndroidAppearance {
  const defaults:AndroidAppearance={theme:'dark',density:'comfortable',reduceMotion:false};
  try{
    const value=JSON.parse(localStorage.getItem(androidAppearanceStorageKey)||'null');
    return value&&typeof value==='object' ? {
      theme:value.theme==='oled'?'oled':'dark',
      density:value.density==='compact'?'compact':'comfortable',
      reduceMotion:value.reduceMotion===true
    } : defaults;
  }catch{return defaults;}
}
let androidAppearance=readAndroidAppearance();
function renderAndroidAppearance():void {
  if(!nativeAndroidRuntime)return;
  document.documentElement.dataset.appearance=androidAppearance.theme;
  document.documentElement.dataset.density=androidAppearance.density;
  document.documentElement.dataset.reduceMotion=String(androidAppearance.reduceMotion);
  ($('androidAppearanceTheme') as HTMLSelectElement).value=androidAppearance.theme;
  ($('androidAppearanceDensity') as HTMLSelectElement).value=androidAppearance.density;
  ($('androidAppearanceReduceMotion') as HTMLInputElement).checked=androidAppearance.reduceMotion;
}
function updateAndroidAppearance():void {
  androidAppearance={
    theme:($('androidAppearanceTheme') as HTMLSelectElement).value==='oled'?'oled':'dark',
    density:($('androidAppearanceDensity') as HTMLSelectElement).value==='compact'?'compact':'comfortable',
    reduceMotion:($('androidAppearanceReduceMotion') as HTMLInputElement).checked
  };
  try{localStorage.setItem(androidAppearanceStorageKey,JSON.stringify(androidAppearance));}catch{}
  renderAndroidAppearance();
}
function renderAndroidUsageAndActivity():void {
  if(!nativeAndroidRuntime)return;
  $('androidUsageChats').textContent=String(state.sessions.length);
  $('androidUsageReplies').textContent=String(state.sessions.reduce((sum,session)=>
    sum+session.messages.filter(message=>message.role==='assistant').length,0));
  $('androidUsagePrime').textContent=mobilePrimeVerified
    ? String((mobilePrimeTask?.workers.length||0)+mobilePrimeHistory.reduce((sum,task)=>
      sum+task.workers.length,0)) : '—';
  $('androidActivityPrime').textContent=mobilePrimeVerified
    ? mobilePrimeTask?.status||'Idle' : 'Unverified';
  $('androidActivityCore').textContent=androidCorePluginVerified()?'Linked':'Not verified';
}
function loadAndroidDeveloperMode():boolean {
  try{return localStorage.getItem('mcp-agent-ai-android-developer-v1')==='on';}
  catch{return false;}
}
function showAndroidDeveloperMode(enabled:boolean):void {
  ($('androidDeveloperMode') as HTMLInputElement).checked=enabled;
  $('androidDeveloperControls').classList.toggle('hidden',!enabled);
}
type OwnedChatgptBrowserStatus = {
  supported: boolean; running: boolean; visible: boolean;
  sessionLinked?: boolean;
  signInVerified?: boolean;
  engine?: string; message?: string;
};
let ownedChatgptStatus: OwnedChatgptBrowserStatus | null = null;
let ownedChatgptBusy = false;
let ownedChatgptStatusEpoch = 0;
let ownedChatgptStatusError = '';
// COS separates website sign-in from its bridge connection. Here the only
// proof available is a read-only *composer* probe, NOT login or a received reply.
// It is deliberately volatile and invalidated on browser lifecycle changes.
let ownedChatgptComposerReady = false;
function renderOwnedChatgptSteps() {
  const running = ownedChatgptStatus?.supported === true && ownedChatgptStatus.running;
  const visible = running && ownedChatgptStatus?.visible === true;
  const selected = !nativeAndroidRuntime && $('provider').value === 'chatgpt-browser';
  const steps: Array<[string, boolean, string]> = [
    ['backgroundChatgptStepOpen', !!visible,
      visible ? 'Browser open. Complete sign-in in that window if needed.' :
        running ? 'Browser hidden. Show it to finish sign-in or verify the page.' :
          'Open the private browser and sign in manually.'],
    ['backgroundChatgptStepCheck', !!visible && ownedChatgptComposerReady,
      visible && ownedChatgptComposerReady
        ? 'Composer detected on the owned ChatGPT website. Account sign-in not independently verified.'
        : 'Check page readiness after sign-in. No cookies, chat history or text are read.'],
    ['backgroundChatgptStepSelect', selected,
      selected ? 'ChatGPT website is selected. A successful message exchange is still unverified.' :
        nativeAndroidRuntime ? 'Android AI Connection uses a separate provider; browser page checks do not select one.' :
        'Use ChatGPT in chat after checking its page. No API key required.'],
  ];
  for (const [id, done, description] of steps) {
    const row = $(id);
    row.dataset.stepState = done ? 'ready' : 'pending';
    row.querySelector('span')!.textContent = description;
  }
}
function nativeChatgptCommand(action: 'status' | 'probe' | 'open' | 'hide' | 'show' | 'close') {
  const prefix = nativeAndroidRuntime ? 'android' : 'windows';
  return `${prefix}_chatgpt_${action}`;
}
function showOwnedChatgptStatus(status: OwnedChatgptBrowserStatus | null, message = '') {
  // Step 3 owns the website login, Step 8 verifies the same browser lease.
  // The separate OpenAI Platform OAuth must never substitute for this proof.
  // A verified response from an earlier account/browser session cannot be
  // reused if its authenticated owned session was lost, signed out or could
  // not be verified. Re-linking must require a fresh real response.
  if (nativeAndroidRuntime && !verifiedSiwcChat && status?.sessionLinked !== true &&
      verifiedAiConnection?.provider === 'chatgpt-browser')
    verifiedAiConnection = null;
  ownedChatgptStatus = status;
  const supported = isTauri() && status?.supported === true;
  const running = supported && status.running;
  if (!running || !status?.visible) ownedChatgptComposerReady = false;
  for (const [id, show] of [
    ['backgroundChatgptOpen', !running],
    ['backgroundChatgptShow', !!running && !status?.visible],
    ['backgroundChatgptHide', !!running && !!status?.visible],
    ['backgroundChatgptProbe', !!running && !!status?.visible],
    ['backgroundChatgptUse', !nativeAndroidRuntime && !!running && !!status?.visible && ownedChatgptComposerReady &&
      $('provider').value !== 'chatgpt-browser'],
    ['backgroundChatgptReconcile', !!running && !!pendingOwnedBrowserTurn],
    ['backgroundChatgptClose', !!running],
  ] as Array<[string, boolean]>) {
    const button = $(id) as HTMLButtonElement;
    button.classList.toggle('hidden', !show);
    button.disabled = ownedChatgptBusy || !supported;
  }
  $('backgroundChatgptStatus').textContent = message || status?.message ||
    (isTauri() ? 'Built-in ChatGPT browser backend unavailable. Nothing was opened.' :
      'Available only in the installed native app. Nothing was opened.');
  renderOwnedChatgptSteps();
  if (nativeAndroidRuntime) {
    updateSidebarConnection();
    // Native auto-sign-in probes can complete after Step 3 has rendered.
    // Re-render so Bring Session is enabled immediately after the WebView
    // verifies an account, without a second user action or fake receipt.
    if (!$('settingsPanel').classList.contains('hidden') &&
        $('settingsPanel').dataset.settingsView === 'setup' &&
        androidCosSetupSelected === 'browser') renderAndroidCosSetup();
  }
}
async function refreshOwnedChatgptStatus() {
  const epoch = ++ownedChatgptStatusEpoch;
  if (!isTauri()) { showOwnedChatgptStatus(null); return; }
  try {
    const status = await invoke<OwnedChatgptBrowserStatus>(nativeChatgptCommand('status'));
    if (epoch === ownedChatgptStatusEpoch && !ownedChatgptBusy) {
      ownedChatgptStatusError = '';
      showOwnedChatgptStatus(status);
    }
  } catch (error) {
    if (epoch === ownedChatgptStatusEpoch && !ownedChatgptBusy) {
      ownedChatgptStatusError = 'Private ChatGPT browser status is unavailable. Try Sign in again; no session is assumed.';
      showOwnedChatgptStatus(null, ownedChatgptStatusError);
    }
  }
}
async function changeOwnedChatgpt(action: 'open' | 'show' | 'hide' | 'close'): Promise<boolean> {
  if (!isTauri() || ownedChatgptBusy || ownedChatgptStatus?.supported !== true) return false;
  ownedChatgptBusy = true;
  ownedChatgptComposerReady = false;
  ownedChatgptStatusEpoch++;
  showOwnedChatgptStatus(ownedChatgptStatus, 'Checking the built-in browser…');
  try {
    const status = await invoke<OwnedChatgptBrowserStatus>(nativeChatgptCommand(action),
      action === 'open' ? { userInitiated: true } : undefined);
    if (!status || typeof status.running !== 'boolean' || typeof status.visible !== 'boolean')
      throw new Error('Native browser returned no valid lifecycle receipt');
    ownedChatgptBusy = false;
    ownedChatgptStatusError = '';
    showOwnedChatgptStatus(status);
    return true;
  } catch (error) {
    ownedChatgptBusy = false;
    showOwnedChatgptStatus(ownedChatgptStatus,
      `Built-in browser ${action} failed: ${String(error).slice(0,140)}`);
    // A failed/ambiguous native lifecycle command may already have changed state.
    // Never repeat it automatically; request read-only status to reconcile.
    void refreshOwnedChatgptStatus();
    return false;
  }
}
for (const action of ['open', 'show', 'hide', 'close'] as const)
  $("backgroundChatgpt" + action[0].toUpperCase() + action.slice(1))
    .addEventListener('click', () => void changeOwnedChatgpt(action));
// Read-only native page check is shared by Check and Use; Use rechecks to
// reject an old probe after page navigation, sign-out or account switch.
// It does not inspect credentials, account identity, prompts or history.
async function verifyOwnedChatgptComposer(): Promise<boolean> {
    const result = await invoke<any>(nativeChatgptCommand('probe'));
    let ready: boolean;
    if (nativeAndroidRuntime) {
      if (result?.owned !== true || result?.origin !== 'chatgpt.com' ||
          result?.readOnly !== true || typeof result?.composerReady !== 'boolean' ||
          typeof result?.sendButtonReady !== 'boolean' ||
          !Number.isSafeInteger(result?.pageGeneration) || result.pageGeneration < 0 ||
          Object.keys(result).sort().join(',') !==
            'composerReady,origin,owned,pageGeneration,readOnly,sendButtonReady')
        throw Error('Page readiness returned an invalid or private-data receipt');
      ready = result.composerReady;
    } else {
      if (result?.supported !== true || typeof result?.ready !== 'boolean' ||
          typeof result?.writable !== 'boolean' || result?.engine !== 'WebView2' ||
          !['ready','missing','ambiguous','hidden','readonly','busy','changed'].includes(result?.state) ||
          typeof result?.message !== 'string' || result.message.length > 160 ||
          Object.keys(result).sort().join(',') !== 'engine,message,ready,state,supported,writable')
        throw Error('Windows page readiness returned an invalid or private-data receipt');
      ready = result.ready;
    }
    return ready;
}
async function checkOwnedChatgptPage(selectProvider: boolean) {
  if (nativeAndroidRuntime && selectProvider) return; // Android AI provider removed
  if (!isTauri() || ownedChatgptBusy || !ownedChatgptStatus?.running ||
      !ownedChatgptStatus.visible ||
      (selectProvider && !ownedChatgptComposerReady)) return;
  ownedChatgptBusy = true;
  const epoch = ++ownedChatgptStatusEpoch;
  showOwnedChatgptStatus(ownedChatgptStatus, 'Checking the current ChatGPT page…');
  try {
    const ready = await verifyOwnedChatgptComposer();
    if (epoch !== ownedChatgptStatusEpoch || !ownedChatgptStatus?.running ||
        !ownedChatgptStatus.visible) return;
    ownedChatgptComposerReady = ready;
    if (ready && selectProvider) {
      $('provider').value = 'chatgpt-browser';
      $('provider').dispatchEvent(new Event('change'));
    }
    showOwnedChatgptStatus(ownedChatgptStatus, !ready
      ? 'ChatGPT composer not ready. Finish sign-in or page loading, then check again.'
      : selectProvider
        ? 'ChatGPT website selected for main chat. Sign-in and reply delivery are not proven until an approved message succeeds.'
        : nativeAndroidRuntime
          ? 'ChatGPT composer detected for browsing only; AI Connection still uses your selected API provider.'
          : 'ChatGPT composer detected. This does not prove account sign-in. Choose “Use ChatGPT in chat” to select it.');
  } catch (error) {
    ownedChatgptComposerReady = false;
    if (epoch === ownedChatgptStatusEpoch)
      showOwnedChatgptStatus(ownedChatgptStatus,
        `ChatGPT page not verified: ${String(error).slice(0,140)}`);
  } finally {
    ownedChatgptBusy = false;
    if (epoch === ownedChatgptStatusEpoch)
      showOwnedChatgptStatus(ownedChatgptStatus,
        $('backgroundChatgptStatus').textContent || 'ChatGPT page check finished.');
  }
}
$('backgroundChatgptProbe').addEventListener('click', () => void checkOwnedChatgptPage(false));
$('backgroundChatgptUse').addEventListener('click', () => void checkOwnedChatgptPage(true));
$('backgroundChatgptReconcile').addEventListener('click', async () => {
  if (!isTauri() || working || ownedBrowserTurnChecking || !pendingOwnedBrowserTurn) return;
  const turn = pendingOwnedBrowserTurn;
  // Read-only reconciliation. Never prepare or click Send for this request ID again.
  await observeOwnedBrowserTurn(turn);
});
function filterAndroidSettings() {
  if (!nativeAndroidRuntime) return;
  const query = $('androidSettingsSearch').value.trim().toLocaleLowerCase();
  let count = 0;
  for (const button of document.querySelectorAll<HTMLButtonElement>('[data-android-settings-category]')) {
    const category = button.dataset.androidSettingsCategory || '';
    const text = [button.textContent || '',
      ...Array.from(document.querySelectorAll<HTMLElement>(`#androidSettingsContent > [data-setting-category="${category}"]`))
        .filter(group => !['connectionSection','backgroundChatgptSettings'].includes(group.id))
        .map(group => group.textContent || '')].join(' ').toLocaleLowerCase();
    const matches = !query || text.includes(query);
    button.classList.toggle('hidden', !matches);
    if (matches) count++;
  }
  $('androidSettingsNoResults').classList.toggle('hidden', count !== 0);
}
let androidGeneralImportPending:{source:string;preferences:AndroidGeneralPreferences}|null = null;
function renderAndroidGeneralSettings():void {
  if (!nativeAndroidRuntime) return;
  ($('androidGeneralFollowOutput') as HTMLInputElement).checked =
    androidGeneralPreferences.followOutput;
  ($('androidGeneralNotifyFinish') as HTMLInputElement).checked =
    androidGeneralPreferences.notifyOnFinish;
  ($('androidGeneralNotificationDetail') as HTMLSelectElement).value =
    androidGeneralPreferences.notificationDetail;
  ($('androidGeneralNotificationDetail') as HTMLSelectElement).disabled =
    !androidGeneralPreferences.notifyOnFinish;
  ($('androidGeneralNotificationPriority') as HTMLSelectElement).value =
    androidGeneralPreferences.notificationPriority;
  ($('androidGeneralNotificationPriority') as HTMLSelectElement).disabled =
    !androidGeneralPreferences.notifyOnFinish;
  ($('androidGeneralMessageDuringTurn') as HTMLSelectElement).value =
    androidGeneralPreferences.messageDuringTurn;
  ($('androidPrivacyFullNotification') as HTMLInputElement).checked =
    androidGeneralPreferences.notificationDetail === 'full_message';
  const hint = document.querySelector<HTMLElement>('.compose-foot .key-hint');
  if (hint) hint.textContent = '· Ctrl/⌘+Enter to send · Enter for newline';
  renderAndroidDuringTurn();
}
async function copyAndroidGeneralBackup():Promise<void> {
  const content = androidGeneralBackupJson();
  const status = $('androidGeneralBackupStatus');
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(content);
    } else {
      const temporary = document.createElement('textarea');
      temporary.value = content;
      temporary.style.cssText = 'position:fixed;left:-9999px;top:0';
      document.body.appendChild(temporary);
      try {
        temporary.focus();
        temporary.select();
        if (!document.execCommand('copy')) throw Error('Clipboard unavailable');
      } finally {temporary.remove();}
    }
    status.textContent = 'General preferences copied. No keys, chats or folder access included.';
  } catch {
    ($('androidGeneralImport') as HTMLDetailsElement).open = true;
    ($('androidGeneralBackupText') as HTMLTextAreaElement).value = content;
    status.textContent = 'Clipboard unavailable. Select and copy the JSON shown above.';
  }
}
function previewAndroidGeneralBackup():void {
  const value = ($('androidGeneralBackupText') as HTMLTextAreaElement).value.trim();
  const apply = $('androidGeneralBackupApply') as HTMLButtonElement;
  apply.disabled = true;
  androidGeneralImportPending = null;
  try {
    const settings = parseAndroidGeneralPreferences(JSON.parse(value));
    const changes = [
      settings.followOutput !== androidGeneralPreferences.followOutput && 'Follow new output',
      settings.notifyOnFinish !== androidGeneralPreferences.notifyOnFinish &&
        'Send notification when finished',
      settings.notificationDetail !== androidGeneralPreferences.notificationDetail &&
        'Notification content',
      settings.notificationPriority !== androidGeneralPreferences.notificationPriority &&
        'Notification priority',
      settings.messageDuringTurn !== androidGeneralPreferences.messageDuringTurn &&
        'Messages during a running turn'
    ].filter(Boolean);
    $('androidGeneralBackupStatus').textContent = changes.length
      ? `Preview: ${changes.join(', ')} will change. Apply to confirm.`
      : 'The backup matches your current General preferences.';
    if (changes.length) {
      androidGeneralImportPending = {source:value,preferences:settings};
      apply.disabled = false;
    }
  } catch {
    $('androidGeneralBackupStatus').textContent =
      'Invalid backup. Paste General preferences JSON (version 3, or a supported version 1/2 backup).';
  }
}
function bindAndroidGeneralSettings():void {
  if (!nativeAndroidRuntime) return;
  renderAndroidGeneralSettings();
  $('messages').addEventListener('scroll', () => {
    if (androidGeneralProgrammaticScroll) return;
    const pane = $('messages');
    if (working && androidGeneralPreferences.followOutput &&
        pane.scrollHeight - pane.clientHeight - pane.scrollTop > 2) {
      // Follow means follow: a swipe up is immediately returned to the newest
      // output instead of sticking in the middle of the stream.
      androidGeneralProgrammaticScroll = true;
      requestAnimationFrame(() => {
        pane.scrollTop = pane.scrollHeight;
        androidGeneralProgrammaticScroll = false;
      });
    } else {
      androidGeneralChatScrolledUp = pane.scrollHeight - pane.clientHeight -
        pane.scrollTop > 95;
    }
  },{passive:true});
  ($('androidGeneralFollowOutput') as HTMLInputElement).addEventListener('change',event => {
    const followOutput = (event.currentTarget as HTMLInputElement).checked;
    if (!saveAndroidGeneralPreferences({...androidGeneralPreferences,followOutput}))
      $('androidGeneralBackupStatus').textContent = 'Could not save this preference on the phone.';
  });
  ($('androidGeneralNotifyFinish') as HTMLInputElement).addEventListener('change',event => {
    const notifyOnFinish = (event.currentTarget as HTMLInputElement).checked;
    if (!saveAndroidGeneralPreferences({...androidGeneralPreferences,notifyOnFinish}))
      $('androidGeneralBackupStatus').textContent = 'Could not save this preference on the phone.';
  });
  ($('androidGeneralNotificationDetail') as HTMLSelectElement).addEventListener('change',event => {
    const notificationDetail = (event.currentTarget as HTMLSelectElement).value;
    if (notificationDetail !== 'task_finished' && notificationDetail !== 'full_message') return;
    if (!saveAndroidGeneralPreferences({...androidGeneralPreferences,notificationDetail}))
      $('androidGeneralBackupStatus').textContent = 'Could not save this preference on the phone.';
  });
  ($('androidPrivacyFullNotification') as HTMLInputElement).addEventListener('change',event => {
    const notificationDetail=(event.currentTarget as HTMLInputElement).checked
      ? 'full_message' : 'task_finished';
    if(!saveAndroidGeneralPreferences({...androidGeneralPreferences,notificationDetail}))
      $('androidGeneralBackupStatus').textContent='Could not save this privacy preference.';
  });
  showAndroidDeveloperMode(loadAndroidDeveloperMode());
  ($('androidDeveloperMode') as HTMLInputElement).addEventListener('change',event=>{
    const enabled=(event.currentTarget as HTMLInputElement).checked;
    try{localStorage.setItem('mcp-agent-ai-android-developer-v1',enabled?'on':'off');}catch{}
    showAndroidDeveloperMode(enabled);
  });
  ($('androidGeneralNotificationPriority') as HTMLSelectElement).addEventListener('change',event => {
    const notificationPriority = (event.currentTarget as HTMLSelectElement).value;
    if (notificationPriority !== 'foreground' && notificationPriority !== 'background' &&
        notificationPriority !== 'both') return;
    if (!saveAndroidGeneralPreferences({...androidGeneralPreferences,notificationPriority}))
      $('androidGeneralBackupStatus').textContent = 'Could not save this preference on the phone.';
  });
  ($('androidGeneralMessageDuringTurn') as HTMLSelectElement).addEventListener('change',event => {
    const messageDuringTurn = (event.currentTarget as HTMLSelectElement).value;
    if (messageDuringTurn !== 'steer' && messageDuringTurn !== 'queue') return;
    if (!saveAndroidGeneralPreferences({...androidGeneralPreferences,messageDuringTurn}))
      $('androidGeneralBackupStatus').textContent = 'Could not save this preference on the phone.';
  });
  $('androidGeneralOpenPermissions').addEventListener('click',() => {
    setAndroidSettingsView('setup');
    chooseAndroidCosSetupStep('access');
  });
  $('androidGeneralOpenWorkspace').addEventListener('click',() => {
    setAndroidSettingsView('workspace');
  });
  $('androidGeneralOpenHealth').addEventListener('click',() => {
    setAndroidSettingsView('workspace');
    const target = $('androidWorkspaceHealthTitle') as HTMLElement;
    target.scrollIntoView({block:'start',behavior:'auto'});
  });
  $('androidGeneralOpenReview').addEventListener('click',() => openToolPanel('review'));
  $('androidGeneralExport').addEventListener('click',() => void copyAndroidGeneralBackup());
  $('androidGeneralBackupText').addEventListener('input',() => {
    androidGeneralImportPending = null;
    ($('androidGeneralBackupApply') as HTMLButtonElement).disabled = true;
    $('androidGeneralBackupStatus').textContent = '';
  });
  $('androidGeneralBackupPreview').addEventListener('click',previewAndroidGeneralBackup);
  $('androidGeneralBackupApply').addEventListener('click',() => {
    const pending = androidGeneralImportPending;
    const current = ($('androidGeneralBackupText') as HTMLTextAreaElement).value.trim();
    if (!pending || current !== pending.source) {
      $('androidGeneralBackupStatus').textContent = 'Backup changed. Preview it again.';
      return;
    }
    androidGeneralImportPending = null;
    ($('androidGeneralBackupApply') as HTMLButtonElement).disabled = true;
    $('androidGeneralBackupStatus').textContent = saveAndroidGeneralPreferences(pending.preferences)
      ? 'General preferences imported and applied.'
      : 'Import was not saved. Your current settings remain unchanged.';
  });
}
// As on COS Windows, Setup and Workspace borrow the exact same control
// surfaces. The Android native policy and saved shared-folder receipt are the
// only authorities; moving DOM nodes cannot create a second editable copy.
const androidWorkspaceSharedSurfaces = nativeAndroidRuntime
  ? ([
      ['androidCosAccessPanel','androidWorkspacePermissionsSlot'],
      ['androidCosFolderPanel','androidWorkspaceFoldersSlot']
    ] as const).map(([id,slot]) => {
      const node = $(id);
      const anchor = document.createComment(`Setup home for ${id}`);
      node.before(anchor);
      return {node,anchor,slot};
    })
  : [];
function androidWorkspaceVisible():boolean {
  return nativeAndroidRuntime && $('settingsPanel').dataset.settingsView === 'workspace';
}
function placeAndroidWorkspaceSurfaces(view:string):void {
  if (!nativeAndroidRuntime) return;
  for (const {node,anchor,slot} of androidWorkspaceSharedSurfaces) {
    const container = view === 'workspace' ? $(slot) : anchor.parentNode;
    if (!container) continue;
    if (view === 'workspace') {
      if (node.parentNode !== container) container.appendChild(node);
    } else if (node.parentNode !== container || node.previousSibling !== anchor) {
      container.insertBefore(node,anchor.nextSibling);
    }
  }
}
let androidWorkspaceChecksBusy = false;
let androidWorkspaceChecksShown = false;
let androidWorkspaceActivitySignature = '';
function androidWorkspaceAge(when:number|null|undefined):string {
  if (!Number.isSafeInteger(when) || !when || when > Date.now() + 60_000) return '—';
  const seconds = Math.max(0,Math.floor((Date.now() - when) / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ${Math.floor(seconds % 3600 / 60)}m`;
  return `${Math.floor(seconds / 86400)}d ${Math.floor(seconds % 86400 / 3600)}h`;
}
function androidWorkspaceDuration(seconds:number|null|undefined):string {
  if (!Number.isSafeInteger(seconds) || seconds === null || seconds === undefined || seconds < 0)
    return '—';
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ${Math.floor(seconds % 3600 / 60)}m`;
  return `${Math.floor(seconds / 86400)}d ${Math.floor(seconds % 86400 / 3600)}h`;
}
function androidWorkspaceActivityText(events:readonly AndroidTunnelEvent[]):string {
  return events.map(event =>
    `${new Date(event.timestampMs).toLocaleTimeString('en-US',
      {hour:'numeric',minute:'2-digit',second:'2-digit'})}  ${event.kind}  ${event.detail}`).join('\n');
}
function renderAndroidWorkspace():void {
  if (!nativeAndroidRuntime) return;
  const policy = androidMcpAccessSnapshot;
  const readOnly = !!policy && policy.preset === 'read_only' &&
    androidMcpReadCapabilities.every(key => policy.requested[key]) &&
    androidMcpWriteCapabilities.every(key => !policy.requested[key]) &&
    !policy.requested.browserControl && !policy.requested.terminal;
  const readButton = $('androidWorkspaceReadOnly') as HTMLButtonElement;
  readButton.disabled = !isTauri() || !policy || androidMcpAccessBusy || readOnly;
  readButton.dataset.active = String(readOnly);
  readButton.setAttribute('aria-pressed',String(readOnly));
  readButton.textContent = androidMcpAccessBusy ? 'Saving…' : 'Read only';
  const choose = $('androidWorkspaceChooseFolder') as HTMLButtonElement;
  choose.disabled = !isTauri() || !androidMainStorageVerified() ||
    androidMainFolderBusy || androidMainSelectionBusy;
  const tunnel = androidOpenAiTunnelSnapshot;
  const tunnelLive = tunnel?.pollActive === true &&
    ['polling','tool_verified'].includes(tunnel.state);
  const pluginLinked = androidCorePluginVerified();
  const health = tunnel?.health;
  const setStatus = (id:string,state:string,value:string):void => {
    const node = $(id);
    node.dataset.state = state;
    node.textContent = value;
  };
  setStatus('androidWorkspaceTunnelStatus',tunnelLive ? 'verified' :
    !tunnel || tunnel.state === 'connecting' ? 'checking' :
    tunnel.state === 'error' ? 'error' : 'pending',
    tunnelLive ? 'ok · polling' : !tunnel || tunnel.state === 'connecting'
      ? 'Checking…' : tunnel.state === 'error' ? 'interrupted' : 'not running');
  setStatus('androidWorkspacePluginStatus',pluginLinked ? 'verified' :
    !tunnel || tunnel.state === 'connecting' ? 'checking' : 'pending',
    pluginLinked ? 'linked' : !tunnel || tunnel.state === 'connecting'
      ? 'Checking…' : 'not verified');
  const metrics:[string,number|null|undefined][] = [
    ['androidWorkspaceVerifiedLink',health?.verifiedLinkAtMs],
    ['androidWorkspaceLastCall',health?.lastChatgptCallAtMs]
  ];
  for (const [id,value] of metrics) {
    const node = $(id);
    const text = androidWorkspaceAge(value);
    node.textContent = text;
    node.dataset.state = text === '—' ? 'unknown' : 'known';
  }
  const fact = (id:string,value:string,state?:string):void => {
    const node = $(id);
    node.textContent = value;
    node.setAttribute('title',value);
    if (state) node.dataset.state = state;
    else delete node.dataset.state;
  };
  fact('androidWorkspaceRoute',health?.route ?? (tunnel ? '—' : 'Checking…'));
  fact('androidWorkspacePollErrors',health ? String(health.pollErrors) : '—',
    health?.pollErrors ? 'error' : undefined);
  fact('androidWorkspaceUptime',androidWorkspaceDuration(health?.uptimeSeconds));
  fact('androidWorkspaceLastTool',androidWorkspaceAge(health?.lastChatgptCallAtMs));
  fact('androidWorkspaceTunnelClient',health?.tunnelClient || '—');
  fact('androidWorkspaceLocalServer',health?.localServer || '—');
  fact('androidWorkspaceToolCount',typeof health?.toolCount === 'number'
    ? String(health.toolCount) : '—');
  const events = tunnel?.events || [];
  const signature = JSON.stringify(events);
  if (signature !== androidWorkspaceActivitySignature) {
    const feed = $('androidWorkspaceActivityFeed');
    const scroll = feed.scrollTop;
    if (!events.length) {
      const empty = document.createElement('p');
      empty.id = 'androidWorkspaceActivityStatus';
      empty.className = 'android-workspace-feed-empty';
      empty.textContent = tunnel
        ? 'No tunnel events recorded in this app session.'
        : 'Checking native activity…';
      feed.replaceChildren(empty);
    } else {
      feed.replaceChildren(...events.map(event => {
        const row = document.createElement('div');
        row.className = 'android-workspace-activity-row';
        row.dataset.kind = event.kind;
        const time = document.createElement('time');
        time.dateTime = new Date(event.timestampMs).toISOString();
        time.textContent = new Date(event.timestampMs).toLocaleTimeString('en-US', {
          hour:'numeric',minute:'2-digit',second:'2-digit'
        });
        const kind = document.createElement('strong');
        kind.className = 'android-workspace-activity-kind';
        kind.textContent = event.kind;
        const detail = document.createElement('span');
        detail.className = 'android-workspace-activity-detail';
        detail.textContent = event.detail;
        row.append(time,kind,detail);
        return row;
      }));
    }
    feed.scrollTop = scroll;
    androidWorkspaceActivitySignature = signature;
  }
  ($('androidWorkspaceCopyActivity') as HTMLButtonElement).disabled = events.length === 0;
  const checks = $('androidWorkspaceChecksResult');
  checks.hidden = !androidWorkspaceChecksShown;
  if (androidWorkspaceChecksShown) {
    $('androidWorkspaceChecksSummary').textContent = androidWorkspaceChecksBusy
      ? 'Running native checks…'
      : 'Native checks completed';
    const lines = [
      `Tunnel: ${tunnelLive ? 'polling successfully' : tunnel?.state ?? 'unknown'}`,
      `Core Plugin: ${pluginLinked ? 'linked to this live tunnel' : 'no current verification'}`,
      `MCP access: ${policy ? 'native policy loaded' : 'unavailable'}`,
      `Folder: ${androidMainStorageVerified() ? 'storage granted' : 'not verified'}`
    ];
    $('androidWorkspaceChecksList').replaceChildren(...lines.map(line => {
      const item = document.createElement('li');
      item.textContent = line;
      return item;
    }));
  }
  const check = $('androidWorkspaceRunChecks') as HTMLButtonElement;
  check.disabled = androidWorkspaceChecksBusy || !isTauri();
  $('androidWorkspaceRunChecksLabel').textContent = androidWorkspaceChecksBusy
    ? 'Checking…' : 'Run checks';
}
async function refreshAndroidWorkspace():Promise<void> {
  if (!nativeAndroidRuntime || !isTauri() || androidWorkspaceChecksBusy) return;
  androidWorkspaceChecksBusy = true;
  renderAndroidWorkspace();
  try {
    await Promise.allSettled([
      refreshAndroidMcpAccess(),
      refreshAndroidMainStorageStatus(true),
      refreshAndroidOpenAiTunnel(true),
      refreshAndroidTunnelSavedId(),
      refreshAndroidTunnelCredentials()
    ]);
    // Selection is read after the main storage receipt, so a revoked Android
    // file permission cannot leave an old folder shown as approved.
    await refreshAndroidMainSelection(true);
    await refreshAndroidTunnelStoredAccess();
    await refreshAndroidOpenAiTunnel(true);
  } finally {
    androidWorkspaceChecksBusy = false;
    renderAndroidCosSetup();
  }
}

// Plugins modules share the single Core Tunnel. Native module receipts remain
// separate: a healthy Core poll alone does not verify a Plugins tool call.
type AndroidPluginDescriptor = {
  id:string; name:string; description:string;
  available:boolean; enabled:boolean; ready:boolean; tools:number;
  installed?:boolean;
  installation?:'bundled_with_app'|'external_unavailable';
  updateSupported?:boolean;
  category?:'automation'|'security'|'development'|'integration'|'productivity';
  risk?:'read_only'|'approval_required';
  version?:string;
  sourceLabel?:string;
  permissions?:string[];
  toolNames?:string[];
};
type AndroidPluginsStatus = {
  state:'disconnected'|'connecting'|'polling'|'tool_verified'|'error';
  pollActive:boolean; toolCallsVerified:number; desiredConnected:boolean;
  lastError?:string;
  plugins:AndroidPluginDescriptor[]; availableTools:number;
};
let androidPluginsStatus:AndroidPluginsStatus|null = null;
let androidPluginsCatalog:AndroidPluginDescriptor[] = [];
let androidPluginsBusy = false;
let androidPluginsGeneration = 0;
let androidPluginsRefreshInFlight = false;
let androidPluginsChatOpenBusy = false;
let androidPluginsChatOpenMessage = '';
let androidPluginsModuleMessage = '';
let androidPluginsSearchValue = '';
let androidPluginsCategoryValue = 'all';
const androidPluginsCategories = new Set(['automation','security','development','integration','productivity']);
function androidPluginCategory(item:AndroidPluginDescriptor):NonNullable<AndroidPluginDescriptor['category']> {
  if (item.category) return item.category;
  return item.id==='browser'||item.id==='app_control' ? 'automation' :
    item.id==='reverse_engineering' ? 'development' :
    item.id==='security_audit' ? 'security' :
    item.id==='web_fetch' ? 'integration' : 'productivity';
}
function androidPluginsDescriptor(value:unknown):AndroidPluginDescriptor|null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  const item = value as Record<string,unknown>;
  if (typeof item.id !== 'string' || !/^[a-z0-9_-]{1,64}$/.test(item.id) ||
      typeof item.name !== 'string' || item.name.length < 1 || item.name.length > 100 ||
      typeof item.available !== 'boolean' || typeof item.enabled !== 'boolean' ||
      typeof item.ready !== 'boolean' || typeof item.tools !== 'number' ||
      !Number.isInteger(item.tools) || item.tools < 0 || item.tools > 1000) return null;
  const category = typeof item.category==='string' && androidPluginsCategories.has(item.category)
    ? item.category as AndroidPluginDescriptor['category'] : undefined;
  const risk=item.risk==='read_only'||item.risk==='approval_required'
    ? item.risk : undefined;
  const version=typeof item.version==='string' && /^[a-zA-Z0-9._+-]{1,28}$/.test(item.version)
    ? item.version : undefined;
  const sourceLabel=typeof item.sourceLabel==='string' && item.sourceLabel.length<=60
    ? item.sourceLabel : undefined;
  const permissions=Array.isArray(item.permissions)&&item.permissions.length<=8 &&
    item.permissions.every(x=>typeof x==='string'&&x.length<=100)
    ? item.permissions as string[] : undefined;
  const toolNames=Array.isArray(item.toolNames)&&item.toolNames.length<=24 &&
    item.toolNames.every(x=>typeof x==='string'&&/^[a-z0-9_/-]{1,64}$/.test(x))
    ? item.toolNames as string[] : undefined;
  return {
    id:item.id, name:item.name, description:typeof item.description === 'string'
      ? item.description.slice(0,300) : '',
    available:item.available, enabled:item.enabled, ready:item.ready, tools:item.tools,
    installed:typeof item.installed==='boolean'?item.installed:undefined,
    installation:item.installation==='bundled_with_app'||item.installation==='external_unavailable'
      ? item.installation:undefined,
    updateSupported:item.updateSupported===true,
    category,risk,version,sourceLabel,permissions,toolNames
  };
}
function androidPluginsList(raw:unknown):AndroidPluginDescriptor[] {
  if (!Array.isArray(raw) || raw.length > 32) throw Error('Invalid Plugins catalog');
  const result = raw.map(androidPluginsDescriptor);
  if (result.some(item => !item)) throw Error('Invalid Plugins descriptor');
  const plugins = result as AndroidPluginDescriptor[];
  if (new Set(plugins.map(item => item.id)).size !== plugins.length)
    throw Error('Duplicate Plugins ID');
  return plugins;
}
function androidPluginsReceipt(raw:unknown):AndroidPluginsStatus {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw))
    throw Error('Invalid Plugins status');
  const data = raw as Record<string,unknown>;
  if (!['disconnected','connecting','polling','tool_verified','error'].includes(String(data.state)) ||
      typeof data.pollActive !== 'boolean' ||
      typeof data.desiredConnected !== 'boolean' ||
      typeof data.toolCallsVerified !== 'number' ||
      !Number.isInteger(data.toolCallsVerified) || data.toolCallsVerified < 0 ||
      typeof data.availableTools !== 'number' || !Number.isInteger(data.availableTools) ||
      data.availableTools < 0 || data.availableTools > 1000 ||
      (data.lastError !== undefined && data.lastError !== null && typeof data.lastError !== 'string'))
    throw Error('Invalid Plugins status');
  return {
    state:data.state as AndroidPluginsStatus['state'],
    pollActive:data.pollActive, desiredConnected:data.desiredConnected,
    toolCallsVerified:data.toolCallsVerified, availableTools:data.availableTools,
    lastError:typeof data.lastError === 'string' ? data.lastError.slice(0,120) : undefined,
    plugins:androidPluginsList(data.plugins)
  };
}
function androidPluginsVerified():boolean {
  return androidPluginsStatus?.state === 'tool_verified' &&
    androidPluginsStatus.toolCallsVerified > 0 && androidPluginsStatus.pollActive;
}
function androidPluginsShowing():boolean {
  return nativeAndroidRuntime && !$('settingsPanel').classList.contains('hidden') &&
    $('settingsPanel').dataset.settingsView === 'plugins';
}
function renderAndroidPlugins():void {
  if (!nativeAndroidRuntime) return;
  const snapshot = androidPluginsStatus;
  const core = androidOpenAiTunnelSnapshot;
  const coreOnline = core?.pollActive === true &&
    (core.state === 'polling' || core.state === 'tool_verified');
  const verified = coreOnline && androidPluginsVerified();
  const stateLabel = verified ? 'Tools working' : coreOnline ? 'Core connected' :
    core?.state === 'connecting' ? 'Connecting…' :
    core?.state === 'error' ? 'Core error' : core ? 'Not connected' : 'Status unavailable';
  const state = $('androidPluginsState');
  state.textContent = stateLabel;
  state.dataset.state = verified ? 'tool_verified' :
    coreOnline ? 'polling' : core?.state || 'disconnected';
  const coreStatus = $('androidPluginsCoreStatus');
  coreStatus.dataset.state = coreOnline ? 'connected' :
    core?.state === 'error' ? 'error' : 'unverified';
  coreStatus.textContent = androidOpenAiTunnelVerified()
    ? `Core Tunnel verified · ${core!.toolCallsVerified} successful tool call${core!.toolCallsVerified === 1 ? '' : 's'}.`
    : coreOnline ? 'Core Tunnel online · Waiting for a verified ChatGPT tool call.' :
      core?.state === 'connecting' ? 'Core Tunnel connecting…' :
      core?.state === 'error' ? 'Core Tunnel interrupted. Check connection in Setup.' :
      core ? 'Core Tunnel disconnected. Connect once in Setup.' :
      'Unable to verify Core Tunnel status. Check Setup.';
  const chatState = $('androidPluginsChatStatus');
  chatState.dataset.state = verified ? 'connected' :
    core?.state === 'error' ? 'error' : 'unverified';
  chatState.textContent = verified ? 'Core connector active · Optional Plugins tool call verified through Core.' :
    (androidPluginsChatOpenMessage ||
      (coreOnline && snapshot ? 'Core Tunnel online · Optional Plugins tools await a verified call through Core.' :
      !core ? 'Unable to check Core Tunnel connection.' :
      core?.state === 'connecting' ? 'Core Tunnel connecting…' :
      core?.state === 'error' ? 'Core Tunnel connection error. Check Setup.' :
      coreOnline ? 'Optional modules status unavailable · Core Tunnel is online.' :
      'Core connector not verified · Connect Core Tunnel in Setup; no second connector required.'));
  ($('androidPluginsOpenChatGPT') as HTMLButtonElement).disabled =
    androidPluginsChatOpenBusy || !isTauri();
  $('androidPluginsToolCount').textContent = `${snapshot?.availableTools ?? 0} available tools`;
  $('androidPluginsModulesFeedback').textContent = androidPluginsModuleMessage;
  const list = $('androidPluginsModules');
  list.replaceChildren();
  const known = new Map(androidPluginsCatalog.map(item => [item.id,item]));
  for (const item of snapshot?.plugins || []) known.set(item.id,item);
  const all=[...known.values()];
  const activeCount=all.filter(item=>snapshot?.plugins.some(receipt=>
    receipt.id===item.id && receipt.available && receipt.enabled)).length;
  const availableCount=all.filter(item=>snapshot?.plugins.some(receipt=>
    receipt.id===item.id && receipt.available)).length;
  $('androidPluginsMarketplaceSummary').textContent=
    activeCount+' enabled · '+availableCount+' available';
  if (!known.size) {
    list.append(el('p','','No native Plugins modules reported by the backend.'));
    return;
  }
  const filtered=all.filter(item=>{
    const category=androidPluginCategory(item);
    if(androidPluginsCategoryValue!=='all'&&androidPluginsCategoryValue!==category)return false;
    const search=androidPluginsSearchValue.toLocaleLowerCase();
    if(!search)return true;
    return [item.name,item.id,item.description,category,
      ...(item.permissions||[]),...(item.toolNames||[])].join(' ').toLocaleLowerCase().includes(search);
  });
  if(!filtered.length){
    list.append(el('p','android-plugins-empty',
      'No plugins match that search and category. Clear the search or choose All categories.'));
    return;
  }
  for (const item of filtered) {
    const receipt = snapshot?.plugins.find(entry => entry.id === item.id);
    const installed = receipt?.installed ?? item.installed ?? true;
    const bundled = receipt?.installation==='bundled_with_app' ||
      item.installation==='bundled_with_app';
    const available = item.available && !!receipt?.available;
    const enabled = installed && available && receipt?.enabled === true;
    const ready = verified && available && enabled && receipt?.ready === true && receipt.tools > 0;
    const label = !installed ? 'Not installed' : !available ? 'Unavailable' : !enabled ? 'Off' : ready
      ? 'Ready' : verified ? 'Not ready' : 'Waiting for ChatGPT';
    const card = el('article','android-plugins-module');
    card.dataset.pluginId=item.id;
    const category=androidPluginCategory(item);
    card.dataset.category=category;
    const title = el('div','android-plugins-module-text');
    const displayName = item.id === 'browser' ? 'Websites (private browser)' :
      item.id === 'app_control' ? 'Open Android apps' : item.name;
    const description = item.id === 'browser' ?
      'ChatGPT can open, read and interact with web pages. No Chrome cookies.' :
      item.id === 'app_control' ?
      'ChatGPT can open a permitted app and stop its session. Not full app control.' :
      item.description || 'MCP tool module';
    title.append(el('strong','',displayName),
      el('small','',description),
      el('span','android-plugins-module-meta',`${label} · ${receipt?.tools ?? 0} tools`));
    const tags=el('div','android-plugins-module-tags');
    const categoryLabel={
      automation:'Automation',security:'Security',development:'Development',
      integration:'Integrations',productivity:'Productivity'
    }[category];
    const categoryTag=el('span','',categoryLabel);
    categoryTag.dataset.category=category;
    tags.append(categoryTag);
    tags.append(el('span','',item.sourceLabel||'Bundled Android'));
    const risk=item.risk||(category==='automation'?'approval_required':'read_only');
    const riskBadge=el('span','',risk==='read_only'?'Read-only tools':'Action approval required');
    riskBadge.dataset.risk=risk;
    tags.append(riskBadge);
    if(item.version)tags.append(el('span','','v'+item.version));
    title.append(tags);
    const actions = el('div','android-plugins-module-actions');
    const control = el('button','outline android-plugins-toggle', enabled ? 'Disable' : 'Enable');
    control.type = 'button';
    control.disabled = androidPluginsBusy || !isTauri() || !available || !installed;
    control.setAttribute('aria-label',`${enabled ? 'Disable' : 'Enable'} ${item.name}`);
    control.addEventListener('click', () => void toggleAndroidPlugin(item.id,!enabled));
    if(installed) actions.append(control);
    if(bundled){
      const manage=el('button','outline android-plugins-install',installed?'Uninstall':'Install');
      manage.type='button';
      manage.disabled=androidPluginsBusy||!isTauri();
      manage.setAttribute('aria-label',`${installed?'Uninstall':'Install'} bundled ${item.name}`);
      manage.addEventListener('click',()=>void changeAndroidPluginInstallation(item.id,!installed,item.name));
      actions.append(manage);
      const update=el('button','outline android-plugins-check-update','Check app update');
      update.type='button';
      update.disabled=androidPluginsBusy||!isTauri();
      update.setAttribute('aria-label',`Check official update for ${item.name}`);
      update.addEventListener('click',()=>void checkBundledAndroidPluginUpdate(item.id,item.name));
      actions.append(update);
      actions.append(el('small','android-plugins-update-note',
        'Code is bundled in this APK; security updates arrive with a newer signed app version.'));
    }
    if(!actions.children.length){
      actions.append(el('small','android-plugins-update-note',
        'External plugin installation is unavailable until package verification is supported.'));
    }
    card.append(title,actions);
    const details=el('details','android-plugins-module-details');
    details.append(el('summary','','Capabilities & permissions'));
    const permissionText=item.permissions?.length
      ? item.permissions.join(' · ') : category==='automation'
        ? 'Requires separate Android runtime consent and app-session checks'
        : risk==='read_only'
          ? 'Bounded read-only inputs, no local filesystem or shell access'
          : 'Native permission required before using actions';
    details.append(el('p','',permissionText));
    if(item.toolNames?.length) {
      const names=el('ul','android-plugins-module-tools');
      for(const name of item.toolNames.slice(0,16))names.append(el('li','',name));
      details.append(names);
    } else {
      details.append(el('p','','Tool names appear after native catalog verification.'));
    }
    card.append(details);
    list.append(card);
  }
}
async function refreshAndroidPlugins(includeCatalog = false):Promise<void> {
  if (!nativeAndroidRuntime || !isTauri() || androidPluginsBusy ||
      androidPluginsRefreshInFlight) return;
  androidPluginsRefreshInFlight = true;
  const generation = ++androidPluginsGeneration;
  try {
    const [status,catalog] = await Promise.all([
      invoke<unknown>('android_plugins_status'),
      includeCatalog ? invoke<unknown>('android_plugins_catalog') : Promise.resolve(null),
      refreshAndroidOpenAiTunnel()
    ]);
    const parsed = androidPluginsReceipt(status);
    const listed = includeCatalog ? androidPluginsList((catalog as {plugins?:unknown})?.plugins) : null;
    if (generation !== androidPluginsGeneration) return;
    androidPluginsStatus = parsed;
    if (listed) androidPluginsCatalog = listed;
  } catch {
    if (generation !== androidPluginsGeneration) return;
    androidPluginsStatus = null;
  } finally {
    androidPluginsRefreshInFlight = false;
    renderAndroidPlugins();
  }
}
async function toggleAndroidPlugin(pluginId:string,enabled:boolean):Promise<void> {
  const current = androidPluginsStatus?.plugins.find(item => item.id === pluginId);
  if (!current?.available || androidPluginsBusy || !isTauri()) return;
  androidPluginsBusy = true;
  ++androidPluginsGeneration;
  androidPluginsStatus = null;
  androidPluginsModuleMessage = 'Applying native module preference…';
  renderAndroidPlugins();
  try {
    androidPluginsStatus = androidPluginsReceipt(await invoke<unknown>(
      'android_plugins_set_enabled',{pluginId,enabled,userInitiated:true}));
    const changed = androidPluginsStatus.plugins.find(item => item.id === pluginId);
    if (!changed || changed.enabled !== enabled)
      throw Error('Module preference was not confirmed');
    androidPluginsModuleMessage = `${changed.name} ${enabled ? 'enabled' : 'disabled'} by Android.`;
  } catch {
    androidPluginsStatus = null;
    androidPluginsModuleMessage = 'Android did not confirm this module change.';
  } finally {
    androidPluginsBusy = false;
    renderAndroidPlugins();
    void refreshAndroidPlugins(true);
  }
}
async function changeAndroidPluginInstallation(pluginId:string,install:boolean,name:string):Promise<void>{
  if(!nativeAndroidRuntime||!isTauri()||androidPluginsBusy)return;
  const original=androidPluginsStatus?.plugins.find(plugin=>plugin.id===pluginId);
  const bundled=original?.installation==='bundled_with_app';
  if(!bundled||original?.installed===install)return;
  const action=install?'Install':'Uninstall';
  if(!window.confirm(install
    ? `Install bundled ${name} into the Android tool catalog? It will stay disabled until you enable it.`
    : `Uninstall ${name} from the Android tool catalog? Its published tool aliases will be revoked; the compiled code remains in this APK.`))return;
  androidPluginsBusy=true;
  ++androidPluginsGeneration;
  androidPluginsModuleMessage=`${action}ing ${name}…`;
  renderAndroidPlugins();
  try{
    const cmd=install?'android_plugins_install_bundled':'android_plugins_uninstall_bundled';
    const next=androidPluginsReceipt(await invoke<unknown>(cmd,{pluginId,userInitiated:true}));
    const changed=next.plugins.find(plugin=>plugin.id===pluginId);
    if(!changed||changed.installed!==install)
      throw Error('Installation state was not verified');
    androidPluginsStatus=next;
    androidPluginsModuleMessage=`${name}: ${install?'installed, disabled by default':'uninstalled and aliases revoked'}.`;
  }catch(error){
    androidPluginsModuleMessage=`${action} not verified: ${String(error).slice(0,120)}`;
  }finally{
    androidPluginsBusy=false;
    await refreshAndroidPlugins(true);
    renderAndroidPlugins();
  }
}
async function checkBundledAndroidPluginUpdate(pluginId:string,name:string):Promise<void>{
  if(!nativeAndroidRuntime||!isTauri()||androidPluginsBusy)return;
  const item=androidPluginsStatus?.plugins.find(plugin=>plugin.id===pluginId);
  if(!item?.installed||item.installation!=='bundled_with_app')return;
  androidPluginsBusy=true;
  androidPluginsModuleMessage=`Checking the official signed-app release for ${name}…`;
  renderAndroidPlugins();
  try{
    const receipt=await invoke<{pluginId:string;checkSucceeded:boolean;
      updateAvailable:boolean;version?:string|null;url?:string|null;
      reason?:string;automaticInstallSupported:boolean;signingCompatibilityVerified:boolean}>(
      'android_plugins_check_update',{pluginId,userInitiated:true});
    if(receipt.pluginId!==pluginId||typeof receipt.checkSucceeded!=='boolean'||
        receipt.automaticInstallSupported!==false||receipt.signingCompatibilityVerified!==false)
      throw Error('Native update metadata receipt was not verified');
    if(!receipt.checkSucceeded){
      androidPluginsModuleMessage=`${name}: official update check unavailable (${String(receipt.reason||'no release').slice(0,70)}).`;
    }else if(receipt.updateAvailable===true && typeof receipt.version==='string'){
      const url=typeof receipt.url==='string'?receipt.url:'';
      const trusted=/^https:\/\/github\.com\/airesz178\/Mcp-Agent-Ai\/releases\/tag\/[A-Za-z0-9._-]{1,80}$/.test(url);
      androidPluginsModuleMessage=`${name}: app release ${receipt.version} may be newer. `+
        (trusted?`Release page: ${url}. `:'')+
        'Android must verify APK signature compatibility before updating; no installation was started.';
    }else{
      androidPluginsModuleMessage=`${name}: no newer stable signed-app release reported by GitHub. No update installed.`;
    }
  }catch(error){
    androidPluginsModuleMessage=`Official update check not verified: ${String(error).slice(0,110)}`;
  }finally{
    androidPluginsBusy=false;
    renderAndroidPlugins();
  }
}

if (nativeAndroidRuntime) {
  $('androidPluginsSearch').addEventListener('input',()=>{
    androidPluginsSearchValue=($('androidPluginsSearch') as HTMLInputElement).value.trim().slice(0,70);
    renderAndroidPlugins();
  });
  $('androidPluginsCategory').addEventListener('change',()=>{
    const value=($('androidPluginsCategory') as HTMLSelectElement).value;
    androidPluginsCategoryValue=value==='all'||androidPluginsCategories.has(value)?value:'all';
    renderAndroidPlugins();
  });
  $('androidPluginsOpenChatGPT').addEventListener('click',async () => {
    if (!isTauri() || androidPluginsChatOpenBusy) return;
    androidPluginsChatOpenBusy = true;
    androidPluginsChatOpenMessage = '';
    renderAndroidPlugins();
    try {
      const receipt = await invoke<{opened:boolean;kind:string}>(
        'android_tunnel_open_platform_page',
        {kind:'chatgpt_plugins',userInitiated:true});
      if (receipt?.opened !== true || receipt.kind !== 'chatgpt_plugins')
        throw Error('ChatGPT connectors page not confirmed');
      // Opening a webpage does not prove the existing Core connector is linked.
      androidPluginsChatOpenMessage = '';
    } catch {
      androidPluginsChatOpenMessage = 'Could not open ChatGPT connector settings.';
    } finally {
      androidPluginsChatOpenBusy = false;
      renderAndroidPlugins();
      // The same native status receipt is checked again when the user returns.
      void refreshAndroidPlugins(true);
    }
  });
  for (const [buttonId,inputId,label] of [
    ['androidPluginsCopyName','androidPluginsChatName','Name'],
    ['androidPluginsCopyDescription','androidPluginsChatDescription','Description']
  ]) {
    $(buttonId).addEventListener('click', async () => {
      const field = $(inputId) as HTMLInputElement|HTMLTextAreaElement;
      try {
        if (navigator.clipboard?.writeText) await navigator.clipboard.writeText(field.value);
        else {
          field.focus(); field.select();
          if (!document.execCommand('copy')) throw Error('Clipboard unavailable');
        }
        $('androidPluginsCopyFeedback').textContent = `${label} copied.`;
      } catch {
        $('androidPluginsCopyFeedback').textContent = 'Clipboard unavailable. Select the text to copy manually.';
      }
    });
  }
  document.addEventListener('visibilitychange',() => {
    if (!document.hidden && androidPluginsShowing()) void refreshAndroidPlugins(true);
  });
  window.setInterval(() => {
    if (!document.hidden && androidPluginsShowing()) void refreshAndroidPlugins();
  },4000);
  renderAndroidPlugins();
}
// Setup > Access > Fine-tune permissions > Automation modes owns these
// preferences. The dropdowns are NOT MCP capability grants or app-session
// authorization. Native Android preferences alone determine the selected
// runtime strategy, with OS capability checks applied independently.
type AutomationModes = {
  version:1; androidMode:'realtime'|'screenshot_click';
  browserMode:'headless'|'right_sidebar'|'open_browser_app'; saved:true;
};
type BrowserEngineHealth = {
  supported?:boolean; running?:boolean; ready?:boolean;
  opened?:boolean; profileIsolationSupported?:boolean;
  available?:boolean; canLaunch?:boolean; reason?:string;
};
let automationModes:AutomationModes|null = null;
let automationLoading = false;
let automationSaving = false;
let automationMessage = '';
let automationHeadless:BrowserEngineHealth|null = null;
let automationInApp:BrowserEngineHealth|null = null;
let automationChrome:BrowserEngineHealth|null = null;
let automationAppProbe:Record<string,unknown>|null = null;

function checkedAutomationModes(value:unknown):AutomationModes {
  const input = value as AutomationModes;
  if (!input || input.version !== 1 || input.saved !== true ||
      !['realtime','screenshot_click'].includes(input.androidMode) ||
      !['headless','right_sidebar','open_browser_app'].includes(input.browserMode))
    throw Error('Native Android automation settings are not verified');
  return input;
}
function androidAutomationShowing():boolean {
  if (!nativeAndroidRuntime || $('settingsPanel').classList.contains('hidden') ||
      $('settingsPanel').dataset.settingsView !== 'setup' ||
      androidCosSetupSelected !== 'tools' ||
      $('androidCosAccessPanel').classList.contains('hidden')) return false;
  const modes = document.getElementById('androidAutomationModesGroup') as HTMLDetailsElement|null;
  const fineTune = modes?.closest<HTMLDetailsElement>('.android-mcp-fine-tune');
  return modes?.open === true && fineTune?.open === true;
}
function automationBrowserSupported():boolean {
  switch (automationModes?.browserMode) {
    case 'headless': return automationHeadless?.supported === true;
    case 'right_sidebar': return automationInApp?.profileIsolationSupported === true;
    case 'open_browser_app': return automationChrome?.canLaunch === true &&
      automationChrome?.available === true;
    default: return false;
  }
}
function renderAndroidAutomationSettings():void {
  if (!nativeAndroidRuntime) return;
  const appSelect = $('androidAutomationAppMode') as HTMLSelectElement;
  const browserSelect = $('androidAutomationBrowserMode') as HTMLSelectElement;
  if (automationModes && !automationSaving) {
    appSelect.value = automationModes.androidMode;
    browserSelect.value = automationModes.browserMode;
  }
  appSelect.disabled = browserSelect.disabled = automationLoading ||
    automationSaving || !isTauri() || !automationModes;
  const mode = automationModes?.browserMode;
  $('androidAutomationBrowserDescription').textContent = mode === 'right_sidebar'
    ? 'The agent uses the isolated full-screen in-app browser view when a chat task needs it. No website is opened by changing this preference.'
    : mode === 'open_browser_app'
      ? 'The agent may launch Google Chrome when a chat task requires it. Chrome tabs and cookies cannot be read or controlled by this agent.'
      : 'The agent uses an isolated headless WebView when a chat task needs a website. No Chrome cookies.';
  const browserBadge = $('androidAutomationBrowserBadge');
  const browserSupported = automationBrowserSupported();
  browserBadge.dataset.state = browserSupported ? 'ready' : 'unavailable';
  browserBadge.textContent = !automationModes || automationLoading ? 'Checking…' :
    browserSupported && mode === 'open_browser_app' ? 'Launch only' :
    browserSupported ? 'Available' : 'Unavailable';
  const appBadge = $('androidAutomationAppBadge');
  const appMode = automationModes?.androidMode;
  const appProbe = automationAppProbe;
  const appCapability = appProbe != null && appProbe.supported === true &&
    appProbe.enabled === true && appProbe.connected === true &&
    (appMode === 'realtime' ? appProbe.realtimeAvailable === true ||
      appProbe.screenshotClickAvailable === true :
      appProbe.screenshotClickAvailable === true);
  const appUsable = appProbe != null && appProbe.effectiveMode === appMode &&
    (appMode === 'realtime' ? appProbe.realtimeAvailable === true :
      appProbe.screenshotClickAvailable === true);
  const appFallbackReady = appMode === 'realtime' && appProbe?.effectiveMode ===
    'screenshot_click' && appProbe.fallbackUsed === true &&
    appProbe.screenshotClickAvailable === true;
  appBadge.dataset.state = appCapability ? 'ready' : 'unavailable';
  appBadge.textContent = !automationModes || automationLoading ? 'Checking…' :
    appUsable || appFallbackReady ? 'Available' :
    appCapability ? 'Needs app session' : 'Unavailable';
  $('androidAutomationAppDescription').textContent = appMode === 'screenshot_click'
    ? 'Reads an approved screenshot and taps visible coordinates. Requires actual screen capture and gesture access.'
    : 'RealTime Screen stays selected. If an app cannot expose controls, the engine falls back to screenshot & click only when Android confirms both permissions. The dropdown does not change.';
  $('androidAutomationAppStatus').textContent = !automationModes ? 'Native preferences not available.' :
    !appProbe ? 'Android mode not verified. No automatic screen access.' :
    appUsable ? 'Android confirmed realtime Accessibility for the active app session. Screen actions are limited to that session.' :
    appFallbackReady ? 'RealTime Screen remains selected. Android is using screenshot & click fallback for this app session.' :
    appCapability && appProbe.reason === 'target_session_required'
      ? 'Accessibility capabilities are available. Start a third-party app session first; screen reads and actions require that active session.' :
    appCapability ? 'Android reports automation capability, but the current target is not ready. Check the foreground app session and OS permissions.' :
    'This automation method is unavailable under current Android permissions. The selected dropdown is unchanged; no unapproved fallback will run.';
  $('androidAutomationBrowserStatus').textContent = automationMessage ||
    (!automationModes ? 'Native browser preference not verified.' :
      browserSupported ? 'Preference saved on Android. Choose a website in chat when needed; this dropdown does not open any page.' :
        'Selected browser engine is currently unavailable. The saved choice is unchanged; no substitute will run automatically.');
}
async function refreshAndroidAutomationSettings(forcePreferences = false):Promise<void> {
  if (!nativeAndroidRuntime || !isTauri() || automationLoading || automationSaving) return;
  automationLoading = true;
  renderAndroidAutomationSettings();
  try {
    if (forcePreferences || !automationModes) {
      automationModes = checkedAutomationModes(
        await invoke('android_automation_preferences_status'));
    }
    const mode = automationModes.androidMode;
    const receipts = await Promise.allSettled([
      invoke<BrowserEngineHealth>('android_background_browser_status'),
      invoke<BrowserEngineHealth>('in_app_browser_status'),
      invoke<BrowserEngineHealth>('android_external_chrome_status'),
      invoke<Record<string,unknown>>('accessibility_automation_mode_status',
        {selectedMode:mode})
    ]);
    automationHeadless = receipts[0].status === 'fulfilled' ? receipts[0].value : null;
    automationInApp = receipts[1].status === 'fulfilled' ? receipts[1].value : null;
    automationChrome = receipts[2].status === 'fulfilled' ? receipts[2].value : null;
    const probe = receipts[3].status === 'fulfilled' ? receipts[3].value : null;
    automationAppProbe = probe?.selectedMode === mode &&
      typeof probe.effectiveMode === 'string' &&
      typeof probe.fallbackUsed === 'boolean' ? probe : null;
  } catch {
    automationModes = null;
    automationAppProbe = null;
    automationMessage = 'Could not verify automation settings from Android.';
  } finally {
    automationLoading = false;
    renderAndroidAutomationSettings();
  }
}
async function saveAndroidAutomationModes(
  appMode:AutomationModes['androidMode'],
  browserMode:AutomationModes['browserMode']
):Promise<void> {
  if (!nativeAndroidRuntime || !isTauri() || !automationModes ||
      automationSaving) return;
  automationSaving = true;
  automationMessage = '';
  renderAndroidAutomationSettings();
  try {
    const receipt = await invoke('android_automation_preferences_set',{
      androidMode:appMode,browserMode,userInitiated:true
    });
    automationModes = checkedAutomationModes(receipt);
  } catch (error) {
    automationMessage = String(error).includes('Stop the current owned browser')
      ? 'Stop the current owned browser before changing modes. Previous selection retained.'
      : 'Mode was not saved. Previous selection retained.';
  } finally {
    automationSaving = false;
    renderAndroidAutomationSettings();
    void refreshAndroidAutomationSettings();
  }
}
function bindAndroidAutomationSettings():void {
  if (!nativeAndroidRuntime) return;
  document.getElementById('androidAutomationOpenFineTune')?.addEventListener('click',() => {
    // The old Agents & automation Settings page is only a wayfinding link:
    // keep one pair of actual dropdowns, under Setup → Access.
    setAndroidSettingsView('setup');
    chooseAndroidCosSetupStep('tools');
    const fineTune = document.querySelector<HTMLDetailsElement>(
      '#androidMcpAccessPresets .android-mcp-fine-tune');
    const modes = document.getElementById('androidAutomationModesGroup') as HTMLDetailsElement|null;
    if (fineTune) fineTune.open = true;
    if (modes) modes.open = true;
    if (androidAutomationShowing()) void refreshAndroidAutomationSettings(true);
    window.requestAnimationFrame(() => modes?.scrollIntoView({behavior:'auto',block:'start'}));
  });
  // Read native preferences when the user expands the fine-tune/mode panels,
  // never launch a browser or target app from a Settings interaction.
  const modesGroup = document.getElementById('androidAutomationModesGroup') as HTMLDetailsElement|null;
  const fineTune = document.querySelector<HTMLDetailsElement>(
    '#androidMcpAccessPresets .android-mcp-fine-tune');
  for (const details of [modesGroup,fineTune]) {
    details?.addEventListener('toggle',() => {
      if (androidAutomationShowing()) void refreshAndroidAutomationSettings(true);
    });
  }
  $('androidAutomationAppMode').addEventListener('change',() => {
    if (!automationModes) return;
    void saveAndroidAutomationModes(
      ($('androidAutomationAppMode') as HTMLSelectElement).value as AutomationModes['androidMode'],
      automationModes.browserMode);
  });
  $('androidAutomationBrowserMode').addEventListener('change',() => {
    if (!automationModes) return;
    void saveAndroidAutomationModes(
      automationModes.androidMode,
      ($('androidAutomationBrowserMode') as HTMLSelectElement).value as AutomationModes['browserMode']);
  });
  window.setInterval(() => {
    if (!document.hidden && androidAutomationShowing())
      void refreshAndroidAutomationSettings();
  }, 5000);
}
function setAndroidSettingsView(view: string) {
  if (!nativeAndroidRuntime ||
      (view !== 'directory' && !Object.prototype.hasOwnProperty.call(androidSettingsLabels, view))) return;
  const panel = $('settingsPanel') as HTMLElement;
  // An active search field must not reopen the Android keyboard when the
  // directory is revealed or hidden during category navigation.
  if (document.activeElement === $('androidSettingsSearch')) $('androidSettingsSearch').blur();
  panel.dataset.settingsView = view;
  placeAndroidWorkspaceSurfaces(view);
  for (const group of panel.querySelectorAll<HTMLElement>('#androidSettingsContent > [data-setting-category]'))
    group.classList.toggle('android-settings-active', group.dataset.settingCategory === view &&
      !['connectionSection','backgroundChatgptSettings'].includes(group.id));
  for (const button of document.querySelectorAll<HTMLButtonElement>('[data-android-settings-category]'))
    button.setAttribute('aria-current', String(button.dataset.androidSettingsCategory === view));
  $('androidSettingsDetailTitle').textContent = androidSettingsLabels[view] || 'Settings';
  const descriptions: Record<string,string> = {
    workspace:'Choose what ChatGPT can access and check your phone connection.',
    general:'How the app behaves day to day. Changes apply immediately.',
    usage:'Available usage information for this Android build.',
    setup:'Connect your phone in 10 steps.',
    plugins:'Manage optional native tools on the shared Core tunnel.',
    agents:'Automation preferences are now in Setup → Access → Fine-tune permissions.',
    appearance:'Display options for this Android build.',
    activity:'Review activity and approved actions.'
  };
  $('androidSettingsDetailDescription').textContent = descriptions[view] || '';
  if (view === 'directory') {
    $('androidSettingsSearch').value = '';
    filterAndroidSettings();
  } else {
    if ($('androidSettingsSearch').value) {
      $('androidSettingsSearch').value = '';
      filterAndroidSettings();
    }
    setAndroidSettingsNavOpen(false);
    $('androidSettingsDetailTitle').focus({ preventScroll: true });
    if (view === 'workspace') {
      renderAndroidCosSetup();
      void refreshAndroidWorkspace();
    }
    if (view === 'setup') {
      // Native receipts are refreshed for all ten steps before choosing the
      // first unresolved stage. No tab visit is required for status checks.
      void scanAndroidCosSetupOnOpen();
    }
    if (view === 'plugins') void refreshAndroidPlugins(true);
    if (view === 'usage'||view === 'activity') renderAndroidUsageAndActivity();
  }
  $('androidSettingsContent').scrollTop = 0;
}
function setAndroidSettingsNavOpen(open: boolean) {
  if (!nativeAndroidRuntime) return;
  $('settingsPanel').dataset.settingsNavOpen = String(open);
  $('androidSettingsBack').setAttribute('aria-expanded', String(open));
  if (!$('settingsPanel').classList.contains('hidden')) {
    $('sidebarToggle').setAttribute('aria-expanded', String(open));
    $('sidebarToggle').setAttribute('aria-label', open ? 'Hide settings sidebar' : 'Show settings sidebar');
  }
  const compact = window.matchMedia('(max-width: 699px)').matches;
  // Only the foreground side of the compact drawer may take touch or focus.
  $('androidSettingsNav').inert = !open && compact;
  $('androidSettingsContent').inert = open && compact;
}
if (nativeAndroidRuntime) window.addEventListener('resize', () => {
  setAndroidSettingsNavOpen($('settingsPanel').dataset.settingsNavOpen === 'true');
});
function returnToAndroidSettingsDirectory() {
  setAndroidSettingsNavOpen(true);
  // The navigation is opened without focusing an editable Search input.
  $('settingsClose').focus({preventScroll: true});
}
if (nativeAndroidRuntime) {
  bindAndroidGeneralSettings();
  bindAndroidAutomationSettings();
  renderAndroidAppearance();
  for(const id of ['androidAppearanceTheme','androidAppearanceDensity','androidAppearanceReduceMotion'])
    $(id).addEventListener('change',updateAndroidAppearance);
  $('settingsClose').setAttribute('aria-label', 'Back to chat');
  for (const button of document.querySelectorAll<HTMLButtonElement>('[data-android-settings-category]'))
    button.addEventListener('click', () => setAndroidSettingsView(button.dataset.androidSettingsCategory || ''));
  $('androidSettingsBack').addEventListener('click', () => {
    returnToAndroidSettingsDirectory();
  });
  $('androidSettingsSearch').addEventListener('input', filterAndroidSettings);
  $('androidWorkspaceReadOnly').addEventListener('click', () => {
    if (androidWorkspaceVisible() && !androidMcpAccessBusy && androidMcpAccessSnapshot)
      void configureAndroidMcpAccess('read_only',androidMcpPresetMask('read_only'));
  });
  $('androidWorkspaceChooseFolder').addEventListener('click', () => {
    if (androidWorkspaceVisible() &&
        !($('androidWorkspaceChooseFolder') as HTMLButtonElement).disabled)
      ($('androidCosFolderBrowse') as HTMLButtonElement).click();
  });
  $('androidWorkspaceRunChecks').addEventListener('click', () => {
    if (androidWorkspaceVisible()) {
      androidWorkspaceChecksShown = true;
      void refreshAndroidWorkspace();
    }
  });
  $('androidWorkspaceCloseChecks').addEventListener('click', () => {
    androidWorkspaceChecksShown = false;
    renderAndroidWorkspace();
  });
  $('androidWorkspaceCopyActivity').addEventListener('click', async () => {
    if (!androidWorkspaceVisible()) return;
    const events = androidOpenAiTunnelSnapshot?.events || [];
    const status = $('androidWorkspaceActivityCopyStatus');
    if (!events.length) {
      status.textContent = 'No recorded events to copy.';
      return;
    }
    try {
      const content = androidWorkspaceActivityText(events);
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(content);
      } else {
        // Android WebView builds without the async Clipboard API still allow
        // an explicit user-tap copy through a temporary selection.
        const temporary = document.createElement('textarea');
        temporary.value = content;
        temporary.setAttribute('aria-hidden','true');
        temporary.style.cssText = 'position:fixed;left:-9999px;top:0;opacity:0';
        document.body.appendChild(temporary);
        try {
          temporary.focus();
          temporary.select();
          if (!document.execCommand('copy')) throw Error('Clipboard unavailable');
        } finally {
          temporary.remove();
        }
      }
      status.textContent = `Copied ${events.length} event(s).`;
    } catch {
      status.textContent = 'Clipboard unavailable. Event details remain visible above.';
    }
  });
  $('androidSettingsOpenReview').addEventListener('click', () => openToolPanel('review'));
  $('androidSettingsOpenFiles').addEventListener('click', () => openToolPanel('files'));
  $('provider').addEventListener('change', renderAndroidCosSetup);
  $('model').addEventListener('input', renderAndroidCosSetup);
  setAndroidSettingsView('general');
  setAndroidSettingsNavOpen(true);
}
function setSettings(open) {
  if (open) closeAndroidViewMenu();
  $('settingsPanel').classList.toggle('hidden', !open);
  if (nativeAndroidRuntime)
    document.querySelector('.shell')?.classList.toggle('android-settings-open', open);
  $('settingsToggle').setAttribute('aria-expanded', String(open));
  $('sidebarCustomize').setAttribute('aria-expanded', String(open));
  if (open) { closeSidebar(); closeToolPanel(); closeExtrasPanel(); }
  if (open && nativeAndroidRuntime) {
    setAndroidSettingsView('general');
    setAndroidSettingsNavOpen(true);
  }
  if (!open && nativeAndroidRuntime) {
    $('sidebarToggle').setAttribute('aria-expanded', String(sidebarOpen));
    $('sidebarToggle').setAttribute('aria-label', sidebarOpen ? 'Close conversations' : 'Open conversations');
  }
  if (open && nativeAndroidRuntime) {
    void refreshStorageAccess();
    void refreshAndroidCosOAuthStatus();
  }
  if (open && !nativeAndroidRuntime) void refreshOwnedChatgptStatus();
}
$('settingsToggle').addEventListener('click', () => setSettings($('settingsPanel').classList.contains('hidden')));
$('sidebarCustomize').addEventListener('click', () => setSettings($('settingsPanel').classList.contains('hidden')));
$('settingsClose').addEventListener('click', () => setSettings(false));
$('keyToggle').addEventListener('click', () => {
  if (['ollama', 'hermes', 'chatgpt-browser'].includes($('provider').value)) { announce('This engine manages its own model access', true); return; }
  $('keyBox').classList.toggle('hidden');
  $('keyToggle').setAttribute('aria-expanded', String(!$('keyBox').classList.contains('hidden')));
  if (!$('keyBox').classList.contains('hidden')) $('apiKey').focus();
});

$('saveKey').addEventListener('click', async () => {
  try {
    await api('/api/key', { provider: $('provider').value, key: $('apiKey').value });
    verifiedAiConnection = null;
    $('apiKey').value = '';
    $('keyBox').classList.add('hidden');
    $('keyToggle').setAttribute('aria-expanded', 'false');
    await refresh();
    announce('Key is kept in memory only. It disappears when Mcp Agent Ai closes.', true);
  } catch (error) { announce(error.message); }
});
$('clearKey').addEventListener('click', async () => {
  try {
    await api('/api/key', { provider: $('provider').value, key: '' });
    verifiedAiConnection = null;
    $('apiKey').value = '';
    await refresh();
    announce('Provider key cleared from memory', true);
  } catch (error) { announce(error.message); }
});

$('skillForm').addEventListener('submit', async event => {
  event.preventDefault();
  try {
    await api('/api/skill', { action: 'add', name: $('skillName').value, text: $('skillText').value });
    $('skillName').value = '';
    $('skillText').value = '';
    await refresh();
    announce('Skill imported disabled. Enable it when ready.', true);
  } catch (error) { announce(error.message); }
});

$('provider').addEventListener('change', () => {
  const browserProvider = $('provider').value === 'chatgpt-browser';
  $('model').disabled = browserProvider;
  if (browserProvider) $('model').value = 'chatgpt.com';
  $('model').placeholder = $('provider').value === 'hermes' ? 'Optional: use configured Hermes model' : 'Enter model ID';
  $('keyBox').classList.add('hidden');
  $('keyToggle').setAttribute('aria-expanded', 'false');
  $('keyToggle').textContent = state?.hasKeys?.includes($('provider').value) ? 'Key in memory' : 'Configure API key';
  $('keyToggle').disabled = ['ollama', 'hermes', 'chatgpt-browser'].includes($('provider').value);
  const selected = $('provider').value;
  const status = $('engineStatus');
  const saved = state?.hasKeys?.includes(selected);
  $('connectionIndicator').textContent = browserProvider ? 'Website session · not verified' : saved ? 'Key in memory' :
    ['ollama', 'hermes'].includes(selected) ? 'Local setup' : 'Not configured';
  updateWelcomeConnection();
  status.textContent = browserProvider ?
    'Uses the app-owned ChatGPT website after manual sign-in; no API key. Browser readiness alone is not a sent/received message.' :
    saved ? 'API key is held in memory only. Connection not yet verified.' :
    selected === 'ollama' ? 'Requires a running local Ollama server' :
    selected === 'hermes' ? 'Requires Hermes CLI and External agents permission' :
    selected ? 'Configure an API key for this session to send messages.' : 'No engine available';
  updateSidebarConnection();
  renderOwnedChatgptSteps();
  // A prior check may still be valid for this live window; the Use button is
  // relevant only when another provider is currently selected.
  showOwnedChatgptStatus(ownedChatgptStatus,
    $('backgroundChatgptStatus').textContent || 'Built-in browser is off.');
});
$('model').addEventListener('input', () => {
  updateSidebarConnection();
  updateAndroidComposerModel();
});
$('provider').addEventListener('change', updateAndroidComposerModel);
$('provider').dispatchEvent(new Event('change'));
refresh().then(() => $('provider').dispatchEvent(new Event('change')))
  .catch(error => announce(`Cannot load preview: ${error.message}`));
