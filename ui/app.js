const $ = id => document.getElementById(id);
let state = null;
let selectedSession = null;
let working = false;

async function api(route, body) {
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

function announce(message, good = false) {
  $('notice').textContent = message;
  $('notice').className = good ? 'notice good' : 'notice';
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = String(text);
  return node;
}

function displayMessages(session, pending = false) {
  const wrapper = $('messages');
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
    return;
  }
  for (const message of messages) {
    const node = el('article', `message ${message.role}`);
    const avatar = el('span', 'avatar', message.role === 'user' ? 'U' : '✳');
    const bubble = el('div', 'bubble');
    bubble.append(el('div', 'who', message.role === 'user' ? 'You' : 'COS Fusion'));
    bubble.append(el('div', 'body', message.text));
    node.append(avatar, bubble);
    wrapper.append(node);
  }
  if (pending) {
    const node = el('article', 'message pending');
    const avatar = el('span', 'avatar', '✳');
    const bubble = el('div', 'bubble');
    bubble.append(el('div', 'who', 'COS Fusion'));
    const dots = el('div', 'typing');
    dots.setAttribute('aria-label', 'Thinking');
    for (let i = 0; i < 3; i++) dots.append(el('i'));
    bubble.append(dots);
    node.append(avatar, bubble);
    wrapper.append(node);
  }
  wrapper.scrollTop = wrapper.scrollHeight;
}

const welcomeTemplate = $('welcome').innerHTML;

function renderThreads() {
  const wrapper = $('threads');
  wrapper.replaceChildren();
  $('threadCount').textContent = String(state.sessions.length);
  if (!state.sessions.length) wrapper.append(el('span', 'thread-muted', 'No conversations yet'));
  for (const session of state.sessions) {
    const title = session.messages?.find(message => message.role === 'user')?.text || 'Conversation';
    const button = el('button', session.id === selectedSession ? 'selected' : '', title.slice(0, 65));
    button.type = 'button';
    button.title = title;
    button.disabled = working;
    button.setAttribute('aria-current', session.id === selectedSession ? 'page' : 'false');
    button.addEventListener('click', () => {
      if (working) return;
      selectedSession = session.id;
      $('provider').value = session.provider || 'demo';
      $('provider').dispatchEvent(new Event('change'));
      $('model').value = session.model || '';
      displayMessages(session);
      renderThreads();
      closeSidebar();
    });
    wrapper.append(button);
  }
}

function renderModules() {
  const moduleInfo = {
    workspace: 'Read files inside the startup workspace',
    notes: 'Save and retrieve short local notes',
    external_agents: 'Run external AI agents and their configured tools',
  };
  const wrapper = $('modules');
  wrapper.replaceChildren();
  for (const [id, enabled] of Object.entries(state.modules)) {
    const row = el('label', 'module');
    const description = el('span');
    description.append(el('strong', '', id), el('small', '', moduleInfo[id] || 'Optional capability'));
    const check = el('input');
    check.type = 'checkbox';
    check.checked = enabled;
    check.addEventListener('change', async () => {
      check.disabled = true;
      try { await api('/api/module', { id, enabled: check.checked }); await refresh(); announce(`${id} ${check.checked ? 'enabled' : 'disabled'}`, true); }
      catch (error) { check.checked = !check.checked; announce(error.message); }
      finally { check.disabled = false; }
    });
    row.append(description, check);
    wrapper.append(row);
  }
}

function renderSkills() {
  const wrapper = $('skills');
  wrapper.replaceChildren();
  if (!state.skills.length) wrapper.append(el('p', '', 'No custom skills installed.'));
  for (const skill of state.skills) {
    const row = el('div', 'skill-row');
    const text = el('strong', '', skill.id);
    const toggle = el('input');
    toggle.type = 'checkbox';
    toggle.checked = skill.enabled;
    toggle.title = `Enable ${skill.id}`;
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

async function refresh() {
  state = await api('/api/state');
  $('workspace').textContent = state.workspace;
  renderThreads();
  renderModules();
  renderSkills();
}

function setBusy(busy) {
  working = busy;
  $('send').disabled = busy;
  $('send').textContent = busy ? 'Working…' : 'Send ↗';
  $('newChat').disabled = busy;
  $('newChatTop').disabled = busy;
  $('provider').disabled = busy;
  $('model').disabled = busy || $('provider').value === 'demo';
  $('prompt').readOnly = busy;
  $('keyToggle').disabled = busy || ['demo', 'ollama', 'hermes'].includes($('provider').value);
  $('settingsToggle').disabled = busy;
  if (state) renderThreads();
}

$('chatForm').addEventListener('submit', async event => {
  event.preventDefault();
  if (working) return;
  const prompt = $('prompt').value.trim();
  if (!prompt) return;
  const provider = $('provider').value;
  const model = $('model').value.trim();
  if (!['demo', 'hermes'].includes(provider) && !model) { announce('Enter a model ID for the selected provider'); return; }
  setBusy(true);
  announce('Sending request to agent…');
  const previous = selectedSession;
  const before = state?.sessions?.find(session => session.id === previous);
  displayMessages({ messages: [...(before?.messages || []), { role: 'user', text: prompt }] }, true);
  try {
    const result = await api('/api/chat', { prompt, provider, model, sessionId: previous });
    selectedSession = result.sessionId;
    $('prompt').value = '';
    updateCharCount();
    await refresh();
    displayMessages(state.sessions.find(session => session.id === selectedSession));
    announce(result.actions.length ? `${result.actions.length} tool action(s) recorded` : 'Response received', true);
  } catch (error) {
    if (before) displayMessages(before);
    else displayMessages(null);
    announce(`Request failed: ${error.message}`);
  } finally { setBusy(false); }
});

$('prompt').addEventListener('keydown', event => {
  if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
    event.preventDefault();
    $('chatForm').requestSubmit();
  }
});

function newConversation() {
  if (working) return;
  selectedSession = null;
  displayMessages(null);
  renderThreads();
  announce('');
  closeSidebar();
  $('prompt').focus();
}
$('newChat').addEventListener('click', newConversation);
$('newChatTop').addEventListener('click', newConversation);

function closeSidebar() {
  $('sidebar').classList.remove('open');
  $('sidebarBackdrop').classList.add('hidden');
  $('sidebarToggle').setAttribute('aria-expanded', 'false');
}
function toggleSidebar() {
  const opened = $('sidebar').classList.toggle('open');
  $('sidebarBackdrop').classList.toggle('hidden', !opened);
  $('sidebarToggle').setAttribute('aria-expanded', String(opened));
  if (opened) setSettings(false);
}
$('sidebarToggle').addEventListener('click', toggleSidebar);
$('sidebarBackdrop').addEventListener('click', closeSidebar);

function updateCharCount() {
  $('charCount').textContent = String($('prompt').value.length);
}
$('prompt').addEventListener('input', updateCharCount);
document.addEventListener('keydown', event => {
  if (event.key === 'Escape') {
    closeSidebar();
    setSettings(false);
    $('keyBox').classList.add('hidden');
    $('keyToggle').setAttribute('aria-expanded', 'false');
  }
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
    event.preventDefault();
    newConversation();
  }
});

function setSettings(open) {
  $('settingsPanel').classList.toggle('hidden', !open);
  $('settingsToggle').setAttribute('aria-expanded', String(open));
  if (open) closeSidebar();
}
$('settingsToggle').addEventListener('click', () => setSettings($('settingsPanel').classList.contains('hidden')));
$('settingsClose').addEventListener('click', () => setSettings(false));
$('keyToggle').addEventListener('click', () => {
  if (['demo', 'ollama', 'hermes'].includes($('provider').value)) { announce('This provider does not need a key in COS Fusion', true); return; }
  $('keyBox').classList.toggle('hidden');
  $('keyToggle').setAttribute('aria-expanded', String(!$('keyBox').classList.contains('hidden')));
  if (!$('keyBox').classList.contains('hidden')) $('apiKey').focus();
});

$('saveKey').addEventListener('click', async () => {
  try {
    await api('/api/key', { provider: $('provider').value, key: $('apiKey').value });
    $('apiKey').value = '';
    $('keyBox').classList.add('hidden');
    $('keyToggle').setAttribute('aria-expanded', 'false');
    await refresh();
    announce('Key kept in this app process only. It will disappear when COS Fusion closes.', true);
  } catch (error) { announce(error.message); }
});
$('clearKey').addEventListener('click', async () => {
  try {
    await api('/api/key', { provider: $('provider').value, key: '' });
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
  $('model').disabled = $('provider').value === 'demo';
  $('model').placeholder = $('provider').value === 'demo' ? 'Not required for demo' :
    $('provider').value === 'hermes' ? 'Optional: use configured Hermes model' : 'Enter model ID';
  $('keyBox').classList.add('hidden');
  $('keyToggle').setAttribute('aria-expanded', 'false');
  $('keyToggle').textContent = state?.hasKeys?.includes($('provider').value) ? 'Key ready' : 'API key';
  $('keyToggle').disabled = ['demo', 'ollama', 'hermes'].includes($('provider').value);
});
$('provider').dispatchEvent(new Event('change'));
refresh().then(() => $('provider').dispatchEvent(new Event('change')))
  .catch(error => announce(`Cannot load preview: ${error.message}`));
