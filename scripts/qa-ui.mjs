import { chromium } from 'playwright';
import { mkdtemp, mkdir, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { existsSync, readdirSync } from 'node:fs';
import { startServer } from '../src/server.mjs';

const projectRoot = fileURLToPath(new URL('..', import.meta.url));
const directory = await mkdtemp(path.join(os.tmpdir(), 'cos-fusion-ui-'));
const screenshotFolder = path.join(projectRoot, 'work', 'ui-qa');
await mkdir(screenshotFolder, { recursive: true });
await mkdir(path.join(directory, 'workspace'));
const app = await startServer({ home: path.join(directory, 'data'), workspaceRoot: path.join(directory, 'workspace'), openBrowser: false });
let browser;
let checks = 0;
function ensure(value, message) {
  if (!value) throw new Error(`UI QA failed: ${message}`);
  checks++;
}
async function closeNativeAndroidSettings(page) {
  if ((await page.locator('#settingsPanel').getAttribute('data-settings-nav-open')) !== 'true')
    await page.locator('#androidSettingsBack').click();
  await page.locator('#settingsClose').click();
}
async function pageGeometry(page, label, expectWelcome = true) {
  const result = await page.evaluate(() => ({
    documentWidth: document.documentElement.scrollWidth,
    screenWidth: window.innerWidth,
    composer: (() => { const rect = document.getElementById('prompt').getBoundingClientRect(); return { left: rect.left, right: rect.right, bottom: rect.bottom }; })(),
    titleVisible: (() => { const rect = document.querySelector('.welcome h1').getBoundingClientRect(); return rect.width > 0; })(),
  }));
  ensure(result.documentWidth <= result.screenWidth + 1, `${label}: page horizontally overflows by ${result.documentWidth - result.screenWidth}px`);
  ensure(result.composer.left >= 0 && result.composer.right <= result.screenWidth, `${label}: composer goes off-screen`);
  ensure(result.titleVisible === expectWelcome, `${label}: empty-chat welcome visibility is incorrect`);
  console.log(`${label}: ${result.screenWidth}px viewport, ${result.documentWidth}px document, composer right=${result.composer.right.toFixed(0)}`);
}

async function verifyAndroidGeneral(browser, url, screenshots, ensure, errors) {
  // Independent Android page: its reloads and local preferences must never
  // change the existing Setup, Workspace, provider or device QA fixtures.
  const page = await browser.newPage({viewport:{width:390,height:844},isMobile:true,hasTouch:true,
    userAgent:'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36'});
  page.on('pageerror', error => errors.push(error.message));
  await page.route('**/api/state', async route => {
    const response = await route.fetch();
    await route.fulfill({response,json:{...(await response.json()),platform:'android'}});
  });
  const openGeneral = async () => {
    await page.locator('#sidebarToggle').click();
    await page.locator('#sidebarCustomize').click();
    await page.locator('[data-android-settings-category="general"]').click();
    await page.locator('#androidGeneralSettings').waitFor({state:'visible'});
  };
  const stored = () => page.evaluate(() =>
    JSON.parse(localStorage.getItem('mcp-agent-ai-android-general-v1') || 'null'));
  const controls = () => page.evaluate(() => ({
    follow:document.getElementById('androidGeneralFollowOutput').checked,
    notify:document.getElementById('androidGeneralNotifyFinish').checked,
    detail:document.getElementById('androidGeneralNotificationDetail').value,
    priority:document.getElementById('androidGeneralNotificationPriority').value,
    during:document.getElementById('androidGeneralMessageDuringTurn').value,
  }));
  const expected = (follow,notify,detail,during,priority='both') =>
    ({follow,notify,detail,priority,during});
  const backup = (followOutput,notifyOnFinish,notificationDetail,messageDuringTurn,notificationPriority='both') =>
    JSON.stringify({version:3,followOutput,notifyOnFinish,notificationDetail,notificationPriority,messageDuringTurn});
  const assertControls = async (follow,notify,detail,during,label,priority='both') => ensure(
    JSON.stringify(await controls()) === JSON.stringify(expected(follow,notify,detail,during,priority)),label);
  const toggleGeneralNavigation = async () => {
    await page.locator('#androidSettingsBack').click();
    await page.locator('[data-android-settings-category="general"]').click();
  };

  await page.goto(url);
  await page.waitForFunction(() => document.documentElement.classList.contains('native-android'));
  await openGeneral();
  ensure(await page.locator('#androidGeneralSettings .android-general-section').count() === 5 &&
    JSON.stringify(await page.locator('#androidGeneralSettings .android-general-section-head h2').allTextContents()) ===
      JSON.stringify(['Chats','Notifications','Privacy','Backup','For developers']) &&
    await page.locator('#androidGeneralSettings').isVisible() &&
    await page.locator('#settingsPanel').getAttribute('data-settings-view') === 'general',
    'Android General must expose five COS-style sections without leaking unrelated categories');
  ensure(await page.locator('#androidGeneralSettings').count() === 1 &&
    await page.locator('[data-android-settings-category="general"]').count() === 1 &&
    await page.locator('#androidGeneralSettings [id^="androidGeneral"]').evaluateAll(nodes =>
      nodes.every(node => document.querySelectorAll(`#${node.id}`).length === 1)),
    'General must use one canonical settings view and one of each control');
  await assertControls(true,false,'task_finished','queue',
    'General defaults must follow output and leave notifications off with manual follow-up queue');
  ensure(await stored() === null &&
    await page.locator('#androidGeneralBackupApply').isDisabled() &&
    await page.locator('#androidGeneralNotificationDetail').isDisabled() &&
    await page.locator('#androidGeneralNotificationPriority').isDisabled() &&
    await page.locator('#androidGeneralSendShortcut,#androidGeneralSoundCues').count() === 0 &&
    (await page.locator('.compose-foot .key-hint').innerText()).includes('Ctrl/⌘+Enter'),
    'Opening General must not save defaults, enable content while off or retain obsolete selectors');
  ensure(await page.locator('#androidForegroundRuntimeSection,#androidForegroundRuntimeStart,#androidForegroundRuntimeStop').count() === 0 &&
    !/background session|acquire wakelock|release wakelock/i.test(await page.locator('#androidGeneralSettings').innerText()),
    'General must not resurrect the Termux-style native foreground controls');

  // Include the expanded backup editor in bounds checks; it is commonly
  // overlooked because the import control starts collapsed.
  // Developer diagnostics are intentionally hidden until Developer mode is
  // enabled. Validate their real tap targets with that preference active.
  await page.locator('#androidDeveloperMode').check();
  ensure(await page.locator('#androidGeneralOpenHealth').isVisible() &&
    await page.locator('#androidGeneralOpenReview').isVisible(),
    'Developer mode must expose diagnostics inside General');
  await page.locator('#androidGeneralImport summary').click();
  for (const width of [320,360,390]) {
    await page.setViewportSize({width,height:844});
    const geometry = await page.evaluate(() => {
      const ids = ['androidGeneralSettings','androidGeneralFollowOutput','androidGeneralNotifyFinish',
        'androidGeneralNotificationDetail','androidGeneralNotificationPriority','androidGeneralMessageDuringTurn',
        'androidGeneralOpenPermissions','androidGeneralOpenWorkspace',
        'androidGeneralExport','androidGeneralBackupText','androidGeneralBackupPreview',
        'androidGeneralBackupApply','androidGeneralOpenHealth','androidGeneralOpenReview'];
      return {screen:innerWidth,document:document.documentElement.scrollWidth,
        sections:document.querySelectorAll('#androidGeneralSettings > .android-general-section').length,
        boxes:ids.map(id => {
          const element=document.getElementById(id),rect=element.getBoundingClientRect();
          return {id,left:rect.left,right:rect.right,width:rect.width,height:rect.height};
        })};
    });
    ensure(geometry.document <= width+1 && geometry.sections === 5 &&
      geometry.boxes.every(item => item.left >= -1 && item.right <= width+1 &&
        item.width > 0 && item.height > 0) &&
      geometry.boxes.filter(item => /Open|Export|Preview|Apply/.test(item.id))
        .every(item => item.height >= 40),
      `Android General clips sections or touch controls at ${width}px: ${JSON.stringify(geometry)}`);
    await page.screenshot({path:path.join(screenshots,`native-android-general-${width}.png`),fullPage:true});
  }
  await page.locator('#androidDeveloperMode').uncheck();
  await page.setViewportSize({width:390,height:410});
  await page.locator('#androidGeneralBackupText').focus();
  const compact = await page.evaluate(() => ({
    screen:innerWidth,document:document.documentElement.scrollWidth,
    field:document.getElementById('androidGeneralBackupText').getBoundingClientRect(),
    shell:document.querySelector('.shell').getBoundingClientRect(),
  }));
  ensure(compact.document <= 391 && compact.field.left >= -1 && compact.field.right <= 391 &&
    compact.field.width >= 200 && compact.shell.bottom <= 411,
    `General backup must remain usable with soft-keyboard-height viewport: ${JSON.stringify(compact)}`);
  await page.setViewportSize({width:390,height:844});

  await page.locator('#androidGeneralFollowOutput').uncheck();
  await page.locator('#androidGeneralNotifyFinish').check();
  ensure(await page.locator('#androidGeneralNotificationDetail').isEnabled(),
    'Enabling notifications must enable content selector');
  ensure(await page.locator('#androidGeneralNotificationPriority').isEnabled(),
    'Enabling notifications must enable delivery priority selector');
  await page.locator('#androidGeneralNotificationDetail').selectOption('full_message');
  await page.locator('#androidGeneralNotificationPriority').selectOption('foreground');
  await page.locator('#androidGeneralMessageDuringTurn').selectOption('steer');
  await assertControls(false,true,'full_message','steer',
    'Changing General follow, notification and during-turn controls must update live form state','foreground');
  ensure(JSON.stringify(await stored()) === backup(false,true,'full_message','steer','foreground') &&
    (await page.locator('.compose-foot .key-hint').innerText()).includes('Ctrl/⌘+Enter'),
    'General preferences must persist as strict version 3, with stable keyboard shortcut');
  await toggleGeneralNavigation();
  await assertControls(false,true,'full_message','steer','General settings must survive category navigation','foreground');
  await page.reload();
  await page.waitForFunction(() => document.documentElement.classList.contains('native-android'));
  await openGeneral();
  await assertControls(false,true,'full_message','steer',
    'General must restore all saved preferences after full renderer reload','foreground');

  await page.evaluate(() => {
    window.__generalCopied = [];
    Object.defineProperty(navigator,'clipboard',{configurable:true,
      value:{writeText:async value => window.__generalCopied.push(value)}});
  });
  await page.locator('#androidGeneralExport').click();
  await page.waitForFunction(() => window.__generalCopied.length === 1);
  const copied = await page.evaluate(() => window.__generalCopied[0]);
  ensure(copied === JSON.stringify(JSON.parse(backup(false,true,'full_message','steer','foreground')),null,2) &&
    JSON.stringify(Object.keys(JSON.parse(copied)).sort()) ===
      JSON.stringify(['followOutput','notifyOnFinish','notificationDetail','notificationPriority','messageDuringTurn','version'].sort()) &&
    !/apiKey|accessToken|password|folder|sessions/i.test(copied) &&
    (await page.locator('#androidGeneralBackupStatus').innerText()).includes('copied'),
    'General export must write only the five device preferences and version to clipboard');
  await page.evaluate(() => {
    Object.defineProperty(navigator,'clipboard',{configurable:true,
      value:{writeText:async () => {throw Error('Denied clipboard access');}}});
  });
  await page.locator('#androidGeneralExport').click();
  await page.waitForFunction(() => document.getElementById('androidGeneralBackupText').value.includes('"version"'));
  ensure(await page.locator('#androidGeneralImport').getAttribute('open') !== null &&
    (await page.locator('#androidGeneralBackupText').inputValue()) === copied &&
    (await page.locator('#androidGeneralBackupStatus').innerText()).includes('Clipboard unavailable') &&
    await page.locator('#androidGeneralBackupApply').isDisabled(),
    'Denied clipboard export must reveal selectable JSON without silently importing it');

  const targetBackup = backup(true,false,'task_finished','queue');
  const beforePreview = await stored();
  await page.locator('#androidGeneralBackupText').fill(targetBackup);
  await page.locator('#androidGeneralBackupPreview').click();
  ensure((await page.locator('#androidGeneralBackupStatus').innerText()).includes('Apply to confirm') &&
    await page.locator('#androidGeneralBackupApply').isEnabled() &&
    JSON.stringify(await stored()) === JSON.stringify(beforePreview),
    'Preview must describe pending changes and never mutate stored General state');
  await assertControls(false,true,'full_message','steer','Preview must not apply backup before explicit confirmation','foreground');
  await page.locator('#androidGeneralBackupText').fill(backup(false,true,'task_finished','steer'));
  ensure(await page.locator('#androidGeneralBackupApply').isDisabled() &&
    JSON.stringify(await stored()) === JSON.stringify(beforePreview),
    'Editing backup after preview must invalidate the Apply action');

  for (const invalid of [
    '{malformed-json',
    JSON.stringify({...JSON.parse(targetBackup),version:4}),
    JSON.stringify({...JSON.parse(targetBackup),followOutput:'true'}),
    JSON.stringify({...JSON.parse(targetBackup),apiKey:'do-not-import'}),
    JSON.stringify({...JSON.parse(targetBackup),notificationDetail:'everything'}),
    JSON.stringify({...JSON.parse(targetBackup),notificationPriority:'urgent'}),
    JSON.stringify({...JSON.parse(targetBackup),messageDuringTurn:'parallel'}),
    JSON.stringify({...JSON.parse(targetBackup),notifyOnFinish:1}),
    JSON.stringify([JSON.parse(targetBackup)]),
    JSON.stringify({version:3,followOutput:true,notifyOnFinish:false,notificationDetail:'task_finished',messageDuringTurn:'queue'}),
  ]) {
    await page.locator('#androidGeneralBackupText').fill(invalid);
    await page.locator('#androidGeneralBackupPreview').click();
    ensure(await page.locator('#androidGeneralBackupApply').isDisabled() &&
      (await page.locator('#androidGeneralBackupStatus').innerText()).includes('Invalid backup') &&
      JSON.stringify(await stored()) === JSON.stringify(beforePreview),
      `General must fail closed on malformed, incompatible or over-scoped backup: ${invalid}`);
  }
  await page.locator('#androidGeneralBackupText').fill(targetBackup);
  await page.locator('#androidGeneralBackupPreview').click();
  await page.locator('#androidGeneralBackupApply').click();
  await assertControls(true,false,'task_finished','queue',
    'Apply must atomically restore the backed-up values');
  ensure(JSON.stringify(await stored()) === targetBackup &&
    await page.locator('#androidGeneralBackupApply').isDisabled() &&
    (await page.locator('#androidGeneralBackupStatus').innerText()).includes('imported and applied'),
    'Valid backup Apply must persist exactly once and disable repeat Apply');
  await page.reload();
  await page.waitForFunction(() => document.documentElement.classList.contains('native-android'));
  await openGeneral();
  await assertControls(true,false,'task_finished','queue','Applied backup must survive full reload');
  // Previously saved v2 preferences upgrade without silently changing when
  // notifications fire: missing priority means the legacy "both" behavior.
  await page.evaluate(() => localStorage.setItem('mcp-agent-ai-android-general-v1',
    JSON.stringify({version:2,followOutput:false,notifyOnFinish:true,
      notificationDetail:'full_message',messageDuringTurn:'steer'})));
  await page.reload();
  await page.waitForFunction(() => document.documentElement.classList.contains('native-android'));
  await openGeneral();
  await assertControls(false,true,'full_message','steer',
    'Legacy v2 records must preserve all preferences and default priority to Both');
  await page.locator('#androidGeneralNotificationPriority').selectOption('background');
  await assertControls(false,true,'full_message','steer',
    'Changing migrated v2 priority must persist the background-only choice','background');
  ensure(JSON.stringify(await stored()) === backup(false,true,'full_message','steer','background'),
    'Saving a migrated v2 record must create complete strict v3 preferences');
  // Existing phones may have the old v1 record; only its followOutput bit
  // carries over. Old notification-sound and Send shortcut choices expire.
  await page.evaluate(() => localStorage.setItem('mcp-agent-ai-android-general-v1',
    JSON.stringify({version:1,followOutput:false,sendShortcut:'enter',soundCues:true})));
  await page.reload();
  await page.waitForFunction(() => document.documentElement.classList.contains('native-android'));
  await openGeneral();
  await assertControls(false,false,'task_finished','queue',
    'Migrating v1 keeps only followOutput and resets retired options to safe v3 defaults');
  await page.locator('#androidGeneralFollowOutput').check();
  ensure(JSON.stringify(await stored()) === backup(true,false,'task_finished','queue'),
    'Saving migrated v1 preference must rewrite localStorage as complete strict v3 record');

  // Exercise the real keydown handler while replacing requestSubmit with a
  // counter. No chat request or account credentials are needed for this QA.
  await page.locator('#androidSettingsBack').click();
  await page.locator('#settingsClose').click();
  await page.evaluate(() => {
    window.__generalSubmitCount = 0;
    document.getElementById('chatForm').requestSubmit = () => {window.__generalSubmitCount++;};
  });
  const prompt = page.locator('#prompt');
  await prompt.fill('Keyboard shortcut QA');
  await prompt.press('Enter');
  ensure(await page.evaluate(() => window.__generalSubmitCount) === 0 &&
    (await prompt.inputValue()).includes('\n'),
    'Modifier mode plain Enter must insert a newline without triggering Send');
  await prompt.press('Control+Enter');
  ensure(await page.evaluate(() => window.__generalSubmitCount) === 1,
    'Modifier mode Ctrl+Enter must reach the Send handler exactly once');
  await prompt.press('Shift+Enter');
  ensure(await page.evaluate(() => window.__generalSubmitCount) === 1,
    'Shift+Enter must remain a newline in modifier mode');
  await prompt.fill('Enter remains newline QA');
  await prompt.press('Enter');
  ensure(await page.evaluate(() => window.__generalSubmitCount) === 1 &&
    (await prompt.inputValue()).includes('\n'),
    'Removal of shortcut setting must keep Android bare Enter as a newline');

  await openGeneral();
  await page.locator('#androidGeneralOpenPermissions').click();
  ensure(await page.locator('#settingsPanel').getAttribute('data-settings-view') === 'setup' &&
    await page.locator('[data-cos-setup-step="access"]').getAttribute('aria-current') === 'step',
    'General notifications must deep-link to the actual Setup permissions stage');
  await toggleGeneralNavigation();
  await page.locator('#androidGeneralOpenWorkspace').click();
  ensure(await page.locator('#settingsPanel').getAttribute('data-settings-view') === 'workspace' &&
    await page.locator('#androidWorkspaceDashboard').isVisible(),
    'General MCP permissions button must navigate to actual Workspace');
  await toggleGeneralNavigation();
  await page.locator('#androidDeveloperMode').check();
  await page.locator('#androidGeneralOpenHealth').click();
  ensure(await page.locator('#settingsPanel').getAttribute('data-settings-view') === 'workspace' &&
    await page.locator('#androidWorkspaceHealthTitle').isVisible(),
    'General developer Health must open existing Workspace Health diagnostics');
  await toggleGeneralNavigation();
  await page.locator('#androidGeneralOpenReview').click();
  ensure(await page.locator('#toolViewPanel').isVisible() &&
    await page.locator('#toolViewTitle').innerText() === 'Review' &&
    await page.locator('#settingsPanel').isHidden(),
    'General developer Review must open the existing native Review panel');
  await page.locator('#toolViewClose').click();
  await page.close();
}

async function verifyAndroidGeneralDuringTurn(browser, url, ensure, errors) {
  // Mock just the OpenAI OAuth/native IPC boundary. The real renderer handles
  // button clicks, localStorage, turn lifecycle and notification dispatch.
  // Every artificial reply is explicitly controlled; no user account or device.
  const page = await browser.newPage({viewport:{width:390,height:844},isMobile:true,hasTouch:true,
    userAgent:'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36'});
  page.on('pageerror', error => errors.push(error.message));
  await page.route('**/api/state', async route => {
    const response = await route.fetch();
    await route.fulfill({response,json:{...(await response.json()),platform:'android'}});
  });
  await page.goto(url);
  await page.waitForFunction(() => document.documentElement.classList.contains('native-android'));
  await page.evaluate(() => {
    window.isTauri = true;
    window.confirm = () => true;
    window.__generalFlowCalls = [];
    window.__generalFlowNotifications = [];
    window.__generalFlowNotificationAttempts = [];
    window.__generalFlowAppForeground = true;
    window.__generalFlowSession = null;
    window.__generalFlowHoldNext = false;
    window.__generalFlowRelease = null;
    window.__generalFlowOutcome = 'completed';
    window.__generalFlowDenied = false;
    const sessionId = '2a722e44-7b32-48d2-b993-1fb9b30190a1';
    window.__TAURI_INTERNALS__ = {
      invoke: async (command,args={}) => {
        if (command === 'android_chatgpt_oauth_status') return {
          supported:true,state:'linked',pending:false,planEnabled:true,sessionLinked:true,
          signInVerified:true,message:'Synthetic user-approved ChatGPT plan session'
        };
        if (command === 'api_request') {
          const response = await fetch(args.route, {
            method:args.body == null ? 'GET' : 'POST',
            headers:args.body == null ? undefined : {'Content-Type':'application/json'},
            body:args.body == null ? undefined : JSON.stringify(args.body)
          });
          const data = await response.json();
          if (args.route === '/api/state') {
            data.platform = 'android';
            data.sessions = window.__generalFlowSession ? [structuredClone(window.__generalFlowSession)] : [];
          }
          return data;
        }
        if (command === 'chatgpt_oauth_chat') {
          if (args.userInitiated !== true || typeof args.prompt !== 'string' ||
              args.prompt.trim().length === 0 ||
              args.sessionId !== (window.__generalFlowSession?.id ?? null) ||
              !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(args.requestId))
            throw Error('Invalid or unapproved synthetic OAuth request');
          window.__generalFlowCalls.push({...args});
          if (window.__generalFlowHoldNext) {
            window.__generalFlowHoldNext = false;
            await new Promise(resolve => {window.__generalFlowRelease = resolve;});
            window.__generalFlowRelease = null;
          }
          if (window.__generalFlowOutcome !== 'completed')
            return {requestId:args.requestId,state:'uncertain',reason:'response_cancelled'};
          const prior = window.__generalFlowSession;
          const next = {id:sessionId,provider:'chatgpt-browser',model:'chatgpt.com',
            messages:[...(prior?.messages || []),{role:'user',text:args.prompt},
              {role:'assistant',text:'Synthetic verified answer 🌟'}],
            actions:[...(prior?.actions || []),
              {type:'chatgpt_oauth_turn',nativeConfirmed:true,mcpToolsUsed:false}]};
          window.__generalFlowSession = next;
          return {requestId:args.requestId,state:'completed',saved:true,status:'confirmed',
            sessionId,answer:'Synthetic verified answer 🌟',actions:[]};
        }
        if (command === 'android_turn_notification') {
          if (args.userInitiated !== true || !['full_message','task_finished'].includes(args.mode) ||
              typeof args.message !== 'string' ||
              !['foreground','background','both'].includes(args.notificationPriority))
            throw Error('Invalid synthetic notification request');
          window.__generalFlowNotificationAttempts.push({...args});
          const matches = args.notificationPriority === 'both' ||
            (args.notificationPriority === 'foreground' && window.__generalFlowAppForeground) ||
            (args.notificationPriority === 'background' && !window.__generalFlowAppForeground);
          if (!matches) return {posted:false,reason:'priority_filtered'};
          window.__generalFlowNotifications.push({...args});
          return window.__generalFlowDenied
            ? {posted:false,reason:'permission_denied'} : {posted:true};
        }
        throw Error('Native QA command unavailable: ' + command);
      }
    };
  });
  const openGeneral = async () => {
    await page.locator('#sidebarToggle').click();
    await page.locator('#sidebarCustomize').click();
    await page.locator('[data-android-settings-category="general"]').click();
  };
  const closeGeneral = async () => {
    await page.locator('#androidSettingsBack').click();
    await page.locator('#settingsClose').click();
  };
  const send = async value => {
    await page.locator('#prompt').fill(value);
    await page.locator('#send').click();
  };
  const beginHeld = async value => {
    await page.evaluate(() => {window.__generalFlowHoldNext = true;});
    await send(value);
    await page.waitForFunction(() => window.__generalFlowRelease !== null);
  };
  const release = async outcome => {
    await page.evaluate(mode => {
      window.__generalFlowOutcome = mode;
      window.__generalFlowRelease();
    },outcome);
  };
  const calls = () => page.evaluate(() => window.__generalFlowCalls.map(call => call.prompt));
  const notifications = () => page.evaluate(() => window.__generalFlowNotifications);
  const pending = page.locator('#androidDuringTurnPending');

  await beginHeld('Primary request A');
  ensure(await page.locator('#prompt').inputValue() === '' &&
    !(await page.locator('#prompt').getAttribute('readonly')) &&
    await page.locator('#send').isEnabled() &&
    (await calls()).length === 1,
    'Android must clear submitted prompt and keep one editable follow-up slot while first turn runs');
  await send('Queued follow-up B');
  ensure(await pending.isVisible() &&
    (await page.locator('#androidDuringTurnPendingText').innerText()).includes('return to your composer') &&
    await page.locator('#prompt').inputValue() === '' &&
    await page.locator('#send').isDisabled() &&
    JSON.stringify(await calls()) === JSON.stringify(['Primary request A']),
    'Default queue must accept exactly one follow-up without prematurely dispatching a second provider call');
  await page.locator('#androidDuringTurnCancel').click();
  ensure(await pending.isHidden() &&
    await page.locator('#prompt').inputValue() === 'Queued follow-up B' &&
    await page.locator('#send').isEnabled(),
    'Remove must restore the waiting draft and reopen the bounded slot during an active turn');
  await page.locator('#send').click();
  ensure(await pending.isVisible() && (await calls()).length === 1,
    'Re-queued follow-up must still consume only one slot with no duplicate backend request');
  await release('completed');
  await page.waitForFunction(() => document.getElementById('prompt').value === 'Queued follow-up B' &&
    document.getElementById('androidDuringTurnPending').classList.contains('hidden'));
  ensure(JSON.stringify(await calls()) === JSON.stringify(['Primary request A']) &&
    (await notifications()).length === 0 &&
    (await page.locator('#notice').innerText()).includes('tap Send'),
    'Queue mode must return a reviewable draft after success, never auto-send or notify by default');
  await page.locator('#send').click();
  await page.waitForFunction(() => window.__generalFlowCalls.length === 2 &&
    document.getElementById('send').disabled === false);
  ensure(JSON.stringify(await calls()) === JSON.stringify(['Primary request A','Queued follow-up B']) &&
    await page.locator('#prompt').inputValue() === '',
    'Explicit second Send must dispatch exactly once after the first completed and clear the old draft');

  await openGeneral();
  await page.locator('#androidGeneralNotifyFinish').check();
  await page.locator('#androidGeneralNotificationDetail').selectOption('full_message');
  await page.locator('#androidGeneralMessageDuringTurn').selectOption('steer');
  ensure((await page.locator('#androidGeneralDuringTurnHint').innerText()).includes('cannot inject') &&
    await page.locator('#androidGeneralNotificationDetail').isEnabled(),
    'Steer must explain verified-next-turn dispatch and notification detail only enables when opted in');
  await closeGeneral();
  await beginHeld('Primary request C');
  await send('Steered follow-up D');
  ensure(await pending.isVisible() &&
    (await page.locator('#androidDuringTurnPendingText').innerText()).includes('after the current reply is verified') &&
    JSON.stringify(await calls()) === JSON.stringify([
      'Primary request A','Queued follow-up B','Primary request C']),
    'Steer must collect a prompt while busy without concurrently calling the OAuth transport');
  await release('completed');
  await page.waitForFunction(() => window.__generalFlowCalls.length === 4 &&
    window.__generalFlowNotifications.length === 2 &&
    document.getElementById('send').disabled === false);
  ensure(JSON.stringify(await calls()) === JSON.stringify([
    'Primary request A','Queued follow-up B','Primary request C','Steered follow-up D']) &&
    (await notifications()).every(item => item.mode === 'full_message' &&
      item.message === 'Synthetic verified answer 🌟' && item.userInitiated === true) &&
    await pending.isHidden(),
    'Steer must auto-dispatch one next turn only after first verification and notify once per confirmed reply');

  await openGeneral();
  await page.locator('#androidGeneralNotificationDetail').selectOption('task_finished');
  await page.locator('#androidGeneralMessageDuringTurn').selectOption('queue');
  await closeGeneral();
  await send('Task-finished notification');
  await page.waitForFunction(() => window.__generalFlowNotifications.length === 3 &&
    document.getElementById('send').disabled === false);
  ensure((await notifications()).at(-1).mode === 'task_finished' &&
    (await notifications()).at(-1).message === '',
    'Task-finished notification must contain no response text');
  await page.evaluate(() => {window.__generalFlowDenied = true;});
  await send('Denied notification permission');
  await page.waitForFunction(() => window.__generalFlowNotifications.length === 4 &&
    document.getElementById('send').disabled === false);
  ensure((await notifications()).length === 4 &&
    (await page.locator('#androidGeneralBackupStatus').textContent()).includes('notifications are disabled'),
    'Permission-denied native notification must be fail-closed without altering a verified reply');
  await page.evaluate(() => {window.__generalFlowDenied = false;});
  await openGeneral();
  await page.locator('#androidGeneralNotificationPriority').selectOption('background');
  await closeGeneral();
  await send('Should not notify while app is open');
  await page.waitForFunction(() => window.__generalFlowNotificationAttempts.length === 5 &&
    document.getElementById('send').disabled === false);
  ensure((await notifications()).length === 4 &&
    (await page.evaluate(() => window.__generalFlowNotificationAttempts.at(-1).notificationPriority)) === 'background',
    'Background priority must suppress completed-turn alerts while app is foreground');
  await page.evaluate(() => {window.__generalFlowAppForeground = false;});
  await send('Notify when app is background');
  await page.waitForFunction(() => window.__generalFlowNotifications.length === 5 &&
    document.getElementById('send').disabled === false);
  ensure((await notifications()).at(-1).notificationPriority === 'background',
    'Background priority must allow a confirmed reply to notify in the background');
  await openGeneral();
  await page.locator('#androidGeneralNotificationPriority').selectOption('foreground');
  await closeGeneral();
  await send('Should not notify while app is background');
  await page.waitForFunction(() => window.__generalFlowNotificationAttempts.length === 7 &&
    document.getElementById('send').disabled === false);
  ensure((await notifications()).length === 5,
    'Foreground priority must suppress the completed-turn notification in background');
  await page.evaluate(() => {window.__generalFlowAppForeground = true;});
  await send('Notify while app is foreground');
  await page.waitForFunction(() => window.__generalFlowNotifications.length === 6 &&
    document.getElementById('send').disabled === false);
  ensure((await notifications()).at(-1).notificationPriority === 'foreground',
    'Foreground priority must allow the completed-turn notification while app is open');

  await openGeneral();
  await page.locator('#androidGeneralMessageDuringTurn').selectOption('steer');
  await closeGeneral();
  await beginHeld('Potentially uncertain request');
  await send('Must remain a draft');
  const countBefore = (await calls()).length;
  const notifyBefore = (await notifications()).length;
  await release('uncertain');
  await page.waitForFunction(() => document.getElementById('prompt').value === 'Must remain a draft' &&
    document.getElementById('androidDuringTurnPending').classList.contains('hidden'));
  ensure((await calls()).length === countBefore &&
    (await notifications()).length === notifyBefore &&
    (await page.locator('#notice').innerText()).includes('Check the previous turn') &&
    await page.locator('#prompt').inputValue() === 'Must remain a draft',
    'Cancelled or uncertain response must not auto-send steer follow-up or post a finished notification');

  // Compare both Follow choices with an actual user scroll event in the chat
  // transcript. A populated body is required; an empty chat cannot scroll.
  await page.evaluate(async () => {
    const pane=document.getElementById('messages');
    for(let i=0;i<65;i++) {
      const row=document.createElement('p');
      row.textContent='Synthetic visible long transcript line '+i;
      row.style.minHeight='35px';
      pane.appendChild(row);
    }
    await new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));
    pane.scrollTop=0;
    pane.dispatchEvent(new Event('scroll'));
  });
  await page.waitForFunction(() => {
    const pane=document.getElementById('messages');
    return pane.scrollHeight>pane.clientHeight && pane.scrollTop<=3;
  });
  ensure(await page.evaluate(() => {
    const pane=document.getElementById('messages');
    return pane.scrollTop<=3;
  }), 'Follow new output ON must allow reading earlier messages once the turn is finished');
  await openGeneral();
  await page.locator('#androidGeneralFollowOutput').uncheck();
  await closeGeneral();
  await page.evaluate(async () => {
    const pane=document.getElementById('messages');
    pane.scrollTop=0;
    pane.dispatchEvent(new Event('scroll'));
    await new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));
  });
  ensure(await page.evaluate(() => document.getElementById('messages').scrollTop <= 3),
    'Follow new output OFF must permit reading older content without snapping down');
  await page.close();
}

try {
  // Reuse a locally installed Playwright Chromium when npm and browser revisions differ.
  const browsersRoot = path.join(os.homedir(), 'AppData', 'Local', 'ms-playwright');
  const browserPaths = existsSync(browsersRoot)
    ? readdirSync(browsersRoot).filter(folder => /^chromium-\d+$/.test(folder))
      .map(folder => path.join(browsersRoot, folder, 'chrome-win64', 'chrome.exe'))
    : [];
  const existingChromium = browserPaths.find(candidate => existsSync(candidate));
  browser = await chromium.launch({ ...(existingChromium ? { executablePath: existingChromium } : {}), headless: true, args: ['--disable-extensions'] });
  const desktop = await browser.newPage({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
  const errors = [];
  desktop.on('pageerror', error => errors.push(error.message));
  await desktop.goto(app.url);
  await desktop.locator('#threadCount').waitFor();
  await pageGeometry(desktop, 'Desktop 1440x900');
  await desktop.screenshot({ path: path.join(screenshotFolder, 'desktop-1440.png'), fullPage: true });
  await desktop.locator('#settingsToggle').click();
  ensure(await desktop.locator('#settingsPanel').isVisible(), 'desktop customize panel did not open');
  ensure(await desktop.locator('#customBrowserSettings').isHidden() &&
    await desktop.locator('#customBrowserWindowsOpen').count() === 0,
    'Android-only custom browser must not expose desktop Windows controls');
  ensure(await desktop.locator('#androidCosSetupWizard').isHidden() &&
    await desktop.locator('#accessibilityManage').isHidden(),
    'Android COS Setup wizard and Allow controls must stay hidden on Windows');
  ensure(await desktop.locator('#settingsPanel .settings-head strong').isVisible(), 'settings title absent');
  await desktop.locator('#modules input[type="checkbox"]').first().check();
  // The toggle performs an asynchronous authenticated write. Checking the
  // server immediately after the DOM checkbox changes races the response.
  await desktop.waitForFunction(async () => {
    try {
      const response = await fetch('/api/state');
      return response.ok && (await response.json()).modules.workspace === true;
    } catch { return false; }
  }, undefined, {timeout:5000});
  ensure((await desktop.evaluate(async () => (await fetch('/api/state')).json())).modules.workspace,
    'workspace module did not enable');
  await desktop.locator('.module-details').first().waitFor({ state: 'visible', timeout: 5000 });
  ensure(await desktop.locator('.module-details').count() === 1, 'enabled module details must appear below its toggle');
  ensure(await desktop.locator('.module-details').first().isVisible(), 'enabled module details are hidden');
  await desktop.screenshot({ path: path.join(screenshotFolder, 'desktop-customize.png'), fullPage: true });
  await desktop.locator('#skillName').fill('qa-skill');
  await desktop.locator('#skillText').fill('# QA guidance\nAlways verify output');
  await desktop.locator('#skillForm button').click();
  await desktop.getByText('qa-skill', { exact: true }).waitFor({ timeout: 5000 });
  ensure(await desktop.getByText('qa-skill', { exact: true }).isVisible(), 'skill import failed');
  await desktop.locator('#settingsClose').click();
  ensure(!(await desktop.locator('#settingsPanel').isVisible()), 'desktop customize panel did not close');
  ensure(!(await desktop.locator('#provider').isVisible()), 'provider selection leaked outside Settings');
  await desktop.locator('#settingsToggle').click();
  await desktop.locator('#provider').selectOption('openai');
  ensure(await desktop.locator('#provider option[value="demo"]').count() === 0, 'demo engine still present');
  await desktop.locator('#settingsClose').click();
  await desktop.locator('#prompt').fill('Check connection');
  await desktop.locator('#prompt').press('Enter');
  await desktop.getByText('Enter a model ID for the selected provider').waitFor({ timeout: 5000 });
  ensure(await desktop.locator('#threads button').count() === 0, 'invalid request created a conversation');
  await desktop.locator('#newChat').click();
  ensure(await desktop.locator('.welcome h1').isVisible(), 'new chat did not restore welcome');
  await desktop.keyboard.press('Control+k');
  ensure(await desktop.locator('.welcome h1').isVisible(), 'Ctrl+K new conversation shortcut failed');
  await desktop.locator('#settingsToggle').click();
  await desktop.locator('#provider').selectOption('hermes');
  ensure(!(await desktop.locator('#model').isDisabled()), 'Hermes optional model ID should remain editable');
  ensure(await desktop.locator('#keyToggle').isDisabled(), 'Hermes must not request a second API key');
  await desktop.locator('#provider').selectOption('openai');
  await desktop.locator('#settingsClose').click();
  ensure(errors.length === 0, `desktop JavaScript errors: ${errors.join('; ')}`);
  await desktop.screenshot({ path: path.join(screenshotFolder, 'desktop-after-qa.png'), fullPage: true });

  // Desktop browser preview above remains only a development test harness; no Windows product.
  const mobile = await browser.newPage({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 1, isMobile: true, hasTouch: true });
  mobile.on('pageerror', error => errors.push(error.message));
  await mobile.goto(app.url);
  await pageGeometry(mobile, 'Mobile 390x844');
  ensure(await mobile.locator('#sidebarToggle').isVisible(), 'mobile navigation button is missing');
  await mobile.locator('#sidebarToggle').click();
  ensure(await mobile.locator('#sidebar').evaluate(node => node.classList.contains('open')), 'mobile drawer failed to open');
  await mobile.locator('#sidebarBackdrop').click({ position: { x: 380, y: 120 } });
  ensure(!(await mobile.locator('#sidebar').evaluate(node => node.classList.contains('open'))), 'mobile drawer did not close');
  await mobile.locator('#newChatTop').click();
  ensure(await mobile.locator('.welcome h1').isVisible(), 'mobile New button failed');
  await mobile.locator('#settingsToggle').click();
  ensure(await mobile.locator('#settingsPanel').isVisible(), 'mobile customize did not open');
  await mobile.screenshot({ path: path.join(screenshotFolder, 'mobile-customize.png'), fullPage: true });
  await mobile.keyboard.press('Escape');
  ensure(!(await mobile.locator('#settingsPanel').isVisible()), 'Escape failed to close settings');
  await mobile.screenshot({ path: path.join(screenshotFolder, 'mobile-390.png'), fullPage: true });
  await mobile.setViewportSize({ width: 375, height: 600 });
  await pageGeometry(mobile, 'Mobile 375x600');
  await verifyAndroidGeneral(browser, app.url, screenshotFolder, ensure, errors);
  await verifyAndroidGeneralDuringTurn(browser, app.url, ensure, errors);
  // Simulate the native Android /api/state response without relying on viewport width.
  const android = await browser.newPage({
    viewport: { width: 390, height: 844 }, deviceScaleFactor: 1, isMobile: true, hasTouch: true,
    userAgent: 'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36',
  });
  android.on('pageerror', error => errors.push(error.message));
  let syntheticMobileSkills = [];
  await android.route('**/api/state', async route => {
    const response = await route.fetch();
    const payload = await response.json();
    await route.fulfill({ response, json: { ...payload, platform: 'android', skillsAvailable: true, skills: syntheticMobileSkills } });
  });
  await android.route('**/api/mobile-skills', async route => {
    const body = route.request().postDataJSON();
    if (body.action === 'add') syntheticMobileSkills.push({
      id: 'qa-skill-1', name: body.name, enabled: false,
    });
    if (body.action === 'toggle')
      syntheticMobileSkills = syntheticMobileSkills.map(s => s.id === body.id ? { ...s, enabled: body.enabled } : s);
    if (body.action === 'delete')
      syntheticMobileSkills = syntheticMobileSkills.filter(s => s.id !== body.id);
    await route.fulfill({ json: { skills: syntheticMobileSkills } });
  });
  // Native panel APIs are exercised with synthetic data in browser QA.
  // Rust native-core independently tests the real bounded file/terminal API.
  await android.route('**/api/terminal', async route => {
    const body = route.request().postDataJSON();
    await route.fulfill({ json: { output: body.command === 'pwd' ? 'app://workspace/' : 'Unknown test command' } });
  });
  await android.route('**/api/workspace', async route => {
    const body = route.request().postDataJSON();
    await route.fulfill({ json: body.action === 'read' ?
      { content: 'Synthetic QA file', path: body.path, readOnly: true } :
      body.action === 'properties' ?
        { name: 'sample.txt', path: body.path, directory: false,
          bytes: 17, modifiedMs: 1700000000000, readOnly: true } :
        { path: body.path, readOnly: true, scope: 'Private application data',
          entries: [{ name: 'sample.txt', directory: false, bytes: 17 }] } });
  });
  await android.route('**/api/workspace-search', async route => {
    const query = route.request().postDataJSON().query;
    await route.fulfill({ json: { query, total: query === 'sample' ? 1 : 0,
      truncated: false, results: query === 'sample' ?
        [{ name: 'sample.txt', path: 'sample.txt', directory: false, bytes: 17 }] : [] } });
  });
  await android.goto(app.url);
  await android.waitForFunction(() => document.documentElement.classList.contains('native-android'));
  const compactUi = await android.evaluate(() => {
    const rect = selector => document.querySelector(selector).getBoundingClientRect();
    return {
      welcomeHidden: getComputedStyle(document.querySelector('.welcome')).display === 'none',
      topbarHeight: rect('.topbar').height,
      composerHeight: rect('.composer').height,
    };
  });
  ensure(compactUi.welcomeHidden, 'Android still shows the center logo and Connect your AI banner');
  ensure(compactUi.topbarHeight <= 46, 'Android header regressed from compact 46px height');
  ensure(compactUi.composerHeight < 95,
    `Android composer regressed from compact height: ${JSON.stringify(compactUi)}`);
  ensure(!(await android.locator('.hero-icon').isVisible()), 'Android center application logo remains visible');
  ensure(!(await android.locator('[data-open-connection]').isVisible()), 'Android Connect your AI card remains visible');
  ensure(!(await android.locator('[data-starter]').first().isVisible()), 'Android suggestion cards remain visible');
  ensure(await android.locator('#prompt').getAttribute('rows') === '1', 'Android composer should use a compact single-row default');
  ensure(await android.locator('#charLimit').count() === 0 &&
    await android.locator('#prompt').getAttribute('maxlength') === null,
    'Android composer must not impose a misleading 20k character UI limit');
  ensure(await android.locator('#send .send-arrow').isVisible() &&
    await android.locator('#androidComposerProgressRing').isVisible(),
    'Send arrow and verified-task progress ring must be mounted on Android');
  await android.locator('#prompt').fill('Write a second line');
  await android.locator('#prompt').press('Enter');
  ensure((await android.locator('#prompt').inputValue()).includes('\n'), 'Android Enter should insert a newline, not send');
  ensure(await android.locator('#threads button').count() === 0, 'Android Enter accidentally submitted a conversation');
  await android.locator('#prompt').fill('');
  ensure(!(await android.locator('.composer-heading').isVisible()), 'Android CURRENT ENGINE heading is visible');
  ensure(!(await android.locator('#newChatTop').isVisible()), 'Android top + New button is visible');
  ensure(!(await android.locator('#settingsToggle').isVisible()), 'Android top settings icon is visible');
  ensure(!(await android.locator('#provider').isVisible()), 'Android provider selector remains on chat surface');
  ensure(!(await android.locator('#model').isVisible()), 'Android model ID remains on chat surface');
  ensure(!(await android.locator('#apiKey').isVisible()), 'Android API key field remains on chat surface');
  ensure(await android.locator('#prompt').isVisible(), 'Android chat composer was incorrectly hidden');
  ensure(await android.locator('#speechDictation').isVisible() &&
    await desktop.locator('#speechDictation').isHidden(),
    'Android speech dictation must be present only in Android layout, not Windows UI');
  ensure(await android.locator('#androidComposeAdd').isVisible() &&
    await android.locator('#androidComposeModeLabel').innerText() === 'Normal' &&
    await android.locator('#androidComposeMore').isVisible() &&
    await android.locator('#androidComposePlan').isVisible() &&
    await android.locator('#androidComposerModel').isVisible() &&
    await desktop.locator('#androidComposeAdd').isHidden() &&
    await desktop.locator('#androidComposerModel').isHidden() &&
    await desktop.locator('#androidComposeMore').isHidden() &&
    await android.locator('#androidComposeMoreMenu').isHidden(),
    'COS composer mobile controls missing or leaking into desktop');
  ensure(await android.locator('.top-actions > #androidComposeMore').count() === 1 &&
    await android.locator('#androidComposeMore + #toolPanelToggle').count() === 1 &&
    await android.locator('.composer #androidComposeMore').count() === 0,
    'Android compact composer overflow must live beside the right sidebar/panel control');
  await android.locator('#androidComposeAdd').click();
  ensure(await android.locator('#androidComposeAddMenu').isVisible() &&
    await android.locator('[data-compose-add="folder"]').isVisible() &&
    await android.locator('[data-compose-add="skills"]').isVisible() &&
    (await android.locator('[data-compose-add="files"]').innerText()).includes('Add photos & files') &&
    (await android.locator('[data-compose-add="folder"]').innerText()).includes('Share a folder') &&
    (await android.locator('[data-compose-add="skills"]').innerText()).includes('Skills') &&
    (await android.locator('[data-compose-add="files"]').getAttribute('title')).includes('not yet supported'),
    'COS Add menu must have icons, exact familiar names and truthful attachment support');
  await android.screenshot({path:path.join(screenshotFolder,'native-android-cos-add-menu.png')});
  await android.locator('[data-compose-add="skills"]').click();
  ensure(await android.locator('#prompt').inputValue() === 'Please add this skill: ' &&
    await android.locator('#androidComposeAddMenu').isHidden() &&
    await android.locator('#extrasPanel').isHidden(),
    'Composer + Skills must write an editable skill request rather than opening old Skills UI');
  await android.locator('#prompt').fill('');
  await android.locator('#androidComposeMode').click();
  ensure(await android.locator('[data-compose-mode="loop"]').isDisabled() &&
    await android.locator('[data-compose-mode="goal"]').isEnabled() &&
    await android.locator('#androidBypassToggle').getAttribute('role') === 'switch' &&
    await android.locator('#androidBypassToggle').getAttribute('aria-checked') === 'false' &&
    (await android.locator('#androidBypassNotice').textContent()).includes('not active') &&
    (await android.locator('#androidBypassToggle').innerText()).trim() === '' &&
    (await android.locator('#androidBypassLabel').innerText()) === 'Bypass Permission',
    'Android Loop must be transparently unavailable until a native Loop backend exists');
  const bypassNetworkCalls = [];
  android.on('request', request => {
    if (/\/api\/(chat|chat-tool-review|mobile-goal-run|goal-tool-review|permissions)/.test(request.url()))
      bypassNetworkCalls.push(request.url());
  });
  await android.locator('#androidBypassToggle').click();
  const bypassMenuBounds = await android.evaluate(() => {
    const menu=document.getElementById('androidComposeModeMenu').getBoundingClientRect();
    const row=document.querySelector('.android-bypass-row').getBoundingClientRect();
    const label=document.getElementById('androidBypassLabel').getBoundingClientRect();
    const toggle=document.getElementById('androidBypassToggle').getBoundingClientRect();
    const track=document.querySelector('#androidBypassToggle .android-bypass-track').getBoundingClientRect();
    const style=getComputedStyle(document.getElementById('androidBypassToggle'));
    return {menuLeft:menu.left,menuRight:menu.right,rowLeft:row.left,rowRight:row.right,
      labelLeft:label.left,labelRight:label.right,toggleLeft:toggle.left,toggleRight:toggle.right,
      toggleWidth:toggle.width,toggleHeight:toggle.height,trackWidth:track.width,trackHeight:track.height,
      borderWidth:style.borderTopWidth,background:style.backgroundColor,boxShadow:style.boxShadow};
  });
  ensure(bypassMenuBounds.menuLeft >= 0 && bypassMenuBounds.menuRight <= 391 &&
    bypassMenuBounds.rowLeft >= bypassMenuBounds.menuLeft &&
    bypassMenuBounds.rowRight <= bypassMenuBounds.menuRight &&
    bypassMenuBounds.labelLeft >= bypassMenuBounds.menuLeft &&
    bypassMenuBounds.labelRight < bypassMenuBounds.toggleLeft &&
    bypassMenuBounds.toggleRight <= bypassMenuBounds.menuRight &&
    bypassMenuBounds.toggleWidth >= 43 && bypassMenuBounds.toggleWidth <= 45 &&
    bypassMenuBounds.toggleHeight >= 43 && bypassMenuBounds.toggleHeight <= 45 &&
    bypassMenuBounds.trackWidth >= 32 && bypassMenuBounds.trackWidth <= 40 &&
    bypassMenuBounds.trackHeight >= 18 && bypassMenuBounds.trackHeight <= 22 &&
    bypassMenuBounds.borderWidth === '0px' &&
    bypassMenuBounds.background === 'rgba(0, 0, 0, 0)' &&
    bypassMenuBounds.boxShadow === 'none',
    `Bypass must show only the switch, without an outer button box: ${JSON.stringify(bypassMenuBounds)}`);
  ensure(await android.locator('#androidBypassToggle').getAttribute('aria-checked') === 'true' &&
    (await android.locator('#androidBypassToggle').innerText()).trim() === '' &&
    (await android.locator('#androidBypassNotice').textContent()).includes('NO permissions are bypassed') &&
    await android.locator('#androidComposeModeMenu').isVisible() &&
    await android.locator('#androidComposeModeLabel').innerText() === 'Normal' &&
    await android.evaluate(() => localStorage.getItem('mcp-agent-ai-android-bypass-ui-only-v1')) === 'on' &&
    bypassNetworkCalls.length === 0,
    'Bypass On must be a visible UI-only preference with zero provider/tool/permission side effects');
  await android.screenshot({path:path.join(screenshotFolder,'native-android-bypass-ui-only-on.png')});
  await android.reload();
  await android.waitForFunction(() => document.documentElement.classList.contains('native-android'));
  await android.locator('#androidComposeMode').click();
  ensure(await android.locator('#androidBypassToggle').getAttribute('aria-checked') === 'true' &&
    (await android.locator('#androidBypassToggle').innerText()).trim() === '' &&
    (await android.locator('#androidBypassNotice').textContent()).includes('Backend pending') &&
    bypassNetworkCalls.length === 0,
    'UI-only Bypass preference should survive reload but must never grant permissions or call tools');
  await android.locator('#androidBypassToggle').click();
  ensure(await android.locator('#androidBypassToggle').getAttribute('aria-checked') === 'false' &&
    (await android.locator('#androidBypassToggle').innerText()).trim() === '' &&
    await android.evaluate(() => localStorage.getItem('mcp-agent-ai-android-bypass-ui-only-v1')) === 'off' &&
    bypassNetworkCalls.length === 0,
    'Bypass Off must persist without provider/tool/native approval side effects');
  await android.locator('[data-compose-mode="goal"]').click();
  ensure(await android.locator('#androidComposeModeLabel').innerText() === 'Goal',
    'Goal composer mode did not update');
  await android.locator('#androidComposeMode').click();
  await android.locator('[data-compose-mode="normal"]').click();
  await android.locator('#prompt').fill('Write a harmless editable plan');
  await android.locator('#androidComposePlan').click();
  ensure(await android.locator('#androidPlanEditor').isVisible() &&
    await android.locator('#androidComposePlan').getAttribute('aria-pressed') === 'true' &&
    await android.locator('#androidPlanObjective').inputValue() === 'Write a harmless editable plan' &&
    !(await android.locator('#toolViewSubagents').isVisible()),
    'Plan button must arm independent Plan editor, never navigate to Sub-agents');
  await android.setViewportSize({width:320,height:568});
  await android.waitForFunction(() => document.querySelector('.shell').getBoundingClientRect().bottom <= 569);
  const tinyPlan = await android.locator('#androidPlanEditor').boundingBox();
  ensure(!!tinyPlan && tinyPlan.x >= 0 && tinyPlan.x + tinyPlan.width <= 321 &&
    tinyPlan.y >= 0 && tinyPlan.y + tinyPlan.height <= 569,
    `Independent Plan editor must remain fully inside 320x568 Android viewport: ${JSON.stringify(tinyPlan)}`);
  await android.screenshot({path:path.join(screenshotFolder,'native-android-plan-editor-320.png')});
  await android.setViewportSize({width:390,height:844});
  await android.locator('#androidPlanSteps').fill('Inspect the files\nReview changes\nTest the result');
  await android.locator('#androidPlanUseDraft').click();
  ensure((await android.locator('#prompt').inputValue()).includes('2. Review changes') &&
    await android.locator('#androidPlanEditor').isHidden() &&
    await android.locator('#androidComposePlan').getAttribute('aria-pressed') === 'false',
    'Using a reviewed Plan must only create an unsent editable chat draft');
  await android.locator('#prompt').fill('');
  await android.locator('#androidComposeMore').click();
  ensure(await android.locator('#androidComposeMoreMenu').isVisible() &&
    await android.locator('#androidComposeShowReasoning').count() === 0 &&
    await android.locator('#androidComposeShowPlan').count() === 0 &&
    await android.locator('#androidRulesThisSession, #androidRulesAllSessions').count() === 0 &&
    await android.locator('#androidStandingRulesSave').innerText() === 'Set rules' &&
    await android.locator('#androidStandingRulesReset').innerText() === 'Reset rules' &&
    (await android.locator('#androidStandingRulesSession').innerText()) === 'Chat session: None' &&
    !(await android.locator('#androidComposeMoreMenu').innerText()).includes('Remind AI every request'),
    'Top-right overflow must show Set/Reset with no toggles and truthful None status');
  ensure(await android.locator('#androidStandingRulesText').isDisabled() &&
    (await android.locator('#androidStandingRulesStatus').innerText()).includes('saved chat'),
    'Browser preview must not pretend to persist native standing rules');
  await android.locator('#androidComposeMore').click();
  // Synthetic Tauri bridge tests explicit private read/save receipts only.
  // No provider requests, permissions, files or real account are accessed.
  await android.evaluate(() => {
    window.isTauri = true;
    window.__standingRulesNativeCalls = [];
    window.__standingRulesMock = {global:{enabled:false,text:''},session:{enabled:false,text:''}};
    window.__TAURI_INTERNALS__ = {invoke:async (command,args) => {
      if (command !== 'api_request' || args.route !== '/api/standing-rules')
        throw Error('Standing rules UI attempted unrelated native operation');
      window.__standingRulesNativeCalls.push(structuredClone(args.body));
      if (args.body.action === 'get') return structuredClone({sessionId:args.body.sessionId,
        global:window.__standingRulesMock.global,session:window.__standingRulesMock.session,
        legacy:{text:'Archived previous rule',wasEnabled:true,inactive:true}});
      if (args.body.action === 'save' && args.body.userInitiated === true &&
          ['global','session'].includes(args.body.scope) &&
          (args.body.scope !== 'session' || !!args.body.sessionId) &&
          typeof args.body.enabled === 'boolean' && typeof args.body.text === 'string') {
        window.__standingRulesMock[args.body.scope] = {enabled:args.body.enabled,text:args.body.text};
        return structuredClone({sessionId:args.body.sessionId,scope:args.body.scope,
          enabled:args.body.enabled,text:args.body.text});
      }
      throw Error('Standing rules request must be an explicit bounded action');
    }};
  });
  await android.locator('#androidComposeMore').click();
  await android.locator('#androidStandingRulesText').waitFor({state:'visible'});
  ensure(await android.locator('#androidStandingRulesText').isDisabled() &&
    await android.locator('#androidStandingRulesSave').isDisabled() &&
    await android.locator('#androidStandingRulesReset').isDisabled() &&
    (await android.locator('#androidStandingRulesSession').innerText()) === 'Chat session: None',
    'Unsent new chat must not acquire an invented ID or accept Rule mutations');
  ensure((await android.evaluate(() => window.__standingRulesNativeCalls)).length === 0,
    'Unbound chat must not request global rules from private storage');
  await android.locator('#androidComposeMore').click();
  await android.locator('#androidComposeMore').click();
  ensure(await android.locator('#androidStandingRulesReset').isDisabled(),
    'Reset must remain disabled until a real saved chat exists');
  await android.screenshot({path:path.join(screenshotFolder,'native-android-standing-rules-menu.png')});
  await android.locator('#androidComposeMore').click();
  await android.evaluate(() => {delete window.__TAURI_INTERNALS__;delete window.isTauri;});
  await android.locator('#androidComposeMore').click();
  ensure(await android.locator('#androidStandingRulesSession').innerText() === 'Chat session: None',
    'Standing rules session status must remain truthful after closing/reopening');
  await android.locator('#androidComposeMore').click();
  ensure(await android.locator('#androidComposeMoreMenu').isHidden(),
    'Android top-right overflow menu did not close');
  ensure(await android.locator('#sidebarToggle').isVisible(), 'Android sidebar toggle was incorrectly hidden');
  ensure(await android.locator('#extrasToggle, #extrasMenu, #extrasBackdrop').count() === 0,
    'Duplicate extension menu is still mounted in Android DOM');
  ensure(!(await desktop.locator('#extrasToggle').isVisible()), 'Windows must not show the Android extensions icon');
  await android.evaluate(() => {
    window.__searchFocusLeft = null;
    document.getElementById('sidebarSearch').addEventListener('focus', () => {
      window.__searchFocusLeft = document.getElementById('sidebarSearch').getBoundingClientRect().left;
    }, { once: true });
  });
  await android.locator('#sidebarQuickSearch').click();
  ensure(await android.locator('#androidViewMenu').isVisible() &&
    await android.locator('#sidebarQuickSearch').getAttribute('aria-expanded') === 'true',
    'Three-line button did not open COS-style View menu');
  ensure(!(await android.locator('#sidebar').evaluate(node => node.classList.contains('open'))),
    'Three-line View button wrongly opened Android sidebar');
  ensure(await android.evaluate(() => document.activeElement?.id !== 'sidebarSearch' && window.__searchFocusLeft === null),
    'Three-line View button unexpectedly focused Search');
  ensure(await android.locator('#androidViewMenu [role="menuitem"]').count() === 7 &&
    await android.locator('[data-view-action="shortcuts"]').count() === 0 &&
    await android.locator('#androidShortcutsDialog').count() === 0,
    'Android standalone View menu must omit desktop keyboard shortcuts');
  ensure(await android.locator('[data-view-action="browser"]').isDisabled(),
    'Android missing browser runtime must be explicitly unavailable');
  ensure(await android.locator('[data-view-action="browser-tools"]').isDisabled(),
    'Advanced browser controls must also stay disabled without a native runtime');
  ensure((await android.locator('[data-view-action="browser"]').innerText()).includes('Android') &&
    await android.locator('[data-view-action="browser"]').getAttribute('aria-disabled') === 'true',
    'Android browser preview must remain disabled without the native runtime');
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-view-menu.png'), fullPage: true });
  await android.locator('[data-view-action="zoom-in"]').click();
  ensure(await android.locator('#androidViewMenu').isVisible() &&
    (await android.locator('#viewZoomPercent').innerText()) === '110%',
    'View Zoom in must work and keep the menu open with updated percent');
  ensure(await android.evaluate(() => parseFloat(document.documentElement.style.zoom) === 110),
    'View Zoom in did not actually adjust Android UI scale');
  ensure(await android.evaluate(() =>
    document.documentElement.scrollWidth <= window.innerWidth + 1),
    'Zoom in creates horizontal overflow in Android WebView');
  await android.locator('[data-view-action="zoom-out"]').click();
  ensure((await android.locator('#viewZoomPercent').innerText()) === '100%',
    'View Zoom out did not restore previous level');
  await android.locator('[data-view-action="zoom-reset"]').click();
  ensure(await android.evaluate(() => document.documentElement.style.zoom === '100%'),
    'Actual size failed to restore 100%');
  await android.locator('[data-view-action="pets"]').click();
  ensure(await android.locator('#petBuddy').isVisible(),
    'View Show pets did not enable the existing Android Pocket Bot');
  ensure(!(await android.locator('#androidViewMenu').isVisible()),
    'View Show pets did not dismiss menu');
  await android.locator('#sidebarQuickSearch').click();
  ensure((await android.locator('#viewPetsLabel').innerText()) === 'Hide pets',
    'View Pets menu label failed to reflect the real toggle state');
  await android.locator('[data-view-action="pets"]').click();
  ensure(!(await android.locator('#petBuddy').isVisible()),
    'View Hide pets did not hide Pocket Bot');
  await android.locator('#sidebarQuickSearch').click();
  await android.locator('[data-view-action="search"]').click();
  ensure(await android.locator('#sidebar').evaluate(node => node.classList.contains('open')) &&
    !(await android.locator('#androidViewMenu').isVisible()),
    'Search chats action must open real Android sidebar and close View menu');
  await android.waitForFunction(() => {
    const side = document.getElementById('sidebar').getBoundingClientRect();
    const main = document.querySelector('.main').getBoundingClientRect();
    return Math.abs(side.width - innerWidth) <= 2 &&
      Math.abs(main.width - innerWidth) <= 2;
  });
  const gridLayout = await android.evaluate(() => {
    const side = document.getElementById('sidebar').getBoundingClientRect();
    const main = document.querySelector('.main').getBoundingClientRect();
    const top = document.querySelector('.topbar').getBoundingClientRect();
    return { sideRight: side.right, sideLeft: side.left, mainLeft: main.left,
      mainWidth: main.width, topLeft: top.left, topWidth: top.width,
      mainHidden: getComputedStyle(document.querySelector('.main')).visibility === 'hidden',
      mainInert: document.querySelector('.main').inert,
      viewport: innerWidth, scrollWidth: document.documentElement.scrollWidth,
      open: document.querySelector('.shell').classList.contains('android-sidebar-open') };
  });
  ensure(gridLayout.open && gridLayout.sideLeft >= -1 &&
    Math.abs(gridLayout.sideRight - gridLayout.viewport) <= 2 &&
    Math.abs(gridLayout.mainWidth - gridLayout.viewport) <= 2 &&
    gridLayout.mainHidden && gridLayout.mainInert &&
    Math.abs(gridLayout.topLeft) <= 1 && gridLayout.topWidth >= gridLayout.viewport - 1 &&
    gridLayout.scrollWidth <= gridLayout.viewport + 1,
    'Android full-screen sidebar must cover the chat without squeezing the composer: ' + JSON.stringify(gridLayout));
  ensure(await android.locator('#androidSidebarClose, #androidSidebarViewMenu, .android-sidebar-top-actions').count() === 0 &&
    await android.locator('#sidebarToggle').count() === 1 &&
    await android.locator('#sidebarQuickSearch').count() === 1 &&
    await android.locator('#sidebarSearchButton').count() === 1 &&
    await android.locator('#sidebarSearch').isVisible() &&
    await android.locator('#sidebarBackdrop').isHidden(),
    'Android duplicated sidebar/header controls or overlay; COS owns one topbar and one brand search');
  ensure(await android.evaluate(() =>
    document.querySelector('.topbar').parentElement === document.querySelector('.shell') &&
    !document.getElementById('sidebar').inert &&
    document.getElementById('sidebarToggle').getAttribute('aria-expanded') === 'true'),
    'Android must reuse one actual topbar, preserve focusability and reflect sidebar state');
  for (const width of [320, 360, 390]) {
    await android.setViewportSize({ width, height: 844 });
    await android.waitForFunction(() => {
      const side = document.getElementById('sidebar').getBoundingClientRect();
      const main = document.querySelector('.main').getBoundingClientRect();
      return side.left >= -1 && Math.abs(side.right - innerWidth) <= 2 &&
        Math.abs(main.width - innerWidth) <= 2;
    });
    const bounds = await android.evaluate(() => ({
      viewport: innerWidth,
      sidebar: document.getElementById('sidebar').getBoundingClientRect().width,
      main: document.querySelector('.main').getBoundingClientRect().width,
      chatInert: document.querySelector('.main').inert,
      sendHidden: getComputedStyle(document.getElementById('send')).visibility === 'hidden',
      root: document.documentElement.scrollWidth,
    }));
    ensure(Math.abs(bounds.sidebar - width) <= 2 && Math.abs(bounds.main - width) <= 2 &&
      bounds.chatInert && bounds.sendHidden && bounds.root <= width + 1,
      `Android navigation must occupy the full ${width}px screen, hiding Send: ${JSON.stringify(bounds)}`);
  }
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-sidebar-full-screen.png'), fullPage: true });
  ensure(await android.evaluate(() => document.activeElement?.id === 'sidebarSearch'),
    'Explicit View Search chats action did not focus the search field');
  ensure(await android.evaluate(() => window.__searchFocusLeft >= 0),
    'View Search chats focused the search field while its drawer was off-screen');
  ensure(await android.evaluate(() => document.documentElement.dataset.inputMode === 'pointer'),
    'Android touch modality was not captured');
  const tapAppearance = await android.locator('#sidebarQuickSearch').evaluate(node => ({
    tap: getComputedStyle(node).webkitTapHighlightColor,
    outline: getComputedStyle(node).outlineStyle,
  }));
  ensure(tapAppearance.tap === 'rgba(0, 0, 0, 0)' || tapAppearance.tap === 'transparent',
    'Android still has blue WebView tap highlighting');
  ensure(tapAppearance.outline === 'none', 'Tapped Android button retains a blue focus outline');
  await android.keyboard.press('Tab');
  ensure(await android.evaluate(() => document.documentElement.dataset.inputMode === 'keyboard'),
    'Keyboard navigation does not restore focus indicator mode');
  await android.locator('#sidebarToggle').click();
  await android.waitForFunction(() => !document.querySelector('.main').inert &&
    getComputedStyle(document.querySelector('.main')).visibility === 'visible');
  ensure(!(await android.locator('.shell').evaluate(node => node.classList.contains('android-sidebar-open'))),
    'Closing Android full-screen navigation must restore the chat');
  ensure(await android.locator('#sidebar').evaluate(node => node.inert) &&
    await android.locator('#sidebarToggle').getAttribute('aria-expanded') === 'false',
    'Closed Android sidebar must be inert and report collapsed aria state');
  ensure(await android.evaluate(() => document.activeElement?.id !== 'sidebarSearch'),
    'Closing Android sidebar left the Search input focused');
  // Android touch navigation should respond to an easy short gesture from
  // the left half of chat and from anywhere inside the open drawer.
  const swipeAndroid = async (target, x1, y1, x2, y2) => android.evaluate(({target,x1,y1,x2,y2}) => {
    const element = document.querySelector(target);
    const from = new Touch({identifier:7,target:element,clientX:x1,clientY:y1});
    const mid = new Touch({identifier:7,target:element,
      clientX:(x1+x2)/2,clientY:(y1+y2)/2});
    const to = new Touch({identifier:7,target:element,clientX:x2,clientY:y2});
    element.dispatchEvent(new TouchEvent('touchstart', {bubbles:true,touches:[from],
      targetTouches:[from],changedTouches:[from]}));
    element.dispatchEvent(new TouchEvent('touchmove', {bubbles:true,cancelable:true,touches:[mid],
      targetTouches:[mid],changedTouches:[mid]}));
    element.dispatchEvent(new TouchEvent('touchmove', {bubbles:true,cancelable:true,touches:[to],
      targetTouches:[to],changedTouches:[to]}));
    const settings = !document.getElementById('settingsPanel').classList.contains('hidden');
    const drawer = document.getElementById(settings ? 'androidSettingsNav' : 'sidebar');
    const preview = {
      dragging: settings ? document.getElementById('settingsPanel').classList.contains('android-settings-swiping') :
        document.querySelector('.shell').classList.contains('android-sidebar-dragging'),
      transform: getComputedStyle(drawer).transform,
    };
    element.dispatchEvent(new TouchEvent('touchend', {bubbles:true,touches:[],
      targetTouches:[],changedTouches:[to]}));
    return preview;
  }, {target,x1,y1,x2,y2});
  await swipeAndroid('.main', 55, 240, 60, 420);
  ensure(await android.locator('#sidebar').evaluate(node => node.inert),
    'Vertical chat scroll was mistaken for a sidebar swipe');
  await swipeAndroid('.main', 120, 240, 148, 245);
  ensure(await android.locator('#sidebar').evaluate(node => node.inert),
    'Tiny incidental sideways movement opened chat navigation');
  await swipeAndroid('#prompt', 40, 550, 175, 550);
  ensure(await android.locator('#sidebar').evaluate(node => node.inert),
    'Swipe across chat input must never steal text editing');
  const chatSwipePreview = await swipeAndroid('.main', 110, 240, 176, 252);
  ensure(chatSwipePreview.dragging && chatSwipePreview.transform !== 'none',
    'Chat sidebar must visibly follow the finger before release');
  ensure(await android.locator('#sidebarToggle').getAttribute('aria-expanded') === 'true' &&
    await android.locator('#sidebar').evaluate(node => !node.inert),
    'Short rightward swipe from Android chat did not open the sidebar');
  await swipeAndroid('#sidebar', 218, 230, 155, 243);
  ensure(await android.locator('#sidebarToggle').getAttribute('aria-expanded') === 'false' &&
    await android.locator('#sidebar').evaluate(node => node.inert),
    'Short leftward swipe anywhere in Android sidebar did not restore chat');
  ensure(await android.evaluate(() => !document.querySelector('.shell').classList.contains('android-sidebar-dragging')),
    'Swipe preview must be cleaned up after sidebar settles');
  // Exercise Chromium's actual touch dispatch path, not just handcrafted DOM
  // TouchEvents: WebView can cancel gestures during native pan recognition.
  const cdpTouch = await android.context().newCDPSession(android);
  const realTouchSwipe = async (x1, y1, x2, y2) => {
    const point = (x, y) => ({x, y, id: 1, radiusX: 4, radiusY: 4, force: 1});
    await cdpTouch.send('Input.dispatchTouchEvent', {
      type: 'touchStart', touchPoints: [point(x1, y1)]
    });
    for (let i = 1; i <= 5; i++) {
      await cdpTouch.send('Input.dispatchTouchEvent', {
        type: 'touchMove', touchPoints: [point(x1 + (x2 - x1) * i / 5,
          y1 + (y2 - y1) * i / 5)]
      });
    }
    await cdpTouch.send('Input.dispatchTouchEvent', {type: 'touchEnd', touchPoints: []});
  };
  await realTouchSwipe(118, 255, 169, 262);
  ensure(await android.locator('#sidebar').evaluate(node => !node.inert),
    'Real Chromium rightward touch gesture failed to open chat sidebar');
  await realTouchSwipe(238, 255, 187, 263);
  ensure(await android.locator('#sidebar').evaluate(node => node.inert),
    'Real Chromium leftward touch gesture failed to close chat sidebar');
  await realTouchSwipe(115, 245, 123, 400);
  ensure(await android.locator('#sidebar').evaluate(node => node.inert),
    'Real Chromium vertical pan opened the chat sidebar accidentally');
  await cdpTouch.detach();
  for (const width of [320, 360, 390]) {
    await android.setViewportSize({ width, height: 760 });
    const geometry = await android.evaluate(() => {
      const rect = id => document.getElementById(id).getBoundingClientRect();
      const composer = rect('chatForm');
      const send = rect('send');
      const mic = rect('speechDictation');
      return { viewport: innerWidth, composerLeft: composer.left, composerRight: composer.right,
        sendLeft: send.left, sendRight: send.right, micRight: mic.right,
        root: document.documentElement.scrollWidth, chatInert: document.querySelector('.main').inert };
    });
    ensure(!geometry.chatInert && geometry.root <= width + 1 &&
      geometry.composerLeft >= -1 && geometry.composerRight <= width + 1 &&
      geometry.sendLeft >= geometry.composerLeft - 1 &&
      geometry.sendRight <= geometry.composerRight - 1 &&
      geometry.micRight <= geometry.sendLeft + 1,
      `Android Send/mic escape composer at ${width}px after sidebar closes: ${JSON.stringify(geometry)}`);
  }
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-composer-send-contained.png'), fullPage: true });
  await android.keyboard.press('Control+k');
  ensure(await android.locator('#sidebar').evaluate(node => node.classList.contains('open')) &&
    await android.evaluate(() => document.activeElement?.id === 'sidebarSearch'),
    'Ctrl+K did not open Search chats as in COS View menu');
  await android.keyboard.press('Escape');
  await android.locator('#sidebarToggle').click();
  await android.locator('#sidebarToggle').click();
  await android.waitForFunction(() => Math.abs(document.querySelector('.main').getBoundingClientRect().left) <= 1);
  ensure(!(await android.locator('#sidebar').evaluate(node => node.classList.contains('open'))),
    'Canonical sidebar toggle did not restore chat');
  await android.locator('#sidebarToggle').click();
  await android.locator('#sidebarQuickSearch').click();
  ensure(await android.locator('#androidViewMenu').isVisible() &&
    !(await android.locator('#sidebar').evaluate(node => node.classList.contains('open'))),
    'Canonical topbar View button did not switch back to real COS View menu');
  await android.keyboard.press('Escape');
  await android.locator('#sidebarToggle').click();
  await android.locator('[data-sidebar-extra="plugins"]').click();
  ensure(await android.locator('#androidPluginsSettings').isVisible(),
    'Android Plugins must open the dedicated COS-style Plugins setup');
  ensure(await android.locator('#settingsPanel').getAttribute('data-settings-view') === 'plugins',
    'Android Plugins must have a separate Settings route from Core and remote MCP');
  ensure((await android.locator('#androidPluginsState').innerText()).includes('Status unavailable'),
    'Missing native Plugins tunnel must not be invented as connected');
  ensure(await android.locator('#androidPluginsRefresh').count() === 0,
    'Plugins status must check the native backend automatically, without a Refresh button');
  ensure((await android.locator('#androidPluginsToolCount').innerText()).includes('0 available tools'),
    'Unverified Android Plugins cannot claim tool availability');
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-plugins.png'), fullPage: true });
  await android.locator('#androidSettingsBack').click();
  await android.locator('#settingsClose').click();
  await android.locator('#sidebarToggle').click();
  await android.locator('[data-sidebar-extra="skills"]').click();
  ensure(await android.locator('#extrasSkills').isVisible(), 'Skills page unavailable');
  await android.waitForFunction(() => document.getElementById('sidebar').inert && document.querySelector('.main').getBoundingClientRect().left <= 1);
  ensure((await android.locator('#mobileSkillsList').innerText()).includes('No custom skills'), 'Skills empty state incorrect');
  await android.locator('#mobileSkillName').fill('Review code');
  await android.locator('#mobileSkillInstructions').fill('Review tests and edge cases.');
  await android.locator('#mobileSkillForm button').click();
  await android.locator('#mobileSkillCount').getByText('1').waitFor();
  ensure(await android.locator('#mobileSkillsList').getByText('Review code').isVisible(), 'Skill creation not reflected in UI');
  ensure(!(await android.locator('#mobileSkillsList input').first().isChecked()), 'New skills must default disabled');
  await android.locator('#mobileSkillsSearch').fill('unmatched');
  ensure((await android.locator('#mobileSkillsList').innerText()).includes('No skills matching'),
    'Skills search missing no-results state');
  await android.locator('#mobileSkillsSearch').fill('review');
  ensure(await android.locator('#mobileSkillsList').getByText('Review code').isVisible(),
    'Skills search failed to match stored skill');
  await android.locator('#mobileSkillsSearch').fill('');
  await android.locator('#mobileSkillsList input').first().check();
  await android.waitForFunction(() => document.querySelector('#mobileSkillsList input')?.checked);
  ensure(syntheticMobileSkills[0]?.enabled === true, 'Skill toggle failed to reach backend');
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-skills.png'), fullPage: true });
  await android.locator('#mobileSkillsList button').first().click();
  await android.waitForFunction(() => document.getElementById('mobileSkillCount').textContent === '0');
  ensure(syntheticMobileSkills.length === 0, 'Skill removal did not reach backend');
  await android.locator('#extrasClose').click();
  await android.locator('#sidebarToggle').click();
  await android.locator('[data-sidebar-extra="pets"]').click();
  ensure(await android.locator('#extrasPets').isVisible(), 'Pets page unavailable');
  await android.waitForFunction(() => document.getElementById('sidebar').inert && document.querySelector('.main').getBoundingClientRect().left <= 1);
  ensure((await android.locator('.pet-preview').boundingBox()).height <= 180, 'Android Pets preview too tall');
  ensure(!(await android.locator('#petBuddy').isVisible()), 'Pet must start disabled');
  await android.locator('#mobilePetEnabled').check();
  ensure(await android.locator('#petBuddy').isVisible(), 'Pet enabled but not displayed');
  await android.locator('#mobilePetPosition').selectOption('left');
  ensure(await android.locator('#petBuddy').getAttribute('data-position') === 'left' &&
    await android.evaluate(() => localStorage.getItem('mcp-mobile-pocket-bot-position-v1') === 'left'),
    'Pocket Bot left-side selection not applied or stored');
  ensure(await android.evaluate(() => localStorage.getItem('mcp-mobile-pocket-bot-v1')) === 'on', 'Pet preference not stored');
  await android.waitForTimeout(200);
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-pets.png'), fullPage: true });
  await android.locator('#extrasClose').click();
  ensure(await android.locator('#petBuddy').isVisible(), 'Pet companion missing from chat after closing Pets');
  await android.locator('#petBuddy').click();
  ensure(await android.locator('#extrasPets').isVisible(), 'Tapping companion did not reopen Pet settings');
  await android.locator('#mobilePetEnabled').uncheck();
  ensure(!(await android.locator('#petBuddy').isVisible()), 'Pet did not disable');
  await android.locator('#extrasClose').click();
  ensure(await android.locator('#prompt').isVisible(), 'Chat did not recover from extensions');
  ensure(await android.locator('#toolPanelToggle').isVisible(), 'Android top-right panel button is missing');
  ensure(!(await desktop.locator('#toolPanelToggle').isVisible()), 'Desktop must not show Android panel button');
  await android.locator('#toolPanelToggle').click();
  ensure(await android.locator('#toolPanelToggle').getAttribute('aria-expanded') === 'true', 'Panel menu aria-expanded missing');
  ensure(await android.locator('#toolPanelMenu').isVisible(), 'Android workspace menu did not open');
  ensure(await android.locator('#toolPanelMenu [role="menuitem"]').count() === 4, 'Missing Review/Terminal/Files/Sub-agents items');
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-panel-menu.png'), fullPage: true });
  await android.locator('#toolPanelBackdrop').click({ position: { x: 5, y: 230 } });
  ensure(!(await android.locator('#toolPanelMenu').isVisible()), 'Workspace menu did not dismiss on outside tap');
  await android.locator('#toolPanelToggle').click();
  await android.locator('[data-tool-view="review"]').click();
  ensure(await android.locator('#toolReview').isVisible(), 'Review panel failed to open');
  ensure(await android.locator('[data-work-tab="review"]').getAttribute('aria-current') === 'true',
    'COS-style workspace Review tab not selected');
  await android.locator('[data-work-tab="terminal"]').click();
  ensure(await android.locator('#toolTerminal').isVisible(), 'Switching to Terminal tab failed');
  await android.locator('#androidPtyWorkspace').waitFor({state:'visible'});
  ensure(await android.locator('#androidPtyNew').isDisabled() &&
    await android.locator('#terminalForm').isHidden(),
    'Android preview must not offer a fake PTY without its native backend');
  await android.locator('[data-work-tab="review"]').click();
  ensure((await android.locator('#reviewSummary').innerText()).includes('No recorded conversations yet'), 'Review empty state is missing');
  await android.locator('#toolViewClose').click();
  await android.locator('#toolPanelToggle').click();
  await android.locator('[data-tool-view="terminal"]').click();
  ensure(await android.locator('#toolTerminal').isVisible(), 'Terminal panel failed to open');
  ensure(await android.locator('#androidPtyWorkspace').isVisible() &&
    await android.locator('#androidPtyNew').isDisabled() &&
    await android.locator('#terminalInput').isHidden(),
    'Browser-only Android preview must not display a simulated shell');
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-terminal.png'), fullPage: true });
  await android.keyboard.press('Escape');
  ensure(!(await android.locator('#toolViewPanel').isVisible()), 'Escape did not close workspace panel');
  await android.locator('#toolPanelToggle').click();
  await android.locator('[data-tool-view="files"]').click();
  await android.locator('#filesList .files-row').first().waitFor();
  ensure(await android.locator('#filesList .files-row').count() === 1, 'Files did not show native folder contents');
  await android.locator('#filesList .files-row > button').first().click();
  await android.getByText('Synthetic QA file').waitFor();
  ensure(await android.locator('#filesPreview').isVisible(), 'Files text preview missing');
  await android.locator('#filesList .files-row-info').click();
  await android.locator('#filesInfoBody').getByText('17 bytes').waitFor();
  ensure(await android.locator('#filesInfo').isVisible() &&
    (await android.locator('#filesInfo').innerText()).includes('17 bytes') &&
    (await android.locator('#filesInfo').innerText()).includes('app://workspace/sample.txt'),
    'Native Files properties not displayed for selected private file');
  await android.locator('#filesInfoClose').click();
  await android.locator('#filesSearchQuery').fill('sample');
  await android.locator('#filesSearchForm button[type="submit"]').click();
  await android.getByText('1 match(es) · app-private names only').waitFor();
  ensure(await android.locator('#filesList .files-row').count() === 1,
    'Private filename search did not render result');
  await android.locator('#filesSearchQuery').fill('not-a-match');
  await android.locator('#filesSearchForm button[type="submit"]').click();
  await android.locator('#filesList').getByText('No matching files or folders.').waitFor();
  ensure((await android.locator('#filesList').innerText()).includes('No matching'),
    'Private filename search did not show no-matches state');
  await android.locator('#filesSearchClear').click();
  await android.locator('#filesList .files-row').first().waitFor();
  await android.locator('#filesSourceSelected').click();
  ensure(await android.locator('#filesExternalView').isVisible() &&
    await android.locator('#filesChooseFolder').isDisabled() &&
    (await android.locator('#filesExternalStatus').innerText()).includes('preview'),
    'Browser preview must not open Android folder picker without native bridge');
  await android.locator('#filesSourcePrivate').click();
  await android.locator('#filesList .files-row').first().waitFor();
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-files.png'), fullPage: true });
  await android.locator('#toolViewClose').click();
  await android.locator('#toolPanelToggle').click();
  await android.locator('[data-tool-view="subagents"]').click();
  ensure(await android.locator('#toolSubagents').isVisible(), 'Sub-agents panel failed to open');
  ensure(await android.locator('#androidPrimeActiveCount').isVisible() &&
    (await android.locator('#toolViewPanel').getAttribute('data-workspace-panel')) === 'subagents',
    'Sub-agents must open the real right-hand Prime activity drawer');
  await android.locator('#androidPrimeAdvanced').evaluate(node => { node.open = true; });
  ensure(await android.getByText('Local workspace workers').isVisible(), 'Bounded local worker page missing');
  ensure((await android.locator('#toolSubagents').innerText()).includes('not separate AI models') &&
    await android.locator('#localWorkerForm button[type="submit"]').isDisabled(),
    'Browser preview must not claim full COS AI sub-agents or execute a fake worker');
  await android.locator('#toolViewClose').click();
  ensure(await android.locator('#prompt').isVisible(), 'Closing workspace panel did not restore chat');
  await android.locator('#sidebarToggle').click();
  await android.waitForFunction(() => Math.abs(document.getElementById('sidebar').getBoundingClientRect().width - innerWidth) <= 2);
  ensure(await android.locator('#newChat').isVisible(), 'Android sidebar New chat action missing');
  ensure((await android.locator('#newChat').innerText()).includes('New chat'), 'Android sidebar must use the New chat label');
  ensure(!(await android.locator('.new-chat-desktop').isVisible()), 'Android still shows New conversation');
  ensure(await android.locator('#sidebarSearchButton').isVisible() &&
    await android.locator('#sidebarSearch').isHidden(), 'COS brand search must be one icon, not a permanent second search row');
  ensure(await android.locator('[data-sidebar-extra]').count() === 3, 'Android fixed Plugins/Skills/Pets navigation is missing');
  ensure(await android.locator('#projectsToggle').isVisible(), 'Android Projects heading missing');
  ensure(await android.locator('#chatsToggle').isVisible(), 'Android Chats accordion missing');
  ensure(await android.locator('#chatsRefresh').isVisible(), 'Android Chats refresh action missing');
  ensure(await android.locator('#sidebarConnectionStatus').isVisible(),
    'Connection status missing beside Android Settings');
  ensure(await android.locator('#sidebarConnectionStatus').getAttribute('data-status') === 'disconnected',
    'Android incorrectly claims a verified connection at startup');
  const footerStatusLayout = await android.evaluate(() => {
    const settings = document.getElementById('sidebarCustomize').getBoundingClientRect();
    const badge = document.getElementById('sidebarConnectionStatus').getBoundingClientRect();
    const footer = document.querySelector('.android-sidebar-footer');
    const footerStyle = getComputedStyle(footer);
    return { settingsRight: settings.right, badgeRight: badge.right,
      badgeLeft: badge.left, height: badge.height,
      parent: footer.tagName, footerDisplay: footerStyle.display,
      footerDirection:footerStyle.flexDirection, footerWidth:footer.getBoundingClientRect().width,
      settingsWidth:settings.width, badgeTop:badge.top,settingsTop:settings.top };
  });
  ensure(footerStatusLayout.badgeLeft >= footerStatusLayout.settingsRight - 1 &&
    footerStatusLayout.badgeRight <= 391 &&
    footerStatusLayout.height >= 40 && footerStatusLayout.height <= 50,
    'Independent connection-status button must sit at the right of Settings with a touch target: ' + JSON.stringify(footerStatusLayout));
  const footerAnchor = await android.evaluate(() => {
    const sidebar=document.getElementById('sidebar').getBoundingClientRect();
    const footer=document.querySelector('.android-sidebar-footer').getBoundingClientRect();
    const scroll=document.getElementById('sidebarScrollRegion').getBoundingClientRect();
    return {sidebarBottom:sidebar.bottom,footerTop:footer.top,footerBottom:footer.bottom,
      scrollBottom:scroll.bottom,scrollTop:scroll.top};
  });
  ensure(footerAnchor.footerBottom <= footerAnchor.sidebarBottom + 1 &&
    footerAnchor.footerBottom >= footerAnchor.sidebarBottom - 22 &&
    footerAnchor.footerTop >= footerAnchor.scrollBottom - 1 &&
    footerAnchor.scrollBottom > footerAnchor.scrollTop + 100,
    `Settings and connection footer MUST stay below scrollable chats at sidebar bottom: ${JSON.stringify(footerAnchor)}`);
  await android.locator('#sidebarConnectionStatus').click();
  const connectionPopoverVisible = await android.locator('#androidConnectionStatusPopover').isVisible();
  const connectionPopoverDetail = await android.locator('#androidConnectionStatusDetail').innerText();
  ensure(connectionPopoverVisible && connectionPopoverDetail.includes('No ChatGPT assistant reply verified yet'),
    'Connection badge must open an honest provider status popover: ' +
      JSON.stringify({connectionPopoverVisible, connectionPopoverDetail, errors:errors.slice(-3)}));
  const popoverAnchor = await android.evaluate(() => {
    const footer=document.querySelector('.android-sidebar-footer').getBoundingClientRect();
    const pop=document.getElementById('androidConnectionStatusPopover').getBoundingClientRect();
    return {popoverBottom:pop.bottom,popoverRight:pop.right,footerTop:footer.top,footerRight:footer.right};
  });
  ensure(popoverAnchor.popoverBottom < popoverAnchor.footerTop &&
    popoverAnchor.popoverRight <= popoverAnchor.footerRight + 1,
    `Connection popover must appear above its bottom-right control, not at top of sidebar: ${JSON.stringify(popoverAnchor)}`);
  await android.locator('#androidConnectionClose').click();
  ensure(await android.locator('#androidConnectionStatusPopover').isHidden(),
    'Connection status popover did not close');
  ensure(await android.locator('#chatsToggle').getAttribute('aria-expanded') === 'true',
    'Chats accordion must default to expanded');
  const accordionGeometry = await android.evaluate(() => {
    const projects = document.querySelector('.sidebar-group-heading').getBoundingClientRect();
    const chats = document.querySelector('.sidebar-scroll-region .section-title').getBoundingClientRect();
    const style = selector => {
      const el = document.querySelector(selector);
      const css = getComputedStyle(el);
      return { background: css.backgroundColor, border: css.borderTopColor, radius: css.borderRadius };
    };
    return { projects: { x: projects.x, width: projects.width, height: projects.height },
      chats: { x: chats.x, width: chats.width, height: chats.height },
      projectStyle: style('.sidebar-group-heading'), chatStyle: style('.sidebar-scroll-region .section-title') };
  });
  ensure(Math.abs(accordionGeometry.projects.x - accordionGeometry.chats.x) <= 1 &&
    Math.abs(accordionGeometry.projects.width - accordionGeometry.chats.width) <= 1 &&
    Math.abs(accordionGeometry.projects.height - accordionGeometry.chats.height) <= 1,
    'Android Projects and Chats accordion boxes do not match');
  ensure(JSON.stringify(accordionGeometry.projectStyle) === JSON.stringify(accordionGeometry.chatStyle),
    'Android Projects and Chats headers use inconsistent card styling');
  ensure(await android.locator('#chatsToggle svg path').count() === 1 &&
    await android.locator('#projectsToggle svg path').count() === 1,
    'Android accordion icons must use one SVG chevron family');
  await android.locator('#chatsToggle').click();
  ensure(await android.locator('#chatsToggle').getAttribute('aria-expanded') === 'false' &&
    !(await android.locator('#threads').isVisible()),
    'Chats collapse did not hide history or update aria-expanded');
  ensure(await android.locator('#projectsToggle').getAttribute('aria-expanded') === 'true',
    'Collapsing Chats changed Projects expansion state');
  await android.locator('#chatsToggle').press('Enter');
  ensure(await android.locator('#chatsToggle').getAttribute('aria-expanded') === 'true' &&
    await android.locator('#threads').isVisible(),
    'Keyboard Enter did not expand Chats history');
  await android.locator('#chatsToggle').evaluate(node => node.blur());
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-sidebar.png'), fullPage: true });
  await android.locator('[data-sidebar-extra="skills"]').click();
  ensure(await android.locator('#extrasSkills').isVisible(), 'Android sidebar Skills entry did not open Skills');
  await android.locator('#extrasClose').click();
  await android.locator('#sidebarQuickSearch').click();
  ensure(await android.locator('#androidViewMenu').isVisible() &&
    !(await android.locator('#sidebar').evaluate(node => node.classList.contains('open'))),
    'Second top-left icon must open View menu and not sidebar');
  await android.keyboard.press('Escape');
  ensure(!(await android.locator('#androidViewMenu').isVisible()), 'Escape must close View menu');
  await android.locator('#sidebarToggle').click();
  ensure(await android.locator('#sidebarCustomize').isVisible(), 'Android Connections & tools entry missing');
  await android.locator('#sidebarCustomize').click();
  ensure(await android.locator('#settingsPanel').isVisible(), 'Android Connections panel did not open');
  await android.waitForFunction(() => document.getElementById('sidebar').inert && document.querySelector('.main').getBoundingClientRect().left <= 1);
  const settingsBounds = await android.locator('#settingsPanel').boundingBox();
  ensure(settingsBounds?.x === 0 && Math.abs(settingsBounds?.y - 46) <= 1 &&
    Math.abs(settingsBounds?.width - 390) <= 1,
    'COS-style Android Settings must occupy the screen below the shared View/sidebar toolbar');
  ensure(await android.locator('#settingsPanel').getAttribute('data-settings-view') === 'general' &&
    await android.locator('#settingsPanel').getAttribute('data-settings-nav-open') === 'true' &&
    await android.locator('[data-android-settings-category="general"]').getAttribute('aria-current') === 'true' &&
    await android.locator('#androidSettingsDetailTitle').innerText() === 'General',
    'COS-style Android Settings must open General with the category sidebar available');
  ensure(await android.locator('#androidSettingsNav .android-settings-nav-footer').count() === 0 &&
    await android.locator('#androidSettingsCategories [data-android-settings-category="workspace"]').isVisible() &&
    await android.locator('#settingsClose').isVisible(),
    'Android Settings directory must not repeat a Settings footer or its divider beneath Workspace/categories');
  const settingsColumns = await android.evaluate(() => ({
    nav: document.getElementById('androidSettingsNav').getBoundingClientRect(),
    content: document.getElementById('androidSettingsContent').getBoundingClientRect(),
    viewport: innerWidth, scroll: document.documentElement.scrollWidth,
  }));
  ensure(settingsColumns.nav.width >= 389 && settingsColumns.nav.width <= 391 &&
    settingsColumns.content.width === 390 && settingsColumns.scroll <= 391,
    'Mobile COS Settings sidebar must cover the detail page at full viewport width without clipping it');
  ensure(await android.locator('#sidebarToggle').getAttribute('aria-label') === 'Hide settings sidebar',
    'COS topbar sidebar button must target Settings navigation while Settings is open');
  await android.locator('#sidebarToggle').click();
  ensure(await android.locator('#settingsPanel').getAttribute('data-settings-nav-open') === 'false' &&
    await android.locator('#androidSettingsDetailTitle').isVisible(),
    'Settings topbar sidebar button did not reveal the full General page');
  const settingsSwipePreview = await swipeAndroid('#androidSettingsDetailDescription', 112, 165, 177, 175);
  ensure(settingsSwipePreview.dragging && settingsSwipePreview.transform !== 'none',
    'Settings drawer must visibly follow the finger during a rightward swipe');
  ensure(await android.locator('#settingsPanel').getAttribute('data-settings-nav-open') === 'true' &&
    await android.locator('#androidSettingsSearch').isVisible(),
    'Short rightward swipe on Settings detail did not reopen navigation');
  await swipeAndroid('#androidSettingsNav', 218, 280, 155, 289);
  ensure(await android.locator('#settingsPanel').getAttribute('data-settings-nav-open') === 'false' &&
    await android.locator('#androidSettingsDetailTitle').innerText() === 'General',
    'Short leftward swipe in Settings sidebar failed to reveal selected category');
  await swipeAndroid('#androidSettingsDetailDescription', 100, 150, 111, 260);
  ensure(await android.locator('#settingsPanel').getAttribute('data-settings-nav-open') === 'false',
    'Vertical scroll inside Settings detail must not open navigation');
  await swipeAndroid('#androidSettingsDetailDescription', 112, 165, 178, 175);
  ensure(await android.locator('#settingsPanel').getAttribute('data-settings-nav-open') === 'true' &&
    await android.evaluate(() => document.activeElement?.id !== 'androidSettingsSearch'),
    'Settings swipe must reopen sidebar without showing Search keyboard');
  await android.locator('#sidebarToggle').click();
  ensure(await android.locator('#settingsPanel').getAttribute('data-settings-nav-open') === 'false',
    'Settings topbar sidebar button did not close navigation after swipe');
  await android.locator('#sidebarToggle').click();
  ensure(await android.locator('#settingsPanel').getAttribute('data-settings-nav-open') === 'true',
    'Settings topbar sidebar button did not reopen navigation');
  await android.setViewportSize({width: 820, height: 844});
  await android.waitForFunction(() => Math.abs(
    document.getElementById('androidSettingsNav').getBoundingClientRect().right -
    document.getElementById('androidSettingsContent').getBoundingClientRect().left) <= 2);
  const twoPane = await android.evaluate(() => {
    const nav = document.getElementById('androidSettingsNav').getBoundingClientRect();
    const detail = document.getElementById('androidSettingsContent').getBoundingClientRect();
    return {navRight:nav.right, detailLeft:detail.left, detailWidth:detail.width,
      documentWidth:document.documentElement.scrollWidth,
      navInert:document.getElementById('androidSettingsNav').inert};
  });
  ensure(Math.abs(twoPane.navRight - twoPane.detailLeft) <= 2 &&
    twoPane.detailWidth >= 500 && twoPane.documentWidth <= 821 && !twoPane.navInert,
    `COS Settings must use two readable side-by-side panels on wider screens: ${JSON.stringify(twoPane)}`);
  await android.setViewportSize({width:390,height:844});
  ensure(await android.locator('[data-android-settings-category]').count() === 8 &&
    await android.locator('#androidSettingsSearch').isVisible() &&
    await android.locator('#settingsClose').getByText('Back to chat').isVisible(),
    'Android category navigation/search/back-to-chat is missing');
  const mobileNavWidth = await android.evaluate(() => {
    const nav = document.getElementById('androidSettingsNav').getBoundingClientRect();
    const search = document.querySelector('.android-settings-search').getBoundingClientRect();
    return {navLeft:nav.left,navRight:nav.right,navWidth:nav.width,searchWidth:search.width,
      documentWidth:document.documentElement.scrollWidth};
  });
  ensure(mobileNavWidth.navLeft >= -1 && mobileNavWidth.navRight >= 389 &&
    mobileNavWidth.searchWidth >= 330 && mobileNavWidth.documentWidth <= 391,
    `Phone Settings navigation must fully cover its detail pane, with full-width Search: ${JSON.stringify(mobileNavWidth)}`);
  ensure(await android.locator('#provider').isHidden() && await android.locator('#storageAccessSection').isHidden(),
    'General Settings leaked Setup content');
  ensure(await android.locator('#projectBackendControls').isHidden() &&
    await android.locator('#sidebar #projectBackendControls').count() === 0 &&
    await android.locator('#projectNativeEnable').count() === 1,
    'Project Storage must exist once in Settings and not occupy the sidebar or Settings directory');
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-settings-directory.png'), fullPage: true });
  // Simulate the available height after a soft IME occupies the lower display.
  // This is a layout regression, NOT a claim of real hardware IME behavior.
  await android.locator('#androidSettingsSearch').focus();
  await android.setViewportSize({width:390,height:410});
  await android.waitForFunction(() => document.querySelector('.shell').getBoundingClientRect().bottom <= 411);
  const searchImeBox=await android.evaluate(() => {
    const field=document.getElementById('androidSettingsSearch').getBoundingClientRect();
    const shell=document.querySelector('.shell').getBoundingClientRect();
    const nav=document.getElementById('androidSettingsNav').getBoundingClientRect();
    return {fieldWidth:field.width,fieldHeight:field.height,fieldRight:field.right,
      shellBottom:shell.bottom,navRight:nav.right,scrollWidth:document.documentElement.scrollWidth};
  });
  ensure(searchImeBox.fieldWidth >= 170 && searchImeBox.fieldHeight >= 42 &&
    searchImeBox.fieldRight <= searchImeBox.navRight &&
    searchImeBox.shellBottom <= 411 && searchImeBox.scrollWidth <= 391,
    `Settings search must keep full input width on IME-height viewport: ${JSON.stringify(searchImeBox)}`);
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-settings-ime-compact.png') });
  await android.setViewportSize({width:390,height:844});
  await android.locator('#androidSettingsSearch').fill('Health');
  ensure(await android.locator('[data-android-settings-category="workspace"]').isVisible() &&
    await android.locator('[data-android-settings-category="setup"]').isHidden(),
    'Settings search must discover Health under Workspace only');
  await android.locator('#androidSettingsSearch').fill('permissions');
  ensure(await android.locator('[data-android-settings-category="setup"]').isVisible() &&
    await android.locator('[data-android-settings-category="usage"]').isHidden(),
    'Settings search must find actual category section names');
  await android.locator('#androidSettingsSearch').fill('API key');
  ensure(await android.locator('[data-android-settings-category="setup"]').isVisible() &&
    await android.locator('[data-android-settings-category="agents"]').isHidden(),
    'Settings search must find actual control labels inside the Setup category');
  await android.locator('#androidSettingsSearch').fill('unknown-setting-token-xyz');
  ensure(await android.locator('#androidSettingsNoResults').isVisible() &&
    await android.locator('[data-android-settings-category]:visible').count() === 0,
    'Settings search must show an honest empty state');
  await android.locator('#androidSettingsSearch').fill('');
  await android.locator('[data-android-settings-category="usage"]').click();
  ensure(await android.locator('#settingsPanel').getAttribute('data-settings-nav-open') === 'false' &&
    await android.locator('#androidSettingsNav').evaluate(node => node.inert) &&
    await android.locator('#androidSettingsBack').isVisible(),
    'Selecting COS Settings category must show readable full-width content on phone');
  ensure(await android.locator('#androidSettingsDetailTitle').innerText() === 'Usage' &&
    (await android.locator('[data-setting-category="usage"]').innerText()).includes('not available') &&
    await android.locator('#provider').isHidden(),
    'Usage must not claim synthetic account/token data or leak other category controls');
  await android.keyboard.press('Escape');
  ensure(await android.locator('#androidSettingsDirectory').isVisible() &&
    await android.locator('#settingsPanel').isVisible() &&
    await android.locator('#settingsPanel').getAttribute('data-settings-nav-open') === 'true',
    'Escape from a category must reveal COS sidebar without closing Settings');
  ensure(await android.evaluate(() => document.activeElement?.id !== 'androidSettingsSearch'),
    'Escape from a Settings category reopened the Android Search keyboard');
  await android.locator('[data-android-settings-category="appearance"]').click();
  await android.locator('#androidAppearanceTheme').selectOption('oled');
  await android.locator('#androidAppearanceDensity').selectOption('compact');
  await android.locator('#androidAppearanceReduceMotion').check();
  ensure(await android.evaluate(() => document.documentElement.dataset.appearance==='oled' &&
    document.documentElement.dataset.density==='compact' &&
    document.documentElement.dataset.reduceMotion==='true'),
    'Appearance controls must actually update local Android theme/density/motion');
  await android.locator('#androidAppearanceTheme').selectOption('dark');
  await android.locator('#androidAppearanceDensity').selectOption('comfortable');
  await android.locator('#androidAppearanceReduceMotion').uncheck();
  await android.locator('#androidSettingsBack').click();
  ensure(await android.evaluate(() => document.activeElement?.id !== 'androidSettingsSearch'),
    'Settings Back unexpectedly focused Search and requested the soft keyboard');
  ensure(await android.locator('#settingsPanel').getAttribute('data-settings-nav-open') === 'true' &&
    await android.locator('[data-android-settings-category="appearance"]').getAttribute('aria-current') === 'true',
    'Settings Back must reveal categories and preserve the last selected category');
  await swipeAndroid('.main', 50, 270, 270, 280);
  ensure(await android.locator('#settingsPanel').isVisible() &&
    await android.locator('#sidebar').evaluate(node => node.inert),
    'Sidebar swipe must not override full-screen Android Settings');
  await android.locator('[data-android-settings-category="agents"]').click();
  ensure(await android.locator('#androidAutomationBrowserMode,#androidAutomationAppMode').count() === 2 &&
    await android.locator('[data-setting-category="agents"] #androidAutomationBrowserMode, [data-setting-category="agents"] #androidAutomationAppMode').count() === 0 &&
    await android.locator('#androidCosAccessPanel .android-mcp-fine-tune #androidAutomationBrowserMode').count() === 1 &&
    await android.locator('#androidCosAccessPanel .android-mcp-fine-tune #androidAutomationAppMode').count() === 1 &&
    (await android.locator('#androidAutomationAppMode option[value="realtime"]').textContent()).trim() === 'RealTime Screen' &&
    await android.locator('#androidAutomationBrowserUrl,#androidAutomationBrowserOpen,#androidAutomationBrowserStop').count() === 0 &&
    await android.locator('#androidSettingsOpenAgents,#androidSettingsOpenSkills,#androidSettingsOpenPlugins').count() === 0 &&
    await android.locator('[data-android-settings-category="plugins"]').count() === 1 &&
    await android.locator('#androidAutomationOpenFineTune').isVisible(),
    'Agents Settings must not own duplicated automation modes: both dropdowns belong to Setup Access Fine-tune only');
  ensure(await android.locator('#connectionSection').isHidden(), 'Agents category leaked AI connection controls');
  await android.locator('#androidAutomationOpenFineTune').click();
  await android.locator('#androidCosSetupChecking').waitFor({state:'hidden'});
  ensure(await android.locator('#settingsPanel').getAttribute('data-settings-view') === 'setup' &&
    await android.locator('[data-cos-setup-step="tools"]').getAttribute('aria-current') === 'step' &&
    await android.locator('#androidCosAccessPanel').isVisible() &&
    await android.locator('#androidMcpAccessPresets .android-mcp-fine-tune').getAttribute('open') === '' &&
    await android.locator('#androidAutomationModesGroup').getAttribute('open') === '',
    'Agents guide must open Setup Access with Fine-tune permissions and Automation modes expanded');
  // Restore the original QA page's initial Setup selection; the rest of this
  // suite intentionally tests a fresh Permissions-first onboarding flow.
  await android.locator('[data-cos-setup-step="access"]').click();
  await android.locator('#androidSettingsBack').click();
  await android.locator('[data-android-settings-category="workspace"]').click();
  ensure(await android.locator('#androidWorkspaceDashboard').isVisible() &&
    await android.locator('#androidWorkspacePermissionsSlot #androidCosAccessPanel').isVisible() &&
    await android.locator('#androidWorkspaceFoldersSlot #androidCosFolderPanel').isVisible() &&
    await android.locator('#androidWorkspaceCopyActivity').isVisible() &&
    await android.locator('#androidWorkspaceRunChecks').isDisabled() &&
    await android.locator('#androidWorkspaceReadOnly').isDisabled() &&
    await android.locator('#androidWorkspaceChooseFolder').isDisabled() &&
    await android.locator('#projectStorageSettings').isHidden() &&
    await android.locator('#projectNativeEnable').isHidden() &&
    await android.locator('#androidSettingsOpenFiles').isHidden(),
    'Workspace must show native Permissions, Folders, Activity and Health, without retired migration and Files cards');
  const workspaceIds = ['androidCosAccessPanel','androidCosFolderPanel','androidMcpAccessPresets',
    'androidCosSelectedPath','androidCosFolderBrowse','androidCosFolderReset'];
  ensure(await android.evaluate(ids =>
    ids.every(id => document.querySelectorAll('[id="'+id+'"]').length === 1),workspaceIds),
    'Workspace and Setup must keep one physical copy of every shared native control');
  const healthPreview = await android.evaluate(() => ({
    link:document.getElementById('androidWorkspaceVerifiedLink').textContent,
    lastCall:document.getElementById('androidWorkspaceLastCall').textContent,
    route:document.getElementById('androidWorkspaceRoute').textContent,
    errors:document.getElementById('androidWorkspacePollErrors').textContent,
    uptime:document.getElementById('androidWorkspaceUptime').textContent,
    tunnel:document.getElementById('androidWorkspaceTunnelStatus').textContent,
    plugin:document.getElementById('androidWorkspacePluginStatus').textContent,
    activity:document.getElementById('androidWorkspaceActivityStatus').textContent,
    eventRows:document.querySelectorAll('#androidWorkspaceActivityFeed .android-workspace-activity-row').length
  }));
  ensure(healthPreview.tunnel !== 'Connected' && healthPreview.plugin !== 'Verified' &&
    ['—','Unknown','Checking…'].includes(healthPreview.link.trim()) &&
    ['—','Unknown','Checking…'].includes(healthPreview.lastCall.trim()) &&
    ['—','Unknown','Checking…'].includes(healthPreview.errors.trim()) &&
    ['—','Unknown','Checking…'].includes(healthPreview.uptime.trim()) &&
    healthPreview.eventRows === 0 &&
    !/\b[1-9]\d* verified MCP tool call/.test(healthPreview.activity),
    `Browser preview must not invent a native tunnel, linked plugin or tool calls: ${JSON.stringify(healthPreview)}`);
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-workspace-settings.png'), fullPage: true });
  for (const width of [320,360,390]) {
    await android.setViewportSize({width,height:844});
    const workspaceGeometry = await android.evaluate(() => {
      const ids=['androidWorkspaceDashboard','androidWorkspacePermissionsSlot',
        'androidWorkspaceFoldersSlot','androidWorkspaceCopyActivity','androidWorkspaceReadOnly',
        'androidWorkspaceChooseFolder','androidWorkspaceRunChecks',
        'androidWorkspaceActivityFeed','androidWorkspaceActivityStatus',
        'androidWorkspaceVerifiedLink','androidWorkspaceLastCall',
        'androidWorkspaceRoute','androidWorkspacePollErrors',
        'androidWorkspaceTunnelStatus','androidWorkspaceUptime','androidWorkspaceLastTool',
        'androidWorkspaceTunnelClient','androidWorkspaceLocalServer',
        'androidWorkspacePluginStatus','androidWorkspaceToolCount'];
      return {viewport:document.documentElement.clientWidth,document:document.documentElement.scrollWidth,
        elements:ids.map(id => {
          const node=document.getElementById(id),rect=node.getBoundingClientRect();
          return {id,left:rect.left,right:rect.right,width:rect.width,height:rect.height};
        })};
    });
    ensure(workspaceGeometry.document <= width+1 &&
      workspaceGeometry.elements.every(box => box.left >= -1 && box.right <= width+1) &&
      workspaceGeometry.elements.filter(box => /CopyActivity|ReadOnly|ChooseFolder|RunChecks/.test(box.id))
        .every(box => box.width >= 40 && box.height >= 40),
      `Workspace must not overflow or clip touch controls at ${width}px: ${JSON.stringify(workspaceGeometry)}`);
    await android.screenshot({
      path:path.join(screenshotFolder,`native-android-workspace-${width}.png`),fullPage:true});
    if (width === 320 || width === 390) {
      // Workspace contains four vertically stacked cards. Capture the lower
      // sections explicitly; a screenshot at scrollTop=0 only shows Access.
      for (const area of ['activity','folders','health']) {
        await android.locator(`.android-workspace-${area}`).evaluate(node => {
          const scroller=document.getElementById('androidSettingsContent');
          scroller.scrollTop += node.getBoundingClientRect().top -
            scroller.getBoundingClientRect().top - 12;
        });
        await android.screenshot({
          path:path.join(screenshotFolder,`native-android-workspace-${area}-${width}.png`)});
      }
      await android.locator('#androidWorkspacePermissionsTitle').evaluate(node => {
        const scroller=document.getElementById('androidSettingsContent');
        scroller.scrollTop=0;
      });
    }
  }
  await android.setViewportSize({width: 390, height: 844});
  await android.locator('#androidSettingsBack').click();
  await android.locator('[data-android-settings-category="general"]').click();
  ensure(await android.locator('#androidGeneralSettings').isVisible() &&
    await android.locator('#androidGeneralSettings .android-general-section').count() === 5 &&
    await android.locator('#androidWorkspaceDashboard').isHidden(),
    'General must expose five Android sections without leaking Workspace');
  await android.locator('#androidSettingsBack').click();
  await android.locator('[data-android-settings-category="activity"]').click();
  ensure(await android.locator('#androidSettingsOpenReview').isVisible() &&
    await android.locator('#moduleSection').isHidden(),
    'Activity must offer existing Review rather than fake recorded activity');
  await android.locator('#androidSettingsBack').click();
  await android.locator('[data-android-settings-category="setup"]').click();
  const cosSetupOrder = ['access','extension','browser','folder','tools','tunnel','key','connect','mcpPlugin','ready'];
  const cosSetupTitles = ['1. Permissions','2. Extensions','3. Browser','4. Folder',
    '5. Access','6. Tunnel','7. API key','8. Connect','9. MCP Plugin','10. Ready'];
  ensure(await android.locator('#androidCosSetupWizard').isVisible() &&
    JSON.stringify(await android.locator('[data-cos-setup-step]').evaluateAll(buttons =>
      buttons.map(button => button.dataset.cosSetupStep))) === JSON.stringify(cosSetupOrder) &&
    JSON.stringify(await android.locator('[data-cos-setup-step] span').allTextContents()) ===
      JSON.stringify(cosSetupTitles),
    'Android Setup must place permission Allow before native extensions, browser choice and the remaining steps');
  await android.waitForFunction(() => document.getElementById('androidCosSetupProgressText')
    .textContent.includes(' of 10 completed'));
  const initialCosProgress = await android.locator('#androidCosSetupProgress').getAttribute('aria-valuenow');
  const initialStageStates = await android.locator('[data-cos-setup-step]').evaluateAll(buttons =>
    buttons.map(button => [button.dataset.cosSetupStep, button.querySelector('small')?.textContent, button.disabled]));
  ensure(await android.locator('#androidCosSetupProgress').getAttribute('aria-valuemax') === '10' &&
    await android.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Pending' &&
    ['Checking…','Unable to check'].includes(await android.locator('[data-cos-setup-step="browser"] small').innerText()) &&
    ['Checking…','Unable to check'].includes(await android.locator('[data-cos-setup-step="extension"] small').innerText()) &&
    ['Pending','Checking…','Unable to check'].includes(await android.locator('[data-cos-setup-step="access"] small').innerText()) &&
    await android.locator('[data-cos-setup-step="connect"] small').innerText() === 'Pending' &&
    await android.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() === 'Pending' &&
    await android.locator('[data-cos-setup-step]').evaluateAll(buttons => buttons.every(button => !button.disabled)),
    `Setup must distinguish unverified stages while allowing all ten panels to be inspected: ${JSON.stringify(initialStageStates)}`);
  ensure(await android.locator('#androidCosSetupPrev,#androidCosSetupNext,#androidCosSetupRefresh,#androidCosSetupSummary').count() === 0 &&
    await android.locator('#androidCosSetupCurrent').isHidden() &&
    await android.locator('#storageAccessSection').isVisible() &&
    await android.locator('#androidCosExtensionControls').isHidden() &&
    await android.locator('#androidCosBrowserChoices').isHidden() &&
    await android.locator('#customBrowserSettings').count() === 0,
    'Permissions tab must show only five native permission rows, not the retired status card/navigation controls');
  ensure(await android.locator('#androidCosAdvancedFooter,#androidCosTunnelAdvanced').count() === 0 &&
    await android.locator('#androidTunnelLegacyCompat').isHidden() &&
    await android.locator('#androidOpenAiTunnelMethodSelect').isHidden() &&
    await android.locator('#androidLocalMcpControls').isHidden() &&
    await android.locator('#androidLocalMcpStart').isHidden() &&
    await android.locator('#androidLocalMcpStop').isHidden(),
    'Setup may retain inert bridge bindings but must remove all visible Advanced, Cloudflare and local server UI');
  const setupPanels = {
    extension:'#androidCosExtensionControls',browser:'#androidCosBrowserChoices',
    folder:'#androidCosFolderPanel',tools:'#androidCosAccessPanel',
    tunnel:'#androidCosTunnelMethod',
    key:'#androidCosKeyPanel',connect:'#androidCosConnectPanel',mcpPlugin:'#androidCosMcpPluginPanel',
    ready:'#androidCosReadyPanel'
  };
  for (const [step,panel] of Object.entries(setupPanels)) {
    await android.locator(`[data-cos-setup-step="${step}"]`).click();
    ensure(await android.locator(panel).isVisible() &&
      await android.locator('#customBrowserSettings').count() === 0 &&
      await android.locator('#storageAccessSection').isHidden() &&
      await android.locator('#androidCosSetupCurrent').isVisible() &&
      await android.locator('#androidCosAdvancedFooter,#androidCosTunnelAdvanced').count() === 0 &&
      (await android.locator(`[data-cos-setup-step="${step}"] small`).innerText()) !== 'Verified',
      `Setup ${step} must be inspectable without falsely granting prerequisites or leaking removed Advanced controls`);
  }
  ensure(await android.locator('#androidCosReadyChecklist li').count() === 10 &&
    (await android.locator('#androidCosReadyStatus').textContent()).includes('Finish the pending steps') &&
    (await android.locator('#androidCosSetupStepState').textContent()).trim().length > 12 &&
    (await android.locator('#androidCosChatgptStatus').textContent()).trim().length > 15 &&
    await android.locator('[data-cos-setup-step="tunnel"] small').innerText() !== 'Verified' &&
    await android.locator('#androidCosChatgptConnect').count() === 1 &&
    await android.locator('#androidCosTunnelDisconnect').count() === 1 &&
    await android.locator('#androidCosChatgptOpen,#androidCosChatgptBring,#androidCosChatgptCheck,#androidCosPrepareTest').count() === 0 &&
    await android.locator('#androidCosProvider,#androidCosApiKey,#androidCosModel').count() === 0 &&
    await android.locator('#androidCosAdvancedFooter,#androidCosTunnelAdvanced').count() === 0 &&
    await android.locator('#androidOpenAiTunnelMethodSelect').isHidden(),
    'Every later stage must use native status without any Advanced or desktop controls');
  await android.locator('[data-cos-setup-step="access"]').click();
  ensure(await android.locator('[data-cos-setup-step="access"]').getAttribute('aria-current') === 'step' &&
    await android.locator('#androidCosSetupStepTitle').isHidden() &&
    await android.locator('#androidCosSetupStepAction').isHidden() &&
    await android.locator('#accessibilityManage').isDisabled(),
    'No duplicate 1. Permissions box; permission buttons remain controlled by OS');
  await android.evaluate(() => window.dispatchEvent(new Event('focus')));
  ensure(await android.locator('#androidCosSetupProgress').getAttribute('aria-valuenow') === initialCosProgress,
    'Automatic read-only refresh without native permissions must not create a verified receipt');
  const settingsSizes = await android.evaluate(() => ({
    header: document.querySelector('.settings-head').getBoundingClientRect().height,
    control: document.querySelector('#storageAccessRequest').getBoundingClientRect().height,
  }));
  ensure(settingsSizes.header < 92 && settingsSizes.control >= 44, 'Android settings have oversized header or undersized controls');
  ensure(await android.locator('#connectionSection').isHidden() &&
    await android.locator('#backgroundChatgptSettings').isHidden() &&
    await android.locator('#provider').isHidden() &&
    await android.locator('#model').isHidden() &&
    await android.locator('#apiKey').isHidden() &&
    await android.locator('#backgroundChatgptOpen').isHidden() &&
    await android.locator('#connectionSection').getAttribute('aria-hidden') === 'true' &&
    await android.locator('#backgroundChatgptSettings').getAttribute('aria-hidden') === 'true' &&
    !(await android.locator('#androidSettingsContent').innerText()).includes('AI connection') &&
    !(await android.locator('#androidSettingsContent').innerText()).includes('ChatGPT built-in browser'),
    'Entire legacy AI connection and ChatGPT built-in browser cards must be absent from visible Android Setup');
  ensure(await android.locator('#storageAccessSection').isVisible(),
    'Android Settings missing All files access opt-in control');
  ensure(await android.locator('#provider option[value="chatgpt-browser"]').count() === 1 &&
    await android.locator('#provider').inputValue() === 'chatgpt-browser' &&
    await android.locator('#provider option').evaluateAll(options => {
      const names = options.map(option => option.value);
      return names.includes('chatgpt-browser') && names.includes('openai') &&
        names.every(name => ['chatgpt-browser','openai','anthropic','openrouter'].includes(name));
    }),
    'Android hidden selector must default to ChatGPT and retain only supported native API providers');
  ensure((await android.locator('#storageAccessState').innerText()) === 'Preview only' &&
    await android.locator('#storageAccessRequest').isDisabled(),
    'Browser preview must not pretend to grant native Android permission');
  ensure(await android.locator('#androidOptionalPermissionsSection summary').isVisible() &&
    !await android.locator('#androidOptionalPermissionsSection').evaluate(node => node.open) &&
    await android.locator('[data-optional-permission]').count() === 3 &&
    await android.locator('[data-optional-permission-request]').count() === 3 &&
    await android.locator('[data-optional-permission-request]').evaluateAll(buttons =>
      buttons.every(button => button.disabled)) &&
    await android.locator('#androidOptionalPermissionsSection [role="status"]').evaluateAll(states =>
      states.every(state => !/^Allowed$/.test((state.textContent || '').trim()))),
    'Browser preview must retain three optional OS permissions in a collapsed disclosure without granting them');
  await android.locator('#androidOptionalPermissionsSection summary').click();
  ensure(await android.locator('#androidOptionalPermissionsSection').evaluate(node => node.open) &&
    await android.locator('[data-optional-permission-request="camera"]').isVisible() &&
    await android.locator('[data-optional-permission-request="camera"]').isDisabled(),
    'Tapping Optional disclosure reveals real permission rows but preview cannot request OS access');
  await android.locator('#androidOptionalPermissionsSection summary').click();
  ensure(!await android.locator('#androidOptionalPermissionsSection').evaluate(node => node.open) &&
    await android.locator('[data-optional-permission-request="camera"]').isHidden(),
    'Optional disclosure must collapse again without deleting its status rows');
  ensure(await android.locator('#storageAccessSection [data-permission-request]').count() === 4,
    'Android Permissions Center should expose exactly the four supported Settings actions');
  for (const key of ['files', 'overlay', 'notifications', 'battery']) {
    ensure(await android.locator(`[data-permission-request="${key}"]`).isDisabled(),
      `Browser-only preview must not request ${key} permission`);
  }
  ensure((await android.locator('#storageAccessSection').innerText()).includes('Display over other apps') &&
    !(await android.locator('#storageAccessSection').innerText()).includes('One-frame screen capture') &&
    await android.locator('#screenCaptureOne').count() === 0 &&
    (await android.locator('#storageAccessSection').innerText()).includes('Accessibility service') &&
    await android.locator('[data-permission-row]').count() === 5 &&
    await android.locator('#storageAccessFeedback,#androidCosAllowNext,#refreshAndroidPermissions,#accessibilityFeedback').count() === 0 &&
    !(await android.locator('#storageAccessSection').innerText()).includes('Statuses come from Android'),
    'Android keeps Accessibility Allow while removing only the helper text and buttons underneath');
  ensure(await android.locator('#androidCosTunnelMethod').count() === 1 &&
    await android.locator('#androidOpenAiTunnelId').count() === 1 &&
    await android.locator('#androidTunnelSaveId').count() === 1 &&
    await android.locator('#androidCosChatgptConnect,#androidCosTunnelDisconnect').count() === 2 &&
    await android.locator('#androidCosAdvancedFooter,#androidCosTunnelAdvanced').count() === 0 &&
    await android.locator('#androidOpenAiTunnelMethodSelect').isHidden(),
    'Android-only setup retains the Core ID and native Connect actions, with no Advanced method picker');
  await android.locator('#settingsPanel').evaluate(node => { node.scrollTop = 0; });
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-permissions.png'), fullPage: true });
  ensure(!(await desktop.locator('#storageAccessSection').isVisible()),
    'Android-only storage controls leaked into Windows UI');
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-connections.png'), fullPage: true });
  await closeNativeAndroidSettings(android);
  ensure(!(await android.locator('#settingsPanel').isVisible()), 'Android Connections did not close');
  // The old Agents category remains addressable, but modes moved into Setup
  // Access Fine-tune and no browser launch button is exposed there.
  await android.locator('#sidebarToggle').click();
  await android.locator('#sidebarCustomize').click();
  await android.locator('[data-android-settings-category="agents"]').click();
  ensure(await android.locator('[data-setting-category="agents"] #androidAutomationBrowserMode, [data-setting-category="agents"] #androidAutomationAppMode').count() === 0 &&
    await android.locator('#androidCosAccessPanel .android-mcp-fine-tune #androidAutomationBrowserMode').count() === 1 &&
    await android.locator('#androidAutomationBrowserUrl,#androidAutomationBrowserOpen,#androidAutomationBrowserStop').count() === 0 &&
    await android.locator('#androidSettingsOpenAgents,#androidSettingsOpenSkills,#androidSettingsOpenPlugins').count() === 0,
    'Reopening Agents Settings must not duplicate Setup automation dropdowns or obsolete shortcuts');
  await android.locator('#androidSettingsBack').click();
  await android.locator('[data-android-settings-category="workspace"]').click();
  ensure(await android.locator('#androidWorkspaceActivityFeed').getAttribute('role') === 'log' &&
    await android.locator('#androidWorkspaceCopyActivity').isVisible() &&
    await android.locator('#androidWorkspaceCopyActivity').isDisabled() &&
    await android.locator('#androidWorkspaceActivityFeed .android-workspace-activity-row').count() === 0 &&
    await android.locator('#androidWorkspaceChecksResult').isHidden() &&
    await android.locator('#settingsPanel').isVisible(),
    'Workspace Activity must expose a native-backed log and disabled Copy when no events exist');
  await android.locator('#androidSettingsBack').click();
  await android.locator('[data-android-settings-category="activity"]').click();
  await android.locator('#androidSettingsOpenReview').click();
  ensure(await android.locator('#toolReview').isVisible() &&
    await android.locator('#settingsPanel').isHidden(),
    'Android Settings Activity shortcut failed to open real Review panel');
  await android.locator('#toolViewClose').click();
  await pageGeometry(android, 'Native Android 390x844', false);
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-390.png'), fullPage: true });
  await android.setViewportSize({ width: 360, height: 640 });
  await pageGeometry(android, 'Native Android 360x640', false);
  ensure(await android.locator('#androidComposePlan').isVisible() &&
    await android.locator('#speechDictation').isVisible() &&
    await android.locator('#androidComposerModel').isVisible(),
    'Plan/mic/actual model must not disappear on normal 360px Android phone');
  ensure(!(await android.locator('.welcome').isVisible()), 'Android small-screen welcome should stay hidden');
  ensure(await android.locator('#extrasToggle').count() === 0, '360px header still includes duplicate extensions button');
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-compact.png'), fullPage: true });
  await android.setViewportSize({ width: 320, height: 568 });
  await pageGeometry(android, 'Native Android 320x568', false);
  ensure(await android.locator('#androidComposePlan').isVisible() &&
    await android.locator('#androidComposerModel').isVisible() &&
    await android.locator('#speechDictation').isVisible() &&
    await android.locator('#androidComposeMode').isVisible(),
    'Plan/mic/actual model must remain available on 320px Android phones');
  await android.locator('#androidComposeMode').click();
  ensure(await android.locator('#androidBypassToggle').getAttribute('aria-checked') === 'false' &&
    await android.locator('#androidBypassToggle').isVisible() &&
    (await android.locator('#androidBypassNotice').textContent()).includes('not active') &&
    await android.locator('[data-compose-mode="goal"]').isVisible(),
    '320px mode picker must show Goal and Bypass Off switch with truthful backend-pending state');
  await android.screenshot({path:path.join(screenshotFolder,'native-android-goal-mode-320.png')});
  await android.locator('#androidComposeMode').click();
  await android.locator('#sidebarToggle').click();
  await android.locator('#sidebarCustomize').click();
  const smallSettingsGeometry = await android.evaluate(() => {
    const panel = document.getElementById('settingsPanel');
    const rect = panel.getBoundingClientRect();
    return { x: rect.x, y: rect.y, width: rect.width,
      documentWidth: document.documentElement.scrollWidth,
      navWidth: document.getElementById('androidSettingsCategories').scrollWidth };
  });
  ensure(smallSettingsGeometry.x === 0 && Math.abs(smallSettingsGeometry.y - 46) <= 1 &&
    Math.abs(smallSettingsGeometry.width - 320) <= 1 &&
    smallSettingsGeometry.documentWidth <= 320 &&
    smallSettingsGeometry.navWidth <= 292,
    '320px Settings directory must be full-screen with no horizontal overflow');
  await android.locator('[data-android-settings-category="setup"]').click();
  await android.waitForFunction(() => document.getElementById('settingsPanel').dataset.settingsNavOpen === 'false' &&
    getComputedStyle(document.getElementById('androidSettingsNav')).visibility === 'hidden');
  const smallSetupGeometry = await android.evaluate(() => {
    const panel = document.getElementById('androidCosSetupWizard');
    const rect = panel.getBoundingClientRect();
    const steps = document.getElementById('androidCosSetupSteps');
    const progress = document.getElementById('androidCosSetupProgress');
    return {left:rect.left,right:rect.right,stepWidth:steps.scrollWidth,
      stepClientWidth:steps.clientWidth,progressWidth:progress.getBoundingClientRect().width,
      documentWidth:document.documentElement.scrollWidth};
  });
  ensure(smallSetupGeometry.left >= -1 && smallSetupGeometry.right <= 321 &&
    smallSetupGeometry.documentWidth <= 321 &&
    smallSetupGeometry.stepWidth <= smallSetupGeometry.stepClientWidth + 1 &&
    smallSetupGeometry.progressWidth <= 320,
    `320px COS Setup wizard overflow: ${JSON.stringify(smallSetupGeometry)}`);
  await android.locator('[data-cos-setup-step="extension"]').click();
  await android.locator('#androidCosExtensionControls').scrollIntoViewIfNeeded();
  ensure(await android.locator('#androidCosExtensionControls').isVisible() &&
    await android.locator('#androidCosExtensionOpen').isVisible() &&
    await android.locator('#androidCosExtensionInstall').isVisible() &&
    await android.locator('#androidCosExtensionControls button').count() === 2 &&
    await android.locator('#androidCosExtensionOpen').isDisabled() &&
    await android.locator('#androidCosExtensionInstall').isDisabled() &&
    await android.locator('#customBrowserSettings,#customGeckoOpen,#customGeckoInstallProbe').count() === 0 &&
    await android.locator('#androidCosExtensionControls').evaluate(node =>
      node.scrollWidth <= node.clientWidth + 1 &&
      node.getBoundingClientRect().right <= document.documentElement.clientWidth + 1),
    'Two-button Android extension Step 2 must fit 320px and never claim a fake preview backend');
  for (const [step,panel] of [['tools','androidCosAccessPanel'],['tunnel','androidCosTunnelMethod'],
    ['key','androidCosKeyPanel'],['connect','androidCosConnectPanel'],
    ['mcpPlugin','androidCosMcpPluginPanel'],['ready','androidCosReadyPanel']]) {
    await android.locator(`[data-cos-setup-step="${step}"]`).click();
    const dimensions = await android.locator(`#${panel}`).evaluate(node => ({
      scroll:node.scrollWidth,client:node.clientWidth,
      right:node.getBoundingClientRect().right,
      viewport:document.documentElement.scrollWidth
    }));
    ensure(dimensions.scroll <= dimensions.client + 1 &&
      dimensions.right <= 321 && dimensions.viewport <= 321,
      `320px Android Setup ${step} panel overflow: ${JSON.stringify(dimensions)}`);
    if (step === 'tunnel') {
      ensure(await android.locator('#androidCosTunnelAdvanced').count() === 0 &&
        await android.locator('#androidLocalMcpControls').isHidden() &&
        await android.locator('#androidTunnelLegacyCompat').isHidden() &&
        await android.locator('#androidTunnelSaveId').isVisible(),
        '320px Step6 only displays Core tunnel ID and Save, with no Advanced or local server controls');
      await android.screenshot({path:path.join(screenshotFolder,'native-android-core-tunnel-320.png'),fullPage:true});
    }
    if (step === 'key') {
      ensure(await android.locator('#androidTunnelKeyNeededInput').isVisible() &&
        await android.locator('#androidTunnelKeyNeededConfirm').isHidden() &&
        await android.locator('#androidTunnelRemoveStoredKey').isHidden() &&
        await android.locator('#androidCosProvider').isHidden(),
        '320px Step7 must show only restricted Runtime API key flow, with contextual controls hidden until needed');
      await android.screenshot({path:path.join(screenshotFolder,'native-android-create-api-key-step7-320.png'),fullPage:true});
    }
    if (step === 'connect') {
      ensure(await android.locator('#androidCosChatgptConnect').isVisible() &&
        await android.locator('#androidCosTunnelDisconnect').isVisible() &&
        await android.locator('#androidCosChatgptConnect').isDisabled() &&
        await android.locator('#androidCosChatgptOpen,#androidCosChatgptBring,#androidCosChatgptCheck,#androidCosPrepareTest').count() === 0 &&
        await android.locator('#androidCosProvider,#androidCosApiKey,#androidCosModel').count() === 0,
        '320px Step8 must offer Secure MCP tunnel controls disabled without an authorized Core ID/key, no website Sign in/Bring');
      await android.screenshot({path:path.join(screenshotFolder,'native-android-secure-tunnel-step8-320.png'),fullPage:true});
    }
    if (step === 'mcpPlugin') {
      ensure(await android.locator('#androidCosMcpPluginStatus').isVisible() &&
        await android.locator('#androidCosMcpPluginRefresh').count() === 0 &&
        await android.locator('#androidCosMcpPluginOpen').isVisible() &&
        await android.locator('#androidCosMcpPhoneName').isVisible() &&
        await android.locator('#androidCosMcpCopyName').isVisible() &&
        await android.locator('#androidCosMcpCopyDescription').isVisible() &&
        await android.locator('#androidCosMcpPluginConnect,#androidCosMcpPluginDisconnect').count() === 0,
        '320px Step9 must show phone Core Plugin setup, copy controls and verified native evidence without duplicate Connect');
      await android.screenshot({path:path.join(screenshotFolder,'native-android-mcp-plugin-320.png'),fullPage:true});
    }
  }
  await android.screenshot({path:path.join(screenshotFolder,'native-android-cos-setup-320.png'),fullPage:true});
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-settings-320.png'), fullPage: true });
  await closeNativeAndroidSettings(android);
  ensure(await android.locator('#toolPanelToggle').isVisible() &&
    await android.locator('#sidebarQuickSearch').isVisible(),
    '320px Android header lost navigation controls');
  await android.locator('#sidebarQuickSearch').click();
  const smallViewMenu = await android.locator('#androidViewMenu').boundingBox();
  ensure(!!smallViewMenu && smallViewMenu.x >= 0 && smallViewMenu.x + smallViewMenu.width <= 321 &&
    smallViewMenu.y + smallViewMenu.height <= 569,
    'View menu overflows the 320px Android viewport');
  await android.locator('[data-view-action="zoom-in"]').click();
  await android.locator('[data-view-action="zoom-in"]').click();
  const smallZoomedMenu = await android.locator('#androidViewMenu').boundingBox();
  ensure(!!smallZoomedMenu && smallZoomedMenu.x >= 0 &&
    smallZoomedMenu.x + smallZoomedMenu.width <= 321 &&
    smallZoomedMenu.y + smallZoomedMenu.height <= 569,
    'View menu overflows 320px viewport at 120% zoom');
  await android.locator('[data-view-action="zoom-reset"]').click();
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-view-menu-320.png'), fullPage: true });
  await android.locator('#toolPanelToggle').click();
  ensure(!(await android.locator('#androidViewMenu').isVisible()) &&
    await android.locator('#toolPanelMenu').isVisible(),
    'Opening workspace panels failed to close COS View menu');
  await android.locator('#toolPanelToggle').click();
  await android.screenshot({ path: path.join(screenshotFolder, 'native-android-320.png'), fullPage: true });
  await android.locator('#prompt').focus();
  await android.setViewportSize({width:320,height:390});
  await android.waitForFunction(() => document.querySelector('.shell').getBoundingClientRect().bottom <= 391);
  const composerImeBox=await android.evaluate(() => {
    const composer=document.querySelector('.composer-wrap').getBoundingClientRect();
    const shell=document.querySelector('.shell').getBoundingClientRect();
    const send=document.getElementById('send').getBoundingClientRect();
    return {composerTop:composer.top,composerBottom:composer.bottom,shellBottom:shell.bottom,
      sendBottom:send.bottom,scrollWidth:document.documentElement.scrollWidth};
  });
  ensure(composerImeBox.composerTop >= 46 && composerImeBox.composerBottom <= 391 &&
    composerImeBox.sendBottom <= 391 && composerImeBox.shellBottom <= 391 &&
    composerImeBox.scrollWidth <= 321,
    `Ask anything composer must remain visible on IME-height viewport: ${JSON.stringify(composerImeBox)}`);
  await android.screenshot({path:path.join(screenshotFolder,'native-android-composer-ime-compact.png')});
  await android.setViewportSize({width:320,height:568});
  await android.locator('#androidComposeMore').click();
  const narrowRulesMenu = await android.locator('#androidComposeMoreMenu').boundingBox();
  ensure(!!narrowRulesMenu && narrowRulesMenu.x >= 0 &&
    narrowRulesMenu.x + narrowRulesMenu.width <= 321 &&
    narrowRulesMenu.y >= 0 && narrowRulesMenu.y + narrowRulesMenu.height <= 569,
    'Standing rules dropdown must fit 320x568 without screen overflow');
  ensure(await android.locator('#androidStandingRulesText').count() === 1 &&
    await android.locator('#androidStandingRulesReset').count() === 1 &&
    await android.locator('#androidRulesThisSession, #androidRulesAllSessions').count() === 0,
    'Compact overflow must provide one rules editor and Set/Reset without toggles');
  await android.screenshot({ path: path.join(screenshotFolder,'native-android-standing-rules-320.png') });
  await android.locator('#androidComposeMore').click();

  // Assert the first HTML paint, with deferred app.js artificially blocked.
  // It must be Android-native before the application bundle can run.
  const cold = await browser.newPage({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
    userAgent: 'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36',
  });
  cold.on('pageerror', error => errors.push(error.message));
  let releaseMainScript;
  const blocked = new Promise(resolve => { releaseMainScript = resolve; });
  await cold.route('**/app.js', async route => {
    await blocked;
    await route.continue();
  });
  try {
    await cold.goto(app.url, { waitUntil: 'commit' });
    await cold.locator('#welcome').waitFor({ state: 'attached' });
    const beforeBundle = await cold.evaluate(() => ({
      htmlClass: document.documentElement.classList.contains('native-android'),
      welcome: getComputedStyle(document.querySelector('.welcome')).display,
      newButton: getComputedStyle(document.getElementById('newChatTop')).display,
      header: document.querySelector('.topbar').getBoundingClientRect().height,
    }));
    ensure(beforeBundle.htmlClass, 'Android startup style waits for the deferred app bundle');
    ensure(beforeBundle.welcome === 'none' && beforeBundle.newButton === 'none' && beforeBundle.header <= 46,
      'Android cold start paints the old desktop UI before the bundle initializes');
  } finally {
    releaseMainScript();
  }
  await cold.waitForLoadState('domcontentloaded');
  await cold.waitForFunction(() => document.documentElement.classList.contains('native-android'));
  ensure(!(await cold.locator('#newChatTop').isVisible()), 'Android cold launch briefly exposes desktop +New');
  ensure(!(await cold.locator('.welcome-grid').isVisible()), 'Android cold launch briefly exposes large desktop welcome cards');
  ensure(!(await cold.locator('.welcome').isVisible()), 'Android cold launch still shows the center welcome');
  await cold.close();

  // Synthetic messages exercise the conversation layout without using real data or credentials.
  const chat = await browser.newPage({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true,
    userAgent: 'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36' });
  chat.on('pageerror', error => errors.push(error.message));
  await chat.route('**/api/state', async route => {
    const response = await route.fetch();
    const payload = await response.json();
    await route.fulfill({ response, json: {
      ...payload,
      platform: 'android',
      sessions: [{ id: 'synthetic-qa-chat', provider: 'openai', model: 'qa-model',
        messages: [{ role: 'user', text: 'Can you help me plan my next project?' },
          { role: 'assistant', text: 'Sure. We can outline the goal, define milestones, and verify each result.' }] }],
    } });
  });
  await chat.goto(app.url);
  await chat.waitForFunction(() => document.documentElement.classList.contains('native-android'));
  await chat.locator('#sidebarToggle').click();
  await chat.locator('#threads button').first().click();
  await chat.waitForFunction(() => document.getElementById('sidebar').inert && document.querySelector('.main').getBoundingClientRect().left <= 1);
  await chat.waitForFunction(() => Math.abs(document.querySelector('.main').getBoundingClientRect().left) <= 1);
  ensure(await chat.locator('#sidebarToggle').getAttribute('aria-expanded') === 'false', 'Android drawer remained expanded after selecting a conversation');
  ensure(await chat.locator('.message').count() === 2, 'Android synthetic conversation failed to render');
  const bubbleGeometry = await chat.evaluate(() => {
    const user = document.querySelector('.message.user .bubble').getBoundingClientRect();
    const reply = document.querySelector('.message.assistant .bubble').getBoundingClientRect();
    return { userRight: user.right, replyLeft: reply.left, width: window.innerWidth,
      scrollWidth: document.documentElement.scrollWidth };
  });
  ensure(bubbleGeometry.scrollWidth <= bubbleGeometry.width + 1, 'Android chat bubbles overflow viewport');
  ensure(bubbleGeometry.userRight > bubbleGeometry.replyLeft, 'Android chat message alignment is incorrect');
  await chat.locator('#prompt').fill('Synthetic draft stays intact');
  await chat.screenshot({ path: path.join(screenshotFolder, 'native-android-chat.png'), fullPage: true });
  await chat.locator('#sidebarToggle').click();
  await chat.waitForFunction(() => {
    const side = document.getElementById('sidebar').getBoundingClientRect();
    const main = document.querySelector('.main').getBoundingClientRect();
    return document.getElementById('sidebar').classList.contains('open') &&
      side.left >= -1 && Math.abs(side.right - innerWidth) <= 2 &&
      Math.abs(main.width - innerWidth) <= 2 &&
      getComputedStyle(document.querySelector('.main')).visibility === 'hidden';
  });
  const chatSidebarGeometry = await chat.evaluate(() => ({
    sidebarLeft: document.getElementById('sidebar').getBoundingClientRect().left,
    sidebarRight: document.getElementById('sidebar').getBoundingClientRect().right,
    mainLeft: document.querySelector('.main').getBoundingClientRect().left,
    mainInert: document.querySelector('.main').inert,
    mainWidth: document.querySelector('.main').getBoundingClientRect().width,
    viewport: innerWidth,
    shellScroll: document.querySelector('.shell').scrollLeft,
    windowScroll: scrollX,
    visualLeft: visualViewport?.offsetLeft,
    rootLeft: document.documentElement.getBoundingClientRect().left,
    shellLeft: document.querySelector('.shell').getBoundingClientRect().left,
    focusedId: document.activeElement?.id,
  }));
  ensure(chatSidebarGeometry.sidebarLeft >= -1 &&
    Math.abs(chatSidebarGeometry.sidebarRight - chatSidebarGeometry.viewport) <= 2 &&
    Math.abs(chatSidebarGeometry.mainWidth - chatSidebarGeometry.viewport) <= 2 &&
    chatSidebarGeometry.mainInert &&
    chatSidebarGeometry.shellScroll === 0 && chatSidebarGeometry.windowScroll === 0,
    'Android sidebar must cover populated chat without changing its width or scrolling: ' +
    JSON.stringify(chatSidebarGeometry));
  ensure(await chat.locator('.message').count() === 2 &&
    (await chat.locator('.message.assistant').textContent()).includes('define milestones'),
    'Opening Android sidebar must retain the currently selected chat and message history');
  await chat.screenshot({ path: path.join(screenshotFolder, 'native-android-sidebar-over-chat.png'), fullPage: true });
  await chat.locator('#sidebarToggle').click();
  await chat.waitForFunction(() => !document.querySelector('.main').inert &&
    getComputedStyle(document.querySelector('.main')).visibility === 'visible');
  ensure(await chat.locator('.message').count() === 2 &&
    (await chat.locator('.message.assistant').innerText()).includes('define milestones'),
    'Closing Android full-screen sidebar must restore exactly the same chat without starting a new conversation');
  ensure(await chat.locator('#prompt').inputValue() === 'Synthetic draft stays intact',
    'Opening full-screen sidebar cleared the unsent Android composer draft');

  // Long synthetic history validates that Projects+Chats scroll internally while
  // Plugins/Skills/Pets, Search, New chat and Settings stay fixed as in COS.
  const manyChats = await browser.newPage({
    viewport: { width: 390, height: 680 }, isMobile: true, hasTouch: true,
    userAgent: 'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36',
  });
  manyChats.on('pageerror', error => errors.push(error.message));
  const seededSessions = Array.from({ length: 36 }, (_, i) => ({
    id: 'test-session-' + i, provider: 'openai', model: 'qa',
    messages: [{ role: 'user', text: 'Android workspace conversation ' + (i + 1) },
      {role:'assistant', text:i === 6 ? 'Unique synthetic answer about semaphore-qa-search' : 'Generic reply'}],
  }));
  await manyChats.route('**/api/state', async route => {
    const response = await route.fetch();
    const payload = await response.json();
    await route.fulfill({ response, json: { ...payload, platform: 'android', sessions: seededSessions } });
  });
  await manyChats.goto(app.url);
  await manyChats.waitForFunction(() => document.documentElement.classList.contains('native-android'));
  ensure(!(await manyChats.locator('#sidebar').evaluate(node => node.classList.contains('open'))) &&
    await manyChats.locator('#sidebarBackdrop').evaluate(node => node.classList.contains('hidden')),
    'Android sidebar must start closed');
  await manyChats.locator('#sidebarToggle').click();
  await manyChats.waitForFunction(() => Math.abs(document.getElementById('sidebar').getBoundingClientRect().width - innerWidth) <= 2);
  ensure(await manyChats.evaluate(() => document.activeElement?.id !== 'sidebarSearch'),
    'Sidebar toggle unexpectedly focused Search for long chat history');
  await manyChats.locator('#sidebarSearchButton').click();
  ensure(await manyChats.evaluate(() => document.activeElement?.id === 'sidebarSearch'),
    'Tapping Search did not focus input for long chat history');
  ensure(await manyChats.locator('#threads button').count() === 36, 'Synthetic chat history not rendered');
  await manyChats.locator('#chatsToggle').click();
  ensure(await manyChats.locator('#chatsToggle').getAttribute('aria-expanded') === 'false' &&
    !(await manyChats.locator('#threads').isVisible()),
    'Long Chats list did not collapse');
  await manyChats.locator('#chatsToggle').click();
  ensure(await manyChats.locator('#threads button').count() === 36 &&
    await manyChats.locator('#threads').isVisible(),
    'Expanding Chats lost synthetic history');
  const beforeScroll = await manyChats.evaluate(() => {
    const scroll = document.getElementById('sidebarScrollRegion');
    return { height: scroll.clientHeight, content: scroll.scrollHeight,
      fixedNavY: document.querySelector('.sidebar-mobile-nav').getBoundingClientRect().top,
      footerY: document.getElementById('sidebarCustomize').getBoundingClientRect().top };
  });
  ensure(beforeScroll.content > beforeScroll.height + 250, 'Projects/Chats region cannot scroll despite long history');
  await manyChats.locator('#sidebarScrollRegion').evaluate(node => { node.scrollTop = node.scrollHeight; });
  const afterScroll = await manyChats.evaluate(() => ({
    top: document.getElementById('sidebarScrollRegion').scrollTop,
    fixedNavY: document.querySelector('.sidebar-mobile-nav').getBoundingClientRect().top,
    footerY: document.getElementById('sidebarCustomize').getBoundingClientRect().top,
  }));
  ensure(afterScroll.top > 250, 'Scrolling long chat list did not move its scroll container');
  ensure(Math.abs(beforeScroll.fixedNavY - afterScroll.fixedNavY) < 1, 'Plugins/Skills/Pets moved while chats scrolled');
  ensure(Math.abs(beforeScroll.footerY - afterScroll.footerY) < 1, 'Settings footer moved while chats scrolled');
  await manyChats.locator('#sidebarScrollRegion').evaluate(node => { node.scrollTop = 0; });
  await manyChats.locator('#sidebarSearch').fill('conversation 7');
  ensure(await manyChats.locator('#threads button').count() === 1, 'Chat search did not filter titles');
  ensure((await manyChats.locator('#threads').innerText()).includes('conversation 7'), 'Search returned wrong chat');
  await manyChats.locator('#sidebarSearch').fill('semaphore-qa-search');
  ensure(await manyChats.locator('#threads button').count() === 1 &&
    (await manyChats.locator('#threads').innerText()).includes('conversation 7'),
    'Search must inspect later assistant text in actual persisted chats, not only the first title');
  await manyChats.locator('#sidebarSearch').fill('no-exact-match');
  ensure((await manyChats.locator('#threads').innerText()).includes('No matching chats'), 'Missing no-results search state');
  await manyChats.locator('#sidebarSearch').fill('');
  await manyChats.locator('#projectAdd').click();
  await manyChats.locator('#projectAddGroup').click();
  await manyChats.locator('#projectName').fill('Android QA project');
  await manyChats.locator('#projectForm button[type="submit"]').click();
  ensure(await manyChats.locator('#projectsList').getByText('Android QA project').isVisible(), 'Project creation failed');
  await manyChats.locator('#projectRename').click();
  await manyChats.locator('#projectName').fill('Renamed Android QA project');
  await manyChats.locator('#projectForm button[type="submit"]').click();
  ensure(await manyChats.locator('#projectsList').getByText('Renamed Android QA project').isVisible(),
    'Renaming COS-style project failed');
  ensure((await manyChats.locator('#threads').innerText()).includes('No chats in this project'), 'New project should begin empty');
  await manyChats.locator('#projectsList button').first().click();
  await manyChats.locator('#threads button').first().click();
  await manyChats.locator('#sidebarToggle').click();
  await manyChats.waitForFunction(() => Math.abs(document.getElementById('sidebar').getBoundingClientRect().width - innerWidth) <= 2);
  await manyChats.locator('#projectsList').getByText('Renamed Android QA project').click();
  await manyChats.locator('#projectAssign').click();
  ensure(await manyChats.locator('#threads button').count() === 1, 'Assigning existing chat to project failed');
  const storedProjects = await manyChats.evaluate(() => JSON.parse(localStorage.getItem('mcp-agent-mobile-projects-v1')));
  ensure(storedProjects.projects.length === 1 && Object.keys(storedProjects.assignments).length === 1,
    'Project grouping not stored locally');
  await manyChats.locator('#projectsToggle').click();
  ensure(!(await manyChats.locator('#projectsContent').isVisible()), 'Projects collapse did not work');
  await manyChats.locator('#projectsToggle').click();
  ensure(await manyChats.locator('#projectsContent').isVisible(), 'Projects expand did not work');
  await manyChats.screenshot({ path: path.join(screenshotFolder, 'native-android-projects.png'), fullPage: true });
  await manyChats.reload();
  await manyChats.locator('#sidebarToggle').click();
  await manyChats.locator('#projectsList').getByText('Renamed Android QA project').waitFor({ state: 'visible' });
  ensure(await manyChats.locator('#projectsList').getByText('Renamed Android QA project').isVisible(), 'Local project rename did not survive reload');
  await manyChats.locator('#projectsList').getByText('Renamed Android QA project').click();
  manyChats.once('dialog', dialog => dialog.accept());
  await manyChats.locator('#projectRemove').click();
  ensure(await manyChats.locator('#projectsList').getByText('Renamed Android QA project').count() === 0,
    'Removing a project label did not remove the grouping');
  ensure(await manyChats.locator('#threads button').count() === 36,
    'Removing a project label must not delete chats');
  await manyChats.close();
  // Step3 grants native SIWC plan inference; Android chat consumes that grant
  // through Responses API, independent of the private website WebView and MCP.
  const connection = await browser.newPage({
    viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true,
    userAgent: 'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36',
  });
  connection.on('pageerror', error => errors.push(error.message));
  let completedWebsiteSession = null;
  let directProviderChatCalls = 0;
  await connection.exposeFunction('__recordChatgptWebsiteSession', session => {
    completedWebsiteSession = session;
  });
  await connection.route('**/api/state', async route => {
    const response = await route.fetch();
    const payload = await response.json();
    await route.fulfill({ response, json: { ...payload,
      platform: 'android', providers: ['chatgpt-browser'],
      hasKeys: [], sessions: completedWebsiteSession ? [completedWebsiteSession] : [] } });
  });
  await connection.route('**/api/key', async route => {
    await route.fulfill({status:403,json:{error:'Android ChatGPT-only mode has no provider API key'}});
  });
  await connection.route('**/api/chat', async route => {
    directProviderChatCalls++;
    await route.fulfill({status:403,json:{error:'Android ChatGPT must not call a provider API'}});
  });
  await connection.route('**/api/standing-rules', async route => {
    const args = route.request().postDataJSON();
    await route.fulfill({json:{sessionId:args.sessionId ?? null,
      session:{enabled:false,text:''}}});
  });
  await connection.goto(app.url);
  await connection.evaluate(() => {
    window.isTauri = true;
    window.__websiteMock = {supported:true,running:false,visible:false,
      signInVerified:false,sessionLinked:false,
      engine:'Android owned WebView (separate profile)',message:'Synthetic ChatGPT website'};
    window.__websiteAccount = 'signed_out';
    window.__websiteCalls = [];
    window.__websiteTurn = null;
    window.__websiteSendCount = 0;
    window.__websiteOAuth = {supported:true,state:'signed_out',pending:false,
      signInVerified:false,sessionLinked:false,planEnabled:false,
      message:'Connect with ChatGPT and approve Mcp Agent Ai in Chrome.'};
    window.__websiteOAuthCalls = [];
    window.__siwcChatCalls = [];
    window.__siwcChatMode = 'completed';
    window.__siwcSession = null;
    window.__websiteOAuthApproval = 'waiting';
    window.__websiteOAuthBringReject = false;
    window.__TAURI_INTERNALS__ = {
      invoke: async (command,args={}) => {
        const native = window.__websiteMock;
        if (command === 'api_request') {
          // The test browser emulates native IPC while still using the
          // Playwright-routed Rust API for state and standing rules.
          const response = await fetch(args.route, {
            method: args.body == null ? 'GET' : 'POST',
            headers: args.body == null ? undefined : {'Content-Type':'application/json'},
            body: args.body == null ? undefined : JSON.stringify(args.body),
          });
          return response.json();
        }
        if (command === 'android_permissions_status') return {
          files:{supported:true,granted:true},overlay:{supported:true,granted:true},
          notifications:{supported:true,granted:true},battery:{supported:true,granted:true},
        };
        if (command === 'accessibility_status') return {
          supported:true,enabled:true,connected:true,needsReconnect:false,
        };
        if (command.startsWith('android_chatgpt_oauth_')) {
          const action = command.slice('android_chatgpt_oauth_'.length);
          window.__websiteOAuthCalls.push(action);
          const oauth = window.__websiteOAuth;
          if (action === 'status') {
            if (window.__websiteOAuthApproval === 'approved' && oauth.state === 'authorizing')
              Object.assign(oauth,{state:'verified',pending:false,signInVerified:true,
                sessionLinked:false,planEnabled:true,message:'Native OpenAI plan authorization verified.'});
            if (window.__websiteOAuthApproval === 'revoked')
              Object.assign(oauth,{state:'signed_out',pending:false,signInVerified:false,
                sessionLinked:false,planEnabled:false,message:'Authorization revoked.'});
            return structuredClone(oauth);
          }
          if (action === 'start') {
            if (args.userInitiated !== true) throw Error('Native OAuth requires explicit Sign in');
            window.__websiteOAuthApproval = 'waiting';
            Object.assign(oauth,{state:'authorizing',pending:true,signInVerified:false,
              sessionLinked:false,planEnabled:false,message:'Waiting for Mcp Agent Ai approval in Chrome.'});
            return structuredClone(oauth);
          }
          if (action === 'bring') {
            if (args.userInitiated !== true || oauth.state !== 'verified' ||
                !oauth.signInVerified || !oauth.planEnabled || window.__websiteOAuthBringReject)
              throw Error('Native OpenAI approval and plan verification are required');
            Object.assign(oauth,{state:'linked',pending:false,sessionLinked:true,
              message:'Mcp Agent Ai approved and OpenAI plan access verified.'});
            return structuredClone(oauth);
          }
          throw Error('Unexpected native OAuth command '+command);
        }
        if (command === 'chatgpt_oauth_chat') {
          if (window.__websiteOAuth.state !== 'linked' || !window.__websiteOAuth.planEnabled ||
              args.userInitiated !== true || args.sessionId !== (window.__siwcSession?.id ?? null) ||
              typeof args.prompt !== 'string' || !args.prompt.trim() ||
              !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(args.requestId))
            throw Error('Native SIWC request is not authorized or not scoped');
          window.__siwcChatCalls.push({id:args.requestId,prompt:args.prompt});
          if (window.__siwcChatMode !== 'completed')
            return {requestId:args.requestId,state:window.__siwcChatMode,
              reason:'subscription_sharing_usage_limit_exceeded'};
          const sessionId='1ce23b91-a226-49e8-8248-ac4ed602164b';
          const session = {id:sessionId,
            provider:'chatgpt-browser',model:'chatgpt.com',
            actions:[...(window.__siwcSession?.actions ?? []),
              {type:'chatgpt_oauth_turn',nativeConfirmed:true,mcpToolsUsed:false}],
            messages:[...(window.__siwcSession?.messages ?? []),{role:'user',text:args.prompt},
              {role:'assistant',text:'Synthetic SIWC Responses completed answer'}]};
          window.__siwcSession = session;
          await window.__recordChatgptWebsiteSession(session);
          return {requestId:args.requestId,state:'completed',sessionId,
            answer:'Synthetic SIWC Responses completed answer',saved:true,
            status:'confirmed',actions:[]};
        }
        if (command.startsWith('android_chatgpt_')) {
          const action = command.slice('android_chatgpt_'.length);
          window.__websiteCalls.push(action);
          if (action === 'status') {
            if (native.running && window.__websiteAccount === 'signed_in') {
              native.signInVerified = true;
              native.visible = false;
            }
            if (window.__websiteAccount === 'signed_out') {
              native.signInVerified = false;
              native.sessionLinked = false;
            }
            return structuredClone(native);
          }
          if (action === 'open') {
            if (args?.userInitiated !== true) throw Error('Open requires direct consent');
            native.running = true;
            native.visible = true;
            native.signInVerified = false;
            native.sessionLinked = false;
            return structuredClone(native);
          }
          if (action === 'check_session' || action === 'bring_session') {
            if (!native.running) throw Error('Owned browser not running');
            if (action === 'bring_session' &&
                (args?.userInitiated !== true || !native.signInVerified || native.visible))
              throw Error('Bring requires verified sign-in and consent');
            native.signInVerified = window.__websiteAccount === 'signed_in';
            if (native.signInVerified) native.visible = false;
            else native.sessionLinked = false;
            if (action === 'bring_session') native.sessionLinked = native.signInVerified;
            return {owned:true,origin:'chatgpt.com',pageGeneration:1,readOnly:true,
              signInState:window.__websiteAccount,sessionLinked:native.sessionLinked,
              ...(window.__websiteForgeReceipt ? {cookie:'NOT_ALLOWED'} : {})};
          }
          if (action === 'probe') return {owned:true,origin:'chatgpt.com',
            readOnly:true,pageGeneration:1,composerReady:native.signInVerified,
            sendButtonReady:native.signInVerified};
          if (action === 'hide' || action === 'show') {
            native.visible = action === 'show';
            return structuredClone(native);
          }
          throw Error('Unexpected Android ChatGPT command: '+command);
        }
        if (command.startsWith('chatgpt_browser_')) {
          const action = command.slice('chatgpt_browser_'.length);
          window.__websiteCalls.push('turn:'+action);
          let turn = window.__websiteTurn;
          if (action === 'prepare') {
            if (!native.sessionLinked || turn || args.sessionId !== null ||
                typeof args.prompt !== 'string' || !args.prompt.trim())
              throw Error('Private ChatGPT page not brought into app session');
            turn = {requestId:args.requestId,prompt:args.prompt,sent:false};
            window.__websiteTurn = turn;
            return {requestId:args.requestId,state:'prepared',accepted:false,completed:false};
          }
          if (!turn || args.requestId !== turn.requestId) throw Error('Wrong turn id');
          if (action === 'cancel') {
            if (turn.sent) throw Error('Send may already have occurred');
            window.__websiteTurn = null;
            return {requestId:args.requestId,state:'cancelled',accepted:false,completed:false};
          }
          if (action === 'send') {
            if (args.userInitiated !== true || turn.sent) throw Error('Non-consensual or duplicate Send');
            turn.sent = true;
            window.__websiteSendCount++;
            return {requestId:args.requestId,state:'submitted',accepted:false,completed:false};
          }
          if (action === 'observe') return turn.sent
            ? {requestId:args.requestId,state:'completed',accepted:true,completed:true,
                answer:'Synthetic ChatGPT website confirmed answer'}
            : {requestId:args.requestId,state:'prepared',accepted:false,completed:false};
          if (action === 'finalize') {
            if (!turn.sent) throw Error('No verified ChatGPT assistant answer');
            const sessionId='1ce23b91-a226-49e8-8248-ac4ed602164b';
            await window.__recordChatgptWebsiteSession({id:sessionId,
              provider:'chatgpt-browser',model:'chatgpt.com',
              messages:[{role:'user',text:turn.prompt},
                {role:'assistant',text:'Synthetic ChatGPT website confirmed answer'}]});
            window.__websiteTurn = null;
            return {sessionId,answer:'Synthetic ChatGPT website confirmed answer',actions:[]};
          }
          throw Error('Unknown browser turn action');
        }
        throw Error('Synthetic Android command unavailable: '+command);
      },
    };
  });
  await connection.waitForFunction(() => document.getElementById('sidebarConnectionStatus').dataset.status === 'disconnected');
  await connection.locator('#sidebarToggle').click();
  await connection.waitForFunction(() => Math.abs(document.getElementById('sidebar').getBoundingClientRect().width - innerWidth) <= 2);
  ensure((await connection.locator('#sidebarConnectionStatus').textContent()).trim() === 'Disconnected',
    'Android initial provider status must be Disconnected');
  await connection.screenshot({ path: path.join(screenshotFolder, 'native-android-status-disconnected.png'), fullPage: true });
  await connection.locator('#sidebarCustomize').click();
  await connection.locator('[data-android-settings-category="setup"]').click();
  ensure(await connection.locator('#connectionSection').isHidden() &&
    await connection.locator('#backgroundChatgptSettings').isHidden(),
    'Hidden legacy connection widgets must not appear during synthetic provider status tests');
  // Step8 remains the native OpenAI MCP tunnel. Even an approved official
  // ChatGPT OAuth grant cannot replace a Core ID or a real inbound tool call.
  await connection.locator('[data-cos-setup-step="connect"]').click();
  ensure(await connection.locator('#androidCosChatgptConnect').isVisible() &&
    await connection.locator('#androidCosTunnelDisconnect').isVisible() &&
    await connection.locator('#androidCosChatgptOpen,#androidCosChatgptBring,#androidCosChatgptCheck,#androidCosPrepareTest').count() === 0 &&
    await connection.locator('#androidCosProvider,#androidCosModel,#androidCosApiKey').count() === 0 &&
    await connection.locator('#provider').inputValue() === 'chatgpt-browser',
    'Step8 must expose only native tunnel controls, with no second website sign-in or provider selector');
  if (await connection.locator('#androidCosChatgptConnect').isEnabled())
    await connection.locator('#androidCosChatgptConnect').click();
  ensure(await connection.locator('[data-cos-setup-step="connect"] small').innerText() !== 'Verified' &&
    await connection.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() !== 'Verified' &&
    (await connection.evaluate(() => window.__websiteOAuthCalls)).every(call =>
      !['start','bring'].includes(call)) &&
    (await connection.evaluate(() => window.__websiteCalls)).every(call =>
      !['open','bring_session','turn:send','turn:finalize'].includes(call)),
    'Step8 cannot connect a tunnel without Core ID/authorized key or trigger either ChatGPT sign-in');
  await connection.screenshot({
    path:path.join(screenshotFolder,'native-android-step8-tunnel-missing-prerequisites-390.png'),fullPage:true,
  });
  // Step3 opens the native Chrome authorization, which remains pending until
  // the user approves the named app. It never imports website browser cookies.
  await connection.locator('[data-cos-setup-step="browser"]').click();
  await connection.waitForFunction(() => document.getElementById('androidCosBrowserOpen')?.disabled === false);
  await connection.locator('#androidCosBrowserOpen').click();
  await connection.waitForFunction(() => window.__websiteOAuthCalls.includes('start'));
  ensure(await connection.locator('#androidCosBrowserBring').isDisabled() &&
    await connection.locator('[data-cos-setup-step="browser"] small').innerText() !== 'Verified' &&
    (await connection.evaluate(() => window.__websiteCalls)).every(call => call !== 'open'),
    'Native Chrome login without app approval cannot satisfy Step3 or open a website WebView');
  await connection.evaluate(() => {
    window.__websiteOAuthApproval = 'approved';
    window.dispatchEvent(new Event('focus'));
  });
  await connection.waitForFunction(() => document.getElementById('androidCosBrowserBring')?.disabled === false,
    {timeout:12000});
  ensure(await connection.locator('[data-cos-setup-step="browser"] small').innerText() !== 'Verified',
    'OAuth account approval alone must not complete Step3 without explicit Bring');
  await connection.locator('#androidCosBrowserBring').click();
  await connection.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="browser"] small')?.textContent === 'Verified');
  ensure((await connection.evaluate(() => window.__websiteOAuthCalls)).includes('bring') &&
    !(await connection.evaluate(() => window.__websiteCalls)).includes('bring_session') &&
    (await connection.evaluate(() => window.__websiteSendCount)) === 0 &&
    await connection.locator('[data-cos-setup-step="connect"] small').innerText() !== 'Verified',
    'Step3 linked OAuth grant cannot mark Step8 tunnel connected or link WebView cookies');
  await connection.locator('[data-cos-setup-step="connect"]').click();
  ensure(await connection.locator('[data-cos-setup-step="connect"] small').innerText() !== 'Verified' &&
    await connection.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() !== 'Verified',
    'A linked ChatGPT OAuth grant is not the Core tunnel or proof of an inbound MCP tool call');
  await connection.screenshot({
    path:path.join(screenshotFolder,'native-android-step8-tunnel-with-chatgpt-step3-390.png'),fullPage:true,
  });
  // The OAuth grant, not chatgpt.com WebView cookies, supplies eligible
  // Responses inference. No extra website login or MCP plugin is required.
  await closeNativeAndroidSettings(connection);
  await connection.locator('#prompt').fill('Reply with OK using my approved ChatGPT plan.');
  ensure(await connection.locator('#settingsPanel').isHidden() &&
    (await connection.evaluate(() => window.__websiteSendCount)) === 0,
    'Bringing OpenAI OAuth at Step3 must never automatically send or fabricate a website reply');
  connection.once('dialog', dialog => dialog.accept());
  await connection.locator('#send').click();
  await connection.waitForFunction(() =>
    window.__siwcChatCalls.length === 1 &&
    document.getElementById('messages')?.textContent.includes('Synthetic SIWC Responses completed answer'));
  await connection.locator('#sidebarToggle').click();
  await connection.waitForFunction(() => Math.abs(document.getElementById('sidebar').getBoundingClientRect().width - innerWidth) <= 2);
  ensure(directProviderChatCalls === 0 &&
    (await connection.evaluate(() => window.__websiteSendCount)) === 0 &&
    (await connection.evaluate(() => window.__siwcChatCalls)).length === 1 &&
    !(await connection.evaluate(() => window.__websiteCalls)).includes('turn:prepare'),
    'Native SIWC Responses reply must succeed without website WebView or MCP and never fall back to legacy /api/chat');
  await connection.locator('#sidebarToggle').click();
  await connection.waitForFunction(() =>
    document.getElementById('sidebarToggle')?.getAttribute('aria-expanded') === 'false');
  await connection.locator('#prompt').fill('Continue this authorized chat without MCP.');
  connection.once('dialog', dialog => dialog.accept());
  await connection.locator('#send').click();
  await connection.waitForFunction(() => window.__siwcChatCalls.length === 2 &&
    document.getElementById('messages')?.textContent.includes('Continue this authorized chat'));
  ensure((await connection.evaluate(() => window.__siwcChatCalls))[1].prompt ===
    'Continue this authorized chat without MCP.' &&
    (await connection.evaluate(() => window.__siwcSession)).actions.length === 2 &&
    (await connection.evaluate(() => window.__websiteSendCount)) === 0,
    'SIWC follow-up must use saved OAuth conversation only and must never dispatch a website or MCP Send');
  await connection.evaluate(() => { window.__siwcChatMode = 'uncertain'; });
  await connection.locator('#prompt').fill('A separate native error classification test.');
  connection.once('dialog', dialog => dialog.accept());
  await connection.locator('#send').click();
  await connection.waitForFunction(() => document.body.textContent.includes('usage allowance') &&
    window.__siwcChatCalls.length === 3);
  ensure((await connection.evaluate(() => window.__siwcSession)).actions.length === 2 &&
    (await connection.locator('#prompt').inputValue()) === 'A separate native error classification test.' &&
    (await connection.evaluate(() => window.__websiteSendCount)) === 0,
    'Uncertain SIWC request must preserve draft, avoid saving and never retry or dispatch website Send');
  await connection.screenshot({ path: path.join(screenshotFolder, 'native-android-chatgpt-reply-verified.png'), fullPage: true });
  await connection.locator('#sidebarToggle').click();
  await connection.locator('#sidebarCustomize').click();
  await connection.locator('[data-android-settings-category="setup"]').click();
  await connection.locator('[data-cos-setup-step="connect"]').click();
  ensure(await connection.locator('[data-cos-setup-step="connect"] small').innerText() !== 'Verified' &&
    await connection.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() !== 'Verified',
    'Receiving an SIWC assistant reply without a native Secure MCP tunnel cannot verify Steps 8 or 9');
  await connection.evaluate(() => {
    window.__websiteOAuthApproval = 'revoked';
    window.dispatchEvent(new Event('focus'));
  });
  await connection.locator('[data-cos-setup-step="browser"]').click();
  await connection.locator('[data-cos-setup-step="connect"]').click();
  await connection.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="browser"] small')?.textContent !== 'Verified');
  ensure(await connection.locator('[data-cos-setup-step="connect"] small').innerText() !== 'Verified' &&
    directProviderChatCalls === 0,
    'Revoking native OAuth invalidates Step3, without inventing any tunnel or altering website chat');
  await connection.close();
  // Reproduce fresh application Settings->Setup entry using a complete,
  // independent native backend fixture for each state. This is intentionally
  // not a visual-only mock: every Setup badge comes from the same checked
  // invoke receipts used in the installed Android renderer, and each page has
  // its own sessionStorage and async call ledger.
  const setupOpenScenarios = [
    {label:'all-verified',allRequired:true,extension:'verified',oauth:'linked',
      key:'verified',expected:'ready',ready:'Verified',stale:'extension'},
    {label:'extension-skipped-browser-pending',allRequired:true,extension:'notNeeded',
      oauth:'signed_out',key:'verified',expected:'browser',stale:'extension'},
    {label:'permissions-pending',allRequired:false,extension:'verified',
      oauth:'linked',key:'verified',expected:'access',stale:'extension'},
    {label:'step7-pending-after-verified-extension',allRequired:true,extension:'verified',
      oauth:'linked',key:'missing',expected:'key',stale:'extension'},
    {label:'stale-step2-reentry',allRequired:true,extension:'verified',
      oauth:'linked',key:'verified',expected:'ready',ready:'Verified',stale:'extension',
      reopen:'instant'},
    {label:'verified-reopen-native-revocation',allRequired:true,extension:'verified',
      oauth:'linked',key:'verified',expected:'ready',ready:'Verified',stale:'extension',
      reopen:'revoked'},
    {label:'verified-reopen-manual-overrides-scan',allRequired:true,extension:'verified',
      oauth:'linked',key:'verified',expected:'ready',ready:'Verified',stale:'extension',
      reopen:'manual'},
    {label:'cold-unverified-scans-with-checking',allRequired:false,extension:'verified',
      oauth:'linked',key:'missing',expected:'access',stale:'extension',delayed:true},
    {label:'cold-unverified-forged-local-state',allRequired:false,extension:'verified',
      oauth:'linked',key:'missing',expected:'access',stale:'extension',delayed:true,
      forgedLocal:true},
    {label:'slow-backend-manual-tab',allRequired:true,extension:'verified',
      oauth:'linked',key:'verified',expected:'ready',stale:'extension',
      delayed:true,manual:'mcpPlugin'},
    {label:'tunnel-native-auto-follow',allRequired:true,extension:'verified',
      oauth:'linked',key:'verified',tunnel:'connecting',expected:'connect',
      followTunnel:'automatic',stale:'extension'},
    {label:'tunnel-native-manual-step8',allRequired:true,extension:'verified',
      oauth:'linked',key:'verified',tunnel:'connecting',expected:'connect',
      followTunnel:'manualStep8',stale:'extension'},
    {label:'tunnel-native-manual-other',allRequired:true,extension:'verified',
      oauth:'linked',key:'verified',tunnel:'connecting',expected:'connect',
      followTunnel:'manualOther',stale:'extension'},
  ];
  async function runSetupOpenScenario(scenario) {
    const nativePage = await browser.newPage({
      viewport:{width:390,height:844},isMobile:true,hasTouch:true,
      userAgent:'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36'
    });
    nativePage.on('pageerror', error => errors.push(error.message));
    await nativePage.addInitScript(stale => {
      sessionStorage.setItem('mcp-agent-ai-android-setup-step-v1',stale);
    }, scenario.stale);
    if (scenario.forgedLocal) {
      await nativePage.addInitScript(() => {
        // Untrusted web storage is NOT a native OS permission or tunnel proof.
        // A cold app must not trust a forged all-Verified snapshot.
        for (const key of ['mcp-agent-ai-android-setup-snapshot-v1',
          'mcp-agent-ai-android-setup-verified-v1',
          'mcp-agent-ai-android-setup-complete-v1'])
          localStorage.setItem(key,JSON.stringify({verified:true,ready:true,
            completed:10,steps:Object.fromEntries(
              ['access','extension','browser','folder','tools','tunnel','key',
                'connect','mcpPlugin','ready'].map(step=>[step,'Verified']))}));
        // Exact current cosmetic hint is also untrusted: it can affect how
        // the spinner looks, never the native-backed statuses or actions.
        localStorage.setItem('mcp-agent-ai-android-setup-completed-hint-v1','1');
      });
    }
    await nativePage.goto(app.url);
    await nativePage.evaluate(config => {
      window.isTauri = true;
      const tunnelId='tunnel_'+'a'.repeat(32);
      const capabilities=['browseFolders','searchFiles','readFiles','fileMetadata',
        'createFiles','editFiles','moveFiles','deleteFiles','browserControl','terminal'];
      const mask=Object.fromEntries(capabilities.map((name,i)=>[name,i<4]));
      window.__setupScenario={
        calls:[],tunnelId,
        config:structuredClone(config),pendingPermissionStatus:null,
        releasePermissionStatus:null,permissionReleased:!config.delayed,
        pauseNextPermissionStatus:false,permissionsRevoked:false,
        tunnelReceipt:config.key!=='verified'
          ? {state:'disconnected',pollActive:false,toolCallsVerified:0}
          : config.tunnel==='connecting'
            ? {state:'connecting',pollActive:false,toolCallsVerified:0,desiredConnected:true}
            : {state:'tool_verified',pollActive:true,toolCallsVerified:2,desiredConnected:true},
      };
      const s=window.__setupScenario;
      const permissions=()=>Object.fromEntries(['files','overlay','notifications','battery']
        .map(permission=>[permission,{supported:true,
          granted:config.allRequired && !s.permissionsRevoked}]));
      const oauthLinked=config.oauth==='linked';
      const auth={supported:true,state:oauthLinked?'linked':'signed_out',
        pending:false,signInVerified:oauthLinked,sessionLinked:oauthLinked,
        planEnabled:oauthLinked,message:oauthLinked?'Synthetic native OAuth linked':'Sign in required'};
      const ext=config.extension==='verified'
        ? [{id:'setup-native-probe@example.com',name:'Native extension',
          version:'1.0',enabled:true,builtIn:true}] : [];
      window.__TAURI_INTERNALS__={
        invoke:async (command,args) => {
          s.calls.push({command,args:args?structuredClone(args):null});
          if (command==='android_permissions_status') {
            if ((config.delayed && !s.permissionReleased) || s.pauseNextPermissionStatus) {
              if (!s.pendingPermissionStatus)
                s.pendingPermissionStatus=new Promise(resolve => {s.releasePermissionStatus=()=>{
                  s.permissionReleased=true;s.pauseNextPermissionStatus=false;
                  s.pendingPermissionStatus=null;
                  resolve(permissions());
                };});
              return s.pendingPermissionStatus;
            }
            return permissions();
          }
          if (command==='accessibility_status')
            return {supported:true,enabled:config.allRequired,connected:config.allRequired,
              needsReconnect:false};
          if (command==='android_optional_permissions_status')
            return Object.fromEntries(['camera','microphone','location']
              .map(permission=>[permission,{supported:true,granted:false}]));
          if (command==='custom_gecko_list_extensions')
            return {runtimeStarted:true,extensions:structuredClone(ext)};
          if (command==='android_chatgpt_oauth_status') return structuredClone(auth);
          if (command==='android_main_storage_status')
            return {supported:true,granted:config.allRequired,
              readable:config.allRequired,rootPath:'/storage/emulated/0'};
          if (command==='android_main_storage_selection_status')
            return {path:[],selected:false};
          if (command==='document_tree_status') return {granted:false,treeUri:''};
          if (command==='android_mcp_permissions_status')
            return {preset:'read_only',requested:structuredClone(mask),
              effective:structuredClone(mask),available:structuredClone(mask)};
          if (command==='android_tunnel_saved_id_status')
            return {tunnelId,verified:false};
          if (command==='android_tunnel_credentials_status')
            return {stored:config.key==='verified',secure:true};
          if (command==='android_tunnel_check_id')
            return {valid:true,formatOnly:true};
          if (command==='android_tunnel_verify_stored_access')
            return {verified:config.key==='verified',tunnelId,
              reason:config.key==='verified'?'ok':'key_required'};
          if (command==='android_openai_tunnel_status')
            return structuredClone(s.tunnelReceipt);
          if (command==='android_local_mcp_status')
            return {state:'stopped',localUrl:null,toolCallsHandled:0,remoteVerified:false};
          throw Error('Unexpected synthetic native command: '+command);
        },
      };
    },scenario);
    await nativePage.locator('#sidebarToggle').click();
    await nativePage.locator('#sidebarCustomize').click();
    await nativePage.locator('[data-android-settings-category="setup"]').click();
    if (scenario.delayed) {
      await nativePage.waitForFunction(() =>
        window.__setupScenario?.releasePermissionStatus &&
        window.__setupScenario.calls.some(call=>call.command==='android_chatgpt_oauth_status'));
      const coldChecking=await nativePage.locator('#androidCosSetupChecking').isVisible();
      const coldReady=await nativePage.locator('[data-cos-setup-step="ready"] small').innerText();
      const coldAccess=await nativePage.locator('[data-cos-setup-step="access"] small').innerText();
      ensure((scenario.forgedLocal
        ? !coldChecking && coldReady!=='Verified' && coldAccess!=='Verified'
        : coldChecking &&
          await nativePage.locator('[data-cos-setup-step][aria-current="step"]').count() === 0 &&
          coldReady!=='Verified'),
        'Cold Setup must display Checking without a prior hint, and forged localStorage completion must never generate native Verified badges');
      if (scenario.manual)
        await nativePage.locator(`[data-cos-setup-step="${scenario.manual}"]`).click();
      await nativePage.evaluate(() => window.__setupScenario.releasePermissionStatus());
    }
    await nativePage.waitForFunction(config => {
      const selected=document.querySelector('[data-cos-setup-step][aria-current="step"]')?.dataset.cosSetupStep;
      const status=document.querySelector('[data-cos-setup-step="'+config.expected+'"] small')?.textContent;
      const calls=window.__setupScenario?.calls.map(item=>item.command) || [];
      const required=['android_permissions_status','accessibility_status',
        'custom_gecko_list_extensions','android_chatgpt_oauth_status',
        'android_main_storage_status','android_mcp_permissions_status',
        'android_tunnel_saved_id_status','android_tunnel_credentials_status',
        'android_openai_tunnel_status'];
      return required.every(command=>calls.includes(command)) &&
        selected === (config.manual || config.expected) &&
        (config.ready ? status===config.ready : true) &&
        !calls.some(command=>['android_permissions_request','android_chatgpt_oauth_start',
          'android_openai_tunnel_connect_stored','android_openai_tunnel_connect',
          'android_tunnel_credentials_store'].includes(command));
    },scenario,{timeout:18000});
    const observed=await nativePage.evaluate(() => {
      const buttons=[...document.querySelectorAll('[data-cos-setup-step]')];
      return {
        selected:buttons.find(button=>button.getAttribute('aria-current')==='step')?.dataset.cosSetupStep,
        statuses:Object.fromEntries(buttons.map(button=>[button.dataset.cosSetupStep,
          button.querySelector('small')?.textContent])),
        savedStep:sessionStorage.getItem('mcp-agent-ai-android-setup-step-v1'),
        commands:window.__setupScenario.calls.map(item=>item.command),
        overflow:document.documentElement.scrollWidth-window.innerWidth,
      };
    });
    ensure(observed.selected === (scenario.manual || scenario.expected) &&
      observed.overflow<=1 &&
      (scenario.stale !== observed.selected || !!scenario.manual) &&
      await nativePage.locator('#androidCosSetupChecking').isHidden(),
      `Setup-on-open ${scenario.label} must focus the right backend-verified step without horizontal overflow: ${JSON.stringify(observed)}`);
    if (scenario.ready) {
      ensure(observed.statuses.ready===scenario.ready &&
        ['access','extension','browser','folder','tools','tunnel','key','connect','mcpPlugin']
          .every(step=>observed.statuses[step]==='Verified'),
        `Setup-on-open ${scenario.label} cannot show Step10 before the previous nine are verified: ${JSON.stringify(observed.statuses)}`);
    }
    if (scenario.forgedLocal) {
      const forgedHintAfterScan=await nativePage.evaluate(() =>
        localStorage.getItem('mcp-agent-ai-android-setup-completed-hint-v1'));
      ensure(forgedHintAfterScan===null && observed.statuses.access==='Pending' &&
        observed.statuses.ready!=='Verified',
        'Cold native revocation must erase a forged cosmetic completed hint without ever granting Verified');
    }
    if (scenario.label==='extension-skipped-browser-pending')
      ensure(observed.statuses.access==='Verified' &&
        observed.statuses.extension==='Not needed' &&
        observed.statuses.browser==='Pending' &&
        observed.selected==='browser',
        `Optional skipped Step2 must not steal focus from pending Step3: ${JSON.stringify(observed)}`);
    if (scenario.label==='step7-pending-after-verified-extension')
      ensure(['access','extension','browser','folder','tools','tunnel']
        .every(step=>observed.statuses[step]==='Verified') &&
        observed.statuses.key==='Pending' && observed.selected==='key',
        `A missing secure API key must select Step7 instead of Step2: ${JSON.stringify(observed)}`);
    if (scenario.label==='permissions-pending')
      ensure(observed.statuses.access==='Pending' && observed.selected==='access',
        `Missing permission must take precedence over enabled extension: ${JSON.stringify(observed)}`);
    if (scenario.delayed)
      ensure(observed.selected===(scenario.manual || scenario.expected) &&
        observed.statuses.access===(scenario.allRequired?'Verified':'Pending') &&
        (scenario.allRequired || observed.statuses.ready!=='Verified'),
        'Cold Setup scan must honor actual native permissions and explicit selection, never a forged localStorage receipt');
    if (scenario.followTunnel) {
      const prereqs=['access','extension','browser','folder','tools','tunnel','key'];
      ensure(prereqs.every(step=>observed.statuses[step]==='Verified') &&
        observed.selected==='connect' &&
        observed.statuses.connect==='Checking…' &&
        observed.statuses.mcpPlugin==='Pending' &&
        observed.statuses.ready!=='Verified',
        `Starting a reconnecting tunnel must select Step8 as the first unresolved native stage: ${JSON.stringify(observed.statuses)}`);
      // These receipts are supplied ONLY by android_openai_tunnel_status,
      // via the same read-only native poll path as a real installed app.
      // No synthetic DOM labels, manual Refresh, local MCP echo, or tool-call
      // counter alone can forge successful inbound MCP verification.
      const nextTunnel = async (receipt, selectedStep, badges, description) => {
        const previousReads=await nativePage.evaluate(next=>{
          const s=window.__setupScenario;
          const n=s.calls.filter(call=>call.command==='android_openai_tunnel_status').length;
          s.tunnelReceipt=structuredClone(next);
          return n;
        },receipt);
        await nativePage.waitForFunction(({previousReads,selectedStep,badges}) => {
          const current=document.querySelector('[data-cos-setup-step][aria-current="step"]')?.dataset.cosSetupStep;
          const reads=window.__setupScenario.calls.filter(call=>
            call.command==='android_openai_tunnel_status').length;
          return reads>previousReads && current===selectedStep &&
            Object.entries(badges).every(([step,badge])=>
              document.querySelector('[data-cos-setup-step="'+step+'"] small')?.textContent===badge);
        },{previousReads,selectedStep,badges},{timeout:15000});
        const proof=await nativePage.evaluate(() => {
          const s=window.__setupScenario;
          return {
            selected:document.querySelector('[data-cos-setup-step][aria-current="step"]')?.dataset.cosSetupStep,
            nativeReceipt:structuredClone(s.tunnelReceipt),
            commands:s.calls.map(call=>call.command),
            statuses:Object.fromEntries([...document.querySelectorAll('[data-cos-setup-step]')]
              .map(button=>[button.dataset.cosSetupStep,button.querySelector('small')?.textContent])),
          };
        });
        ensure(proof.selected===selectedStep &&
          Object.entries(badges).every(([step,badge])=>proof.statuses[step]===badge) &&
          !proof.commands.some(command=>[
            'android_openai_tunnel_connect','android_openai_tunnel_connect_stored',
            'android_permissions_request','android_chatgpt_oauth_start',
            'android_tunnel_credentials_store'].includes(command)),
          `Auto-follow ${scenario.followTunnel}: ${description}; native receipt proof ${JSON.stringify(proof)}`);
      };
      if (scenario.followTunnel==='manualStep8') {
        await nativePage.locator('[data-cos-setup-step="connect"]').click();
        ensure(await nativePage.locator('[data-cos-setup-step="connect"]').getAttribute('aria-current')==='step',
          'Explicit user selection of Step8 must be respected even when auto-scanner selected Step8 first');
      }
      await nextTunnel({state:'polling',pollActive:true,toolCallsVerified:0,desiredConnected:true},
        scenario.followTunnel==='manualStep8'?'connect':'mcpPlugin',
        {connect:'Verified',mcpPlugin:'Pending'},'connecting to polling must verify Step8 but not Step9');
      if (scenario.followTunnel==='automatic') {
        // This temporarily regresses the native tunnel; auto-follow should
        // remain at Step9 (rather than oscillating back to Step8), and then
        // resume following once the backend recovers.
        await nextTunnel({state:'error',pollActive:false,toolCallsVerified:0,desiredConnected:true},
          'mcpPlugin',{connect:'Pending',mcpPlugin:'Pending'},
          'transient network error must not bounce navigation backward');
        await nextTunnel({state:'polling',pollActive:true,toolCallsVerified:0,desiredConnected:true},
          'mcpPlugin',{connect:'Verified',mcpPlugin:'Pending'},
          'resumed polling must preserve Step9');
        await nextTunnel({state:'polling',pollActive:true,toolCallsVerified:4,desiredConnected:true},
          'mcpPlugin',{connect:'Verified',mcpPlugin:'Pending',ready:'Pending'},
          'a positive native counter in polling state must NOT fake inbound MCP proof');
        await nextTunnel({state:'tool_verified',pollActive:false,toolCallsVerified:4,desiredConnected:true},
          'mcpPlugin',{connect:'Unable to check',mcpPlugin:'Unable to check'},
          'inconsistent verified label with inactive poll must fail closed');
      }
      if (scenario.followTunnel==='manualOther') {
        await nativePage.locator('[data-cos-setup-step="folder"]').click();
        ensure(await nativePage.locator('[data-cos-setup-step="folder"]').getAttribute('aria-current')==='step',
          'Manual inspection of Folder must suspend auto navigation from Step9');
      }
      await nextTunnel({state:'tool_verified',pollActive:true,toolCallsVerified:1,desiredConnected:true},
        scenario.followTunnel==='automatic'?'ready':
          scenario.followTunnel==='manualStep8'?'connect':'folder',
        {connect:'Verified',mcpPlugin:'Verified',ready:'Verified'},
        'only valid native tool_verified + active polling + accepted tools/call can verify Step9 and Step10');
      if (scenario.followTunnel==='automatic') {
        await nextTunnel({state:'polling',pollActive:true,toolCallsVerified:0,desiredConnected:true},
          'ready',{connect:'Verified',mcpPlugin:'Pending',ready:'Pending'},
          'transient lost MCP proof must not repeatedly oscillate away from Step10');
      }
    }
    if (scenario.reopen) {
      await nativePage.locator('[data-cos-setup-step="extension"]').click();
      await nativePage.locator('#androidSettingsBack').click();
      const priorReads=await nativePage.evaluate(() => {
        const s=window.__setupScenario;
        s.pauseNextPermissionStatus=true;
        return s.calls.filter(call=>call.command==='android_permissions_status').length;
      });
      await nativePage.locator('[data-android-settings-category="setup"]').click();
      await nativePage.waitForFunction(previous => {
        const s=window.__setupScenario;
        return s.releasePermissionStatus &&
          s.calls.filter(call=>call.command==='android_permissions_status').length>previous;
      },priorReads);
      const instant=await nativePage.evaluate(() => ({
        checking:!document.getElementById('androidCosSetupChecking').hidden,
        selected:document.querySelector('[data-cos-setup-step][aria-current="step"]')?.dataset.cosSetupStep,
        badges:Object.fromEntries([...document.querySelectorAll('[data-cos-setup-step]')]
          .map(button=>[button.dataset.cosSetupStep,button.querySelector('small')?.textContent])),
        progress:document.getElementById('androidCosSetupProgress').getAttribute('aria-valuenow'),
        activePanel:!document.getElementById('androidCosSetupCurrent').classList.contains('hidden'),
      }));
      ensure(!instant.checking && instant.selected==='ready' &&
        instant.activePanel && instant.progress==='10' &&
        Object.values(instant.badges).every(badge=>badge==='Verified'),
        `Reopened fully verified Setup must immediately show Ready/10 Verified without the Checking overlay, while native refresh is deliberately blocked: ${JSON.stringify(instant)}`);
      if (scenario.reopen==='manual') {
        await nativePage.locator('[data-cos-setup-step="folder"]').click();
        ensure(await nativePage.locator('[data-cos-setup-step="folder"]').getAttribute('aria-current')==='step' &&
          await nativePage.locator('#androidCosSetupChecking').isHidden(),
          'Explicit folder selection during trusted background scan must be usable immediately');
      }
      await nativePage.evaluate(revoke => {
        const s=window.__setupScenario;
        if (revoke) s.permissionsRevoked=true;
        s.releasePermissionStatus();
      },scenario.reopen==='revoked' || scenario.reopen==='manual');
      await nativePage.waitForFunction(mode => {
        const current=document.querySelector('[data-cos-setup-step][aria-current="step"]')?.dataset.cosSetupStep;
        const access=document.querySelector('[data-cos-setup-step="access"] small')?.textContent;
        const ready=document.querySelector('[data-cos-setup-step="ready"] small')?.textContent;
        const checking=document.getElementById('androidCosSetupChecking').hidden;
        return checking && (mode==='instant'
          ? current==='ready' && access==='Verified' && ready==='Verified'
          : current===(mode==='manual'?'folder':'access') &&
            access==='Pending' && ready!=='Verified');
      },scenario.reopen,{timeout:18000});
      const refreshed=await nativePage.evaluate(() => ({
        selected:document.querySelector('[data-cos-setup-step][aria-current="step"]')?.dataset.cosSetupStep,
        access:document.querySelector('[data-cos-setup-step="access"] small')?.textContent,
        ready:document.querySelector('[data-cos-setup-step="ready"] small')?.textContent,
        completedHint:localStorage.getItem('mcp-agent-ai-android-setup-completed-hint-v1'),
        commands:window.__setupScenario.calls.map(call=>call.command),
      }));
      ensure(!refreshed.commands.some(command=>[
        'android_permissions_request','android_openai_tunnel_connect',
        'android_openai_tunnel_connect_stored','android_chatgpt_oauth_start'].includes(command)) &&
        (scenario.reopen==='instant'
          ? refreshed.selected==='ready' && refreshed.ready==='Verified' && refreshed.completedHint==='1'
          : refreshed.access==='Pending' && refreshed.ready!=='Verified' &&
            refreshed.completedHint===null),
        `Background native rescan must revoke stale state without prompts or synthetic reconnect: ${JSON.stringify(refreshed)}`);
    }
    for (const width of [320,360,390]) {
      await nativePage.setViewportSize({width,height:844});
      const bounds=await nativePage.evaluate(() => {
        const steps=document.getElementById('androidCosSetupSteps');
        const wizard=document.getElementById('androidCosSetupWizard').getBoundingClientRect();
        return {viewport:innerWidth,document:document.documentElement.scrollWidth,
          stepWidth:steps.scrollWidth,stepClient:steps.clientWidth,
          wizardLeft:wizard.left,wizardRight:wizard.right};
      });
      ensure(bounds.document<=bounds.viewport+1 &&
        bounds.stepWidth<=bounds.stepClient+1 &&
        bounds.wizardLeft>=-1 && bounds.wizardRight<=bounds.viewport+1,
        `Setup-on-open ${scenario.label} overflows Android ${width}px: ${JSON.stringify(bounds)}`);
      await nativePage.screenshot({path:path.join(screenshotFolder,
        `native-android-setup-open-${scenario.label}-${width}.png`),fullPage:true});
    }
    await nativePage.close();
  }
  for (const scenario of setupOpenScenarios) await runSetupOpenScenario(scenario);
  // Reopening Setup in a fresh Android app session must read a previously
  // saved Core ID from native storage before Step6/7 is ever selected.
  // A missing Runtime API key must still leave Step9 pending authorization.
  const reopenedCorePage = await browser.newPage({
    viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true,
    userAgent: 'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36',
  });
  reopenedCorePage.on('pageerror', error => errors.push(error.message));
  await reopenedCorePage.goto(app.url);
  await reopenedCorePage.evaluate(() => {
    window.isTauri = true;
    const savedId = 'tunnel_' + 'c'.repeat(32);
    window.__reopenedCore = {savedId,idReads:0,keyReads:0,statusReads:0,connectCalls:0};
    window.__TAURI_INTERNALS__ = {
      invoke: async command => {
        const native = window.__reopenedCore;
        if (command === 'android_tunnel_saved_id_status') {
          native.idReads++;
          return {tunnelId:native.savedId,verified:false};
        }
        if (command === 'android_tunnel_credentials_status') {
          native.keyReads++;
          return {stored:false,secure:true};
        }
        if (command === 'android_openai_tunnel_status') {
          native.statusReads++;
          return {state:'disconnected',pollActive:false,toolCallsVerified:0};
        }
        if (command === 'android_tunnel_check_id')
          return {valid:true,formatOnly:true};
        if (command === 'android_openai_tunnel_connect_stored') {
          native.connectCalls++;
          throw Error('A Runtime API key has not been authorized');
        }
        throw Error('Synthetic Android status unavailable: ' + command);
      },
    };
  });
  ensure(await reopenedCorePage.evaluate(() =>
    window.__reopenedCore.idReads === 0 && window.__reopenedCore.keyReads === 0),
    'Fresh Android session fixture must start before any native Setup credential reads');
  await reopenedCorePage.locator('#sidebarToggle').click();
  await reopenedCorePage.locator('#sidebarCustomize').click();
  await reopenedCorePage.locator('[data-android-settings-category="setup"]').click();
  await reopenedCorePage.waitForFunction(() => {
    const native = window.__reopenedCore;
    return native.idReads > 0 && native.keyReads > 0 &&
      document.getElementById('androidOpenAiTunnelId').value === native.savedId &&
      document.querySelector('[data-cos-setup-step="tunnel"] small')?.textContent === 'Verified';
  });
  ensure(await reopenedCorePage.locator('[data-cos-setup-step="access"]').getAttribute('aria-current') === 'step' &&
    await reopenedCorePage.locator('[data-cos-setup-step="key"] small').innerText() === 'Pending' &&
    await reopenedCorePage.locator('[data-cos-setup-step="connect"] small').innerText() === 'Pending' &&
    await reopenedCorePage.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() === 'Pending' &&
    await reopenedCorePage.locator('#androidCosSetupProgress').getAttribute('aria-valuemax') === '10' &&
    await reopenedCorePage.evaluate(() => window.__reopenedCore.connectCalls === 0),
    'Opening Setup must restore persisted Step6 Core ID without entering Step6/7, authorizing a key or connecting MCP');
  const setupCoreReads = await reopenedCorePage.evaluate(() => ({
    id:window.__reopenedCore.idReads,key:window.__reopenedCore.keyReads,
  }));
  await reopenedCorePage.locator('[data-cos-setup-step="connect"]').click();
  await reopenedCorePage.waitForFunction(({id,key}) =>
    window.__reopenedCore.idReads > id && window.__reopenedCore.keyReads > key,
  setupCoreReads);
  const reopenedStep8Status = await reopenedCorePage.locator('#androidCosChatgptConnectStatus').innerText();
  ensure(await reopenedCorePage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Verified' &&
    await reopenedCorePage.locator('[data-cos-setup-step="connect"] small').innerText() === 'Pending' &&
    await reopenedCorePage.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() === 'Pending' &&
    await reopenedCorePage.locator('#androidCosChatgptConnect').isDisabled() &&
    !reopenedStep8Status.includes('Save a Core Tunnel ID') &&
    await reopenedCorePage.evaluate(() => window.__reopenedCore.connectCalls === 0),
    `Reopened Step8 must require authorized Runtime key for saved Core ID without starting native tunnel: ${reopenedStep8Status}`);
  await reopenedCorePage.close();
  // Simulate only the permission bridge after ordinary page load; never grant
  // permissions or touch any real Android app-ops in UI tests.
  const permissionPage = await browser.newPage({
    viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true,
    userAgent: 'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36',
  });
  // Not needed is an independently resolved stage; installed browser extensions
  // are not mandatory. Progress must count both verified and explicitly
  // not-needed stages instead of assuming every other stage stays Pending.
  const setupProgressMatchesTiles = () => permissionPage.evaluate(() => {
    const states = [...document.querySelectorAll('[data-cos-setup-step] small')]
      .map(el => el.textContent?.trim());
    const complete = states.filter(value => value === 'Verified' || value === 'Not needed').length;
    return Number(document.getElementById('androidCosSetupProgress').getAttribute('aria-valuenow')) === complete;
  });
  const setupNetworkCalls = [];
  permissionPage.on('request', request => {
    if (/\/api\/(chat|chat-tool-review|mobile-goal-run|goal-tool-review)(?:\?|$)/.test(request.url()))
      setupNetworkCalls.push(request.url());
  });
  permissionPage.on('pageerror', error => errors.push(error.message));
  await permissionPage.goto(app.url);
  await permissionPage.evaluate(() => {
    window.isTauri = true;
    window.__permissionTestRequests = [];
    window.__accessibilityRequests = 0;
    window.__accessibility = {supported:true,enabled:false,connected:false,needsReconnect:false};
    window.__permissionTunnelEvents = [];
    window.__permissionTunnel = {state:'disconnected',pollActive:false,toolCallsVerified:0};
    window.__tunnelNativeStatusReads = 0;
    window.__tunnelSavedCredential = {stored:false,secure:true};
    window.__tunnelStoredKeySecret = null;
    window.__tunnelCredentialEvents = [];
    window.__tunnelOpenPlatformEvents = [];
    window.__tunnelCoreIdSaved = null;
    window.__tunnelDeferSavedIdRead = false;
    window.__tunnelSavedIdReadReturned = false;
    window.__tunnelSaveDraftCalls = [];
    window.__tunnelVerifyCalls = [];
    window.__tunnelVerifyResponse = 'ok';
    window.__mcpAccessKeys = ['browseFolders','searchFiles','readFiles','fileMetadata',
      'createFiles','editFiles','moveFiles','deleteFiles','browserControl','terminal'];
    window.__mcpAccessCalls = [];
    const emptyMcpAccessMask = Object.fromEntries(window.__mcpAccessKeys.map(k => [k,false]));
    window.__mcpAccessPolicy = {preset:'read_only',requested:{...emptyMcpAccessMask},
      effective:{...emptyMcpAccessMask},available:{...emptyMcpAccessMask}};
    window.__automationModesSaved = {version:1,androidMode:'realtime',browserMode:'headless',saved:true};
    window.__automationPreferenceCalls = [];
    window.__automationLaunchCalls = [];
    window.__automationFailNextSave = false;
    window.__automationScreenCapabilityReady = false;
    window.__permissionLocalMcpEvents = [];
    window.__permissionLocalMcp = {state:'stopped',localUrl:null,toolCallsHandled:0,remoteVerified:false};
    window.__permissionChatgptCalls = [];
    window.__permissionChatgpt = {supported:true,running:false,visible:false,sessionLinked:false,signInVerified:false,
      engine:'Android owned WebView (separate profile)',message:'Synthetic owned ChatGPT browser'};
    window.__permissionChatgptLogin = 'signed_out';
    window.__permissionChatgptStatusFailure = false;
    window.__permissionChatgptStatusForgery = false;
    window.__permissionChatgptRejectBring = false;
    window.__permissionOAuth = {supported:true,state:'signed_out',pending:false,
      signInVerified:false,sessionLinked:false,planEnabled:false,
      message:'Sign in with ChatGPT and approve Mcp Agent Ai.'};
    window.__permissionOAuthCalls = [];
    window.__permissionOAuthGrant = 'waiting';
    window.__permissionOAuthStatusFailure = false;
    window.__permissionOAuthStartFailure = false;
    window.__permissionOAuthBringRejected = false;
    window.__permissionOAuthStatusForgery = false;
    window.__permissionFolder = {granted:false,treeUri:''};
    window.__mainStorageCalls = [];
    window.__mainStorageSelection = {path:[],selected:false};
    window.__mainStorageSelectionCalls = [];
    window.__permissionGecko = [];
    window.__permissionGeckoCalls = [];
    window.__permissionGeckoProbeId = 'mcp-agent-browser-probe@example.com';
    window.__permissionTestData = {
      files: { supported: true, granted: false },
      overlay: { supported: true, granted: true },
      notifications: { supported: true, granted: false },
      battery: { supported: true, granted: false },
    };
    window.__optionalPermissionTestData = {
      camera: {supported:true,granted:false},
      microphone: {supported:true,granted:false},
      location: {supported:true,granted:false},
    };
    window.__optionalPermissionRequests = [];
    window.__optionalPermissionStatusReads = 0;
    window.__optionalPermissionStatusFail = false;
    window.__optionalPermissionRequestFail = false;
    window.__setupNativeStatusReads = [];
    window.__foregroundRuntime = {supported:true,running:false,
      notificationVisible:false,stopAvailable:false};
    window.__foregroundRuntimeActions = [];
    window.__foregroundRuntimeWakeLockHeld = false;
    window.__foregroundRuntimeServiceAlive = false;
    window.__foregroundRuntimeExitSuppressed = false;
    // Model *native* service lifecycle and notification PendingIntents, not
    // JavaScript commands. Android calls onActivityPaused even when the
    // WebView cannot execute any JavaScript in the background.
    window.__foregroundRuntimePause = () => {
      window.__foregroundRuntimeActions.push('native:pause');
      if (window.__foregroundRuntimeExitSuppressed ||
          !window.__permissionTestData.notifications.granted) return;
      window.__foregroundRuntimeServiceAlive = true;
      Object.assign(window.__foregroundRuntime,
        {running:true,notificationVisible:true,stopAvailable:true});
    };
    window.__foregroundRuntimeResume = () => {
      window.__foregroundRuntimeActions.push('native:resume');
      Object.assign(window.__foregroundRuntime,
        {running:false,notificationVisible:false,stopAvailable:false});
    };
    window.__foregroundRuntimeReopen = () => {
      window.__foregroundRuntimeActions.push('native:reopen');
      window.__foregroundRuntimeExitSuppressed = false;
      window.__foregroundRuntimeResume();
    };
    window.__foregroundRuntimeNotificationActions = () =>
      window.__foregroundRuntime.notificationVisible
        ? ['Exit',window.__foregroundRuntimeWakeLockHeld
            ? 'Release wakelock' : 'Acquire wakelock']
        : [];
    window.__foregroundRuntimeAction = action => {
      if (!window.__foregroundRuntimeNotificationActions().includes(action))
        throw Error('Notification action unavailable: ' + action);
      if (action === 'Exit') {
        window.__foregroundRuntimeActions.push('notification:exit');
        window.__foregroundRuntimeWakeLockHeld = false;
        window.__foregroundRuntimeServiceAlive = false;
        window.__foregroundRuntimeExitSuppressed = true;
        Object.assign(window.__foregroundRuntime,
          {running:false,notificationVisible:false,stopAvailable:false});
      } else if (action === 'Acquire wakelock' && window.__foregroundRuntime.running) {
        window.__foregroundRuntimeActions.push('notification:acquire');
        window.__foregroundRuntimeWakeLockHeld = true;
      } else if (action === 'Release wakelock' && window.__foregroundRuntime.running) {
        window.__foregroundRuntimeActions.push('notification:release');
        window.__foregroundRuntimeWakeLockHeld = false;
      } else throw Error('Unexpected native notification action: ' + action);
    };
    window.__TAURI_INTERNALS__ = {
      invoke: async (command, args) => {
        if (['android_permissions_status','android_optional_permissions_status',
          'accessibility_status','custom_gecko_list_extensions',
          'android_main_storage_status','android_main_storage_selection_status',
          'android_mcp_permissions_status','android_tunnel_saved_id_status',
          'android_tunnel_credentials_status','android_openai_tunnel_status',
          'android_chatgpt_oauth_status'].includes(command))
          window.__setupNativeStatusReads.push(command);
        if (command.startsWith('android_foreground_runtime_')) {
          window.__foregroundRuntimeActions.push('js:' + command);
          throw Error('Foreground session must be fully native, without renderer commands');
        }
        if (command.startsWith('android_chatgpt_oauth_')) {
          const action = command.slice('android_chatgpt_oauth_'.length);
          const oauth = window.__permissionOAuth;
          window.__permissionOAuthCalls.push(action);
          if (action === 'status') {
            if (window.__permissionOAuthStatusFailure)
              throw Error('Synthetic native OpenAI OAuth status unavailable');
            if (oauth.state === 'authorizing' && window.__permissionOAuthGrant === 'approved')
              Object.assign(oauth,{state:'verified',pending:false,signInVerified:true,
                sessionLinked:false,planEnabled:true,
                message:'OpenAI verified Mcp Agent Ai approval and ChatGPT plan permission.'});
            if (oauth.state === 'authorizing' && window.__permissionOAuthGrant === 'identity_only')
              Object.assign(oauth,{state:'verified',pending:false,signInVerified:true,
                sessionLinked:false,planEnabled:false,
                message:'The account was approved without ChatGPT plan permission.'});
            if (window.__permissionOAuthGrant === 'revoked')
              Object.assign(oauth,{state:'signed_out',pending:false,signInVerified:false,
                sessionLinked:false,planEnabled:false,message:'Native grant revoked.'});
            return {...structuredClone(oauth),
              ...(window.__permissionOAuthStatusForgery ? {accessToken:'MUST_NOT_ESCAPE_ANDROID'} : {})};
          }
          if (action === 'start') {
            if (args?.userInitiated !== true) throw Error('Sign in requires user consent');
            if (window.__permissionOAuthStartFailure)
              throw Error('Native system browser could not open OAuth approval');
            window.__permissionOAuthGrant = 'waiting';
            Object.assign(oauth,{state:'authorizing',pending:true,signInVerified:false,
              sessionLinked:false,planEnabled:false,
              message:'Waiting for Google or ChatGPT login and Mcp Agent Ai approval in Chrome.'});
            return structuredClone(oauth);
          }
          if (action === 'bring') {
            if (args?.userInitiated !== true || oauth.state !== 'verified' ||
                !oauth.signInVerified || !oauth.planEnabled || window.__permissionOAuthBringRejected)
              throw Error('Native plan authorization not verified');
            Object.assign(oauth,{state:'linked',pending:false,signInVerified:true,
              sessionLinked:true,planEnabled:true,
              message:'Mcp Agent Ai approved; ChatGPT plan connection verified.'});
            return structuredClone(oauth);
          }
          throw Error('Unexpected native OAuth command '+command);
        }
        if (command === 'custom_gecko_list_extensions') {
          window.__permissionGeckoCalls.push('list');
          if (window.__permissionGeckoInvalid)
            return {runtimeStarted:false,extensions:[]};
          return {runtimeStarted:true,extensions:structuredClone(window.__permissionGecko)};
        }
        if (command === 'custom_gecko_install_probe') {
          if (args?.userInitiated !== true) throw Error('Explicit extension install required');
          window.__permissionGeckoCalls.push('install');
          if (window.__permissionGeckoInstallDenied) return {installed:false};
          if (!window.__permissionGecko.some(ext => ext.id === window.__permissionGeckoProbeId))
            window.__permissionGecko.push({id:window.__permissionGeckoProbeId,
              name:'Mcp Agent Ai extension probe',version:'1.0',enabled:true,builtIn:true});
          return {installed:true,id:window.__permissionGeckoProbeId};
        }
        if (command === 'custom_gecko_manage') {
          if (!['enable','disable','remove'].includes(args?.action) ||
              args.userInitiated !== true) throw Error('Invalid managed extension action');
          window.__permissionGeckoCalls.push(args.action);
          const index = window.__permissionGecko.findIndex(ext => ext.id === args.id);
          if (index < 0) throw Error('Extension is not installed');
          if (args.action === 'remove') window.__permissionGecko.splice(index,1);
          else window.__permissionGecko[index].enabled = args.action === 'enable';
          return {updated:true,id:args.id};
        }
        if (command.startsWith('android_chatgpt_')) {
          const action = command.slice('android_chatgpt_'.length);
          window.__permissionChatgptCalls.push(action);
          const browser = window.__permissionChatgpt;
          if (action === 'status') {
            if (window.__permissionChatgptStatusFailure)
              throw Error('Synthetic owned ChatGPT browser status unavailable');
            // Setup inspects the stored private profile with a hidden native
            // browser, so an existing login needs no Sign in button tap.
            if (!browser.running) { browser.running = true; browser.visible = false; }
            // Simulated native sign-in monitor: the foreground login window is
            // hidden only when its private, read-only session check confirms login.
            if (browser.running &&
                window.__permissionChatgptLogin === 'signed_in' &&
                window.__permissionChatgptAutoCheck !== false) {
              browser.visible = false;
              browser.signInVerified = true;
            }
            if (browser.signInVerified && window.__permissionChatgptLogin !== 'signed_in') {
              browser.signInVerified = false;
              browser.sessionLinked = false;
            }
            return {...structuredClone(browser),
              ...(window.__permissionChatgptStatusForgery ? {cookies:'NEVER_ALLOWED'} : {})};
          }
          if (action === 'open') {
            if (args?.userInitiated !== true) throw Error('Explicit browser open required');
            if (window.__permissionChatgptOpenFailure)
              throw Error('Synthetic native ChatGPT browser refused to open');
            browser.running = true; browser.visible = true;
            browser.sessionLinked = false; browser.signInVerified = false;
          }
          if (action === 'show') {
            if (!browser.running) throw Error('Browser is not running');
            browser.visible = true;
          }
          if (action === 'hide') {
            if (!browser.running) throw Error('Browser is not running');
            browser.visible = false;
          }
          if (action === 'close') {
            browser.running = false; browser.visible = false;
            browser.sessionLinked = false; browser.signInVerified = false;
          }
          if (action === 'check_session' || action === 'bring_session') {
            if (!browser.running || (action === 'bring_session' &&
                (args?.userInitiated !== true || browser.visible || !browser.signInVerified ||
                  window.__permissionChatgptRejectBring)))
              throw Error('No user-consented owned ChatGPT browser');
            const state = window.__permissionChatgptLogin;
            if (action === 'check_session' && state === 'signed_in') {
              browser.signInVerified = true;
              browser.visible = false;
            }
            if (state !== 'signed_in') {
              browser.sessionLinked = false;
              browser.signInVerified = false;
            }
            if (action === 'bring_session' && state === 'signed_in') {
              browser.sessionLinked = true; browser.visible = false;
            }
            return {owned:true,origin:'chatgpt.com',pageGeneration:1,readOnly:true,
              signInState:state,sessionLinked:browser.sessionLinked,
              ...(window.__permissionChatgptForgery ? {cookies:'NEVER_ALLOWED'} : {})};
          }
          if (!['status','open','show','hide','close'].includes(action)) throw Error('Unexpected ChatGPT action');
          return structuredClone(browser);
        }
        if (command === 'android_permissions_status') {
          if (window.__permissionTestFail) throw Error('synthetic bridge unavailable');
          return structuredClone(window.__permissionTestData);
        }
        if (command === 'android_optional_permissions_status') {
          window.__optionalPermissionStatusReads++;
          if (window.__optionalPermissionStatusFail)
            throw Error('Synthetic Android optional permissions status unavailable');
          return structuredClone(window.__optionalPermissionTestData);
        }
        if (command === 'android_optional_permissions_request') {
          if (args?.userInitiated !== true ||
              !['camera','microphone','location'].includes(args?.permission))
            throw Error('Optional permission request requires explicit, allowlisted user gesture');
          window.__optionalPermissionRequests.push({permission:args.permission,
            userInitiated:args.userInitiated});
          if (window.__optionalPermissionRequestFail)
            throw Error('Synthetic Android optional permission dialog denied');
          // Showing the OS permission dialog is not a grant receipt. The
          // next status read remains denied until simulated Android changes.
          return {opened:true};
        }
        if (command === 'android_permissions_request') {
          window.__permissionTestRequests.push(args.permission);
          if (window.__permissionRequestDenied) throw Error('Synthetic Android permission request denied');
          return { opened: true };
        }
        if (command === 'document_tree_status')
          return structuredClone(window.__permissionFolder);
        if (command === 'document_tree_request') {
          window.__permissionFolder = {granted:true,
            treeUri:'content://com.android.externalstorage.documents/tree/primary%3ASynthetic'};
          return {selected:true};
        }
        if (command === 'android_main_storage_status') {
          const granted = window.__permissionTestData.files.granted === true;
          return {supported:true,granted,readable:granted,rootPath:'/storage/emulated/0'};
        }
        if (command === 'android_main_storage_selection_status') {
          if (!window.__permissionTestData.files.granted) {
            window.__mainStorageSelection = {path:[],selected:false};
            throw Error('All files access revoked');
          }
          return structuredClone(window.__mainStorageSelection);
        }
        if (command === 'android_main_storage_select' || command === 'android_main_storage_reset') {
          if (!window.__permissionTestData.files.granted)
            throw Error('All files access revoked');
          const reset = command === 'android_main_storage_reset';
          const path = reset ? [] : args?.path;
          if (!Array.isArray(path) || path.some(name =>
            !name || name === '..' || name.includes('/') || name.includes('\\')) ||
            (!reset && path.join('/') !== 'Download' && path.join('/') !== 'DCIM'))
            throw Error('Selected folder is unavailable');
          window.__mainStorageSelectionCalls.push({action:reset?'reset':'select',path:structuredClone(path)});
          window.__mainStorageSelection = {path:structuredClone(path),selected:path.length>0};
          return structuredClone(window.__mainStorageSelection);
        }
        if (command === 'android_main_storage_list') {
          window.__mainStorageCalls.push(structuredClone(args?.path));
          if (window.__permissionTestData.files.granted !== true ||
              !Array.isArray(args?.path) || args.path.some(name =>
                !name || name === '..' || name.includes('/') || name.includes('\\')))
            throw Error('Invalid or unauthorized shared storage listing');
          const children = args.path.length === 0
            ? [{name:'DCIM',isDirectory:true,readable:true},
               {name:'Download',isDirectory:true,readable:true},
               {name:'Android',isDirectory:true,readable:false}]
            : args.path.join('/') === 'Download'
              ? [{name:'Receipts',isDirectory:true,readable:true}]
              : [];
          return {path:structuredClone(args.path),entries:children};
        }
        if (command === 'accessibility_status')
          return structuredClone(window.__accessibility);
        if (command === 'accessibility_request_settings') {
          if (args?.userInitiated !== true) throw Error('Explicit OS settings gesture required');
          window.__accessibilityRequests++;
          return {opened:true,destination:'accessibility_settings',enabled:window.__accessibility.enabled};
        }
        if (command === 'android_openai_tunnel_status') {
          window.__tunnelNativeStatusReads++;
          return structuredClone(window.__permissionTunnel);
        }
        if (command === 'android_tunnel_credentials_status')
          return structuredClone(window.__tunnelSavedCredential);
        if (command === 'android_tunnel_saved_id_status') {
          const captured = window.__tunnelCoreIdSaved;
          if (window.__tunnelDeferSavedIdRead) {
            window.__tunnelDeferSavedIdRead = false;
            await new Promise(resolve => { window.__tunnelReleaseSavedIdRead = resolve; });
            window.__tunnelSavedIdReadReturned = true;
          }
          return {tunnelId:captured,verified:false};
        }
        if (command === 'android_tunnel_save_id') {
          if (args?.userInitiated !== true || typeof args?.tunnelId !== 'string' ||
              !/^tunnel_(?:[a-z0-9]{4}_)?[a-z0-9]{32}$/.test(args.tunnelId))
            throw Error('Explicit valid Core ID Save required');
          if (window.__tunnelSaveRejectNext) {
            window.__tunnelSaveRejectNext = false;
            throw Error('Synthetic private draft write rejected');
          }
          window.__tunnelSaveDraftCalls.push(args.tunnelId);
          window.__tunnelCoreIdSaved = args.tunnelId;
          // Saving an identifier is NOT authentication or a tunnel connect.
          window.__permissionTunnel = {state:'disconnected',pollActive:false,toolCallsVerified:0};
          return {tunnelId:args.tunnelId,saved:true,verified:false};
        }
        if (command === 'android_tunnel_verify_and_save') {
          if (args?.userInitiated !== true || typeof args?.tunnelId !== 'string')
            throw Error('Explicit Core ID Save required');
          window.__tunnelVerifyCalls.push(args.tunnelId);
          if (!/^tunnel_(?:[a-z0-9]{4}_)?[a-z0-9]{32}$/.test(args.tunnelId))
            return {verified:false,saved:false,reason:'invalid_id'};
          if (!window.__tunnelSavedCredential.stored)
            return {verified:false,saved:false,reason:'key_required'};
          const reason = window.__tunnelVerifyResponse;
          if (reason !== 'ok') return {verified:false,saved:false,reason};
          window.__tunnelCoreIdSaved = args.tunnelId;
          return {verified:true,saved:true,reason:'ok'};
        }
        if (command === 'android_tunnel_check_id')
          return {valid:typeof args?.tunnelId === 'string' &&
            /^tunnel_(?:[a-z0-9]{4}_)?[a-z0-9]{32}$/.test(args.tunnelId),
            formatOnly:true};
        if (command === 'android_tunnel_open_platform_page') {
          if (args?.userInitiated !== true || !['tunnels','api_keys','chatgpt_plugins'].includes(args?.kind))
            throw Error('Explicit and allowlisted native browser destination only');
          window.__tunnelOpenPlatformEvents.push(args.kind);
          return {opened:true,kind:args.kind};
        }
        if (command === 'android_tunnel_credentials_store') {
          if (args?.userInitiated !== true || typeof args?.apiKey !== 'string' ||
              args.apiKey.length < 8 || args.apiKey.length > 512)
            throw Error('Explicit validated Android Keystore save required');
          window.__tunnelStoredKeySecret = args.apiKey;
          window.__tunnelSavedCredential = {stored:true,secure:true};
          window.__tunnelCredentialEvents.push('store');
          return structuredClone(window.__tunnelSavedCredential);
        }
        if (command === 'android_tunnel_credentials_remove') {
          if (args?.userInitiated !== true) throw Error('Explicit Keystore delete required');
          window.__tunnelStoredKeySecret = null;
          window.__tunnelSavedCredential = {stored:false,secure:true};
          window.__permissionTunnel = {state:'disconnected',pollActive:false,toolCallsVerified:0};
          window.__tunnelCredentialEvents.push('remove');
          return structuredClone(window.__tunnelSavedCredential);
        }
        if (command === 'android_openai_tunnel_connect_stored') {
          if (args?.userInitiated !== true || !window.__tunnelStoredKeySecret ||
              !/^tunnel_(?:[a-z0-9]{4}_)?[a-z0-9]{32}$/.test(args.tunnelId))
            throw Error('Keystore key and approved ID required for explicit connect');
          window.__permissionTunnelEvents.push('connect_stored');
          window.__permissionTunnel = {state:'polling',pollActive:true,toolCallsVerified:0};
          return {started:true};
        }
        if (command === 'android_mcp_permissions_status')
          return structuredClone(window.__mcpAccessPolicy);
        if (command === 'android_automation_preferences_status')
          return structuredClone(window.__automationModesSaved);
        if (command === 'android_automation_preferences_set') {
          if (args?.userInitiated !== true ||
              !['realtime','screenshot_click'].includes(args.androidMode) ||
              !['headless','right_sidebar','open_browser_app'].includes(args.browserMode))
            throw Error('Explicit validated native automation preference save required');
          if (window.__automationFailNextSave) {
            window.__automationFailNextSave = false;
            throw Error('Synthetic native automation preference write rejected');
          }
          window.__automationPreferenceCalls.push(structuredClone(args));
          window.__automationModesSaved = {version:1,androidMode:args.androidMode,
            browserMode:args.browserMode,saved:true};
          return structuredClone(window.__automationModesSaved);
        }
        if (command === 'android_background_browser_status')
          return {supported:true,running:false,ready:false,offscreen:false,profileIsolated:false,url:'',title:''};
        if (command === 'in_app_browser_status')
          return {opened:false,activeTabId:0,profileIsolationSupported:true,isolatedProfile:false,tabs:[]};
        if (command === 'android_external_chrome_status')
          return {supported:true,available:true,canLaunch:true,mode:'external_chrome',launchOnly:true,domControl:false};
        if (command === 'accessibility_automation_mode_status') {
          const ready = window.__automationScreenCapabilityReady;
          return {selectedMode:args.selectedMode,effectiveMode:'unavailable',fallbackUsed:false,
            reason:ready ? 'target_session_required' : 'semantic_action_and_screenshot_tap_unavailable',
            supported:true,enabled:true,connected:true,needsReconnect:false,
            semanticActionAvailable:ready,screenCaptureAvailable:ready,tapAvailable:ready,
            realtimeAvailable:ready,screenshotClickAvailable:ready};
        }
        if (['in_app_browser_open','android_background_browser_start',
          'android_background_browser_stop','android_external_chrome_open',
          'in_app_browser_close_all'].includes(command)) {
          window.__automationLaunchCalls.push(command);
          throw Error('Automation mode selection must never launch a browser');
        }
        if (command === 'android_mcp_permissions_configure') {
          if (window.__mcpAccessRejectNext) {
            window.__mcpAccessRejectNext = false;
            throw Error('Synthetic native permission save rejected');
          }
          const keys = window.__mcpAccessKeys;
          if (args?.userInitiated !== true ||
              !['read_only','files_terminal','full','custom'].includes(args.preset) ||
              !args.requested || typeof args.requested !== 'object' ||
              Object.keys(args.requested).sort().join(',') !== [...keys].sort().join(',') ||
              keys.some(key => typeof args.requested[key] !== 'boolean'))
            throw Error('Invalid explicit MCP tool permission request');
          const read = keys.slice(0,4), write = keys.slice(4,8);
          const canonical = Object.fromEntries(keys.map(key => [key,
            args.preset === 'full' || (args.preset === 'files_terminal' && key !== 'browserControl') ||
            (args.preset === 'read_only' && read.includes(key))]));
          if (args.preset !== 'custom' &&
              keys.some(key => canonical[key] !== args.requested[key]))
            throw Error('MCP preset and requested access mismatch');
          window.__mcpAccessCalls.push({preset:args.preset,
            requested:structuredClone(args.requested)});
          window.__mcpAccessPolicy.preset = args.preset;
          window.__mcpAccessPolicy.requested = structuredClone(args.requested);
          // No backend tool route exists yet. Even requested Full access stays
          // effective=false for all privileged remote capabilities.
          return structuredClone(window.__mcpAccessPolicy);
        }
        if (command === 'android_openai_tunnel_connect') {
          if (args?.userInitiated !== true || !/^tunnel_(?:[a-z0-9]{4}_)?[a-z0-9]{32}$/.test(args.tunnelId) ||
              typeof args.apiKey !== 'string' || args.apiKey.length < 8)
            throw Error('Explicit, validated tunnel start is required');
          window.__permissionTunnelEvents.push('connect');
          window.__permissionTunnel = {state:'polling',pollActive:true,toolCallsVerified:0};
          return {started:true};
        }
        if (command === 'android_openai_tunnel_disconnect') {
          if (args?.userInitiated !== true) throw Error('Explicit tunnel stop is required');
          window.__permissionTunnelEvents.push('disconnect');
          window.__permissionTunnel = {state:'disconnected',pollActive:false,toolCallsVerified:0};
          return {stopped:true};
        }
        if (command === 'android_local_mcp_status') return structuredClone(window.__permissionLocalMcp);
        if (command === 'android_local_mcp_start') {
          if (args?.userInitiated !== true || window.__permissionLocalMcp.state !== 'stopped')
            throw Error('Explicit local MCP start with stopped state required');
          window.__permissionLocalMcpEvents.push('start');
          window.__permissionLocalMcp = {state:'listening',
            localUrl:'http://127.0.0.1:49812/mcp/' + 'a'.repeat(64),
            toolCallsHandled:0,remoteVerified:false};
          return structuredClone(window.__permissionLocalMcp);
        }
        if (command === 'android_local_mcp_stop') {
          if (args?.userInitiated !== true || window.__permissionLocalMcp.state !== 'listening')
            throw Error('Explicit local MCP stop with listening state required');
          window.__permissionLocalMcpEvents.push('stop');
          window.__permissionLocalMcp = {state:'stopped',localUrl:null,
            toolCallsHandled:0,remoteVerified:false};
          return structuredClone(window.__permissionLocalMcp);
        }
        throw Error('Unexpected native call: ' + command);
      },
    };
  });
  await permissionPage.locator('#sidebarToggle').click();
  await permissionPage.locator('#sidebarCustomize').click();
  await permissionPage.locator('[data-android-settings-category="general"]').click();
  ensure(await permissionPage.locator(
    '#androidForegroundRuntimeSection,#androidForegroundRuntimeStatus,#androidForegroundRuntimeFeedback,#androidForegroundRuntimeStart,#androidForegroundRuntimeStop'
  ).count() === 0 &&
    !/background session/i.test(await permissionPage.locator('#androidSettingsContent > [data-setting-category="general"]').allTextContents().then(items => items.join(' '))) &&
    await permissionPage.evaluate(() => window.__foregroundRuntimeActions.length === 0),
    'General must not contain Background Session status or controls or query native foreground service');
  // Android lifecycle and notification buttons are native, so simulate their
  // transitions without issuing JS Tauri commands or touching a real phone.
  await permissionPage.evaluate(() => window.__foregroundRuntimePause());
  ensure(await permissionPage.evaluate(() => !window.__foregroundRuntime.running &&
    window.__foregroundRuntimeNotificationActions().length === 0),
    'Without notification permission Android must not start a hidden background session');
  await permissionPage.evaluate(() => {
    window.__permissionTestData.notifications.granted = true;
    window.__foregroundRuntimePause();
  });
  ensure(await permissionPage.evaluate(() =>
    window.__foregroundRuntime.running && window.__foregroundRuntime.notificationVisible &&
    !window.__foregroundRuntimeWakeLockHeld &&
    JSON.stringify(window.__foregroundRuntimeNotificationActions()) ===
      JSON.stringify(['Exit','Acquire wakelock']) &&
    !window.__foregroundRuntimeActions.some(action => action.startsWith('js:'))) &&
    JSON.stringify(await permissionPage.evaluate(() => window.__permissionTunnelEvents)) === '[]',
    'Native pause shows exactly Exit/Acquire wakelock without JS calls, automatic wake lock or MCP verification');
  await permissionPage.evaluate(() => window.__foregroundRuntimeResume());
  ensure(await permissionPage.evaluate(() =>
    window.__foregroundRuntimeServiceAlive && !window.__foregroundRuntime.running &&
    !window.__foregroundRuntime.notificationVisible &&
    window.__foregroundRuntimeNotificationActions().length === 0),
    'Native onActivityResumed must remove foreground notification while preserving the started session');
  await permissionPage.evaluate(() => window.__foregroundRuntimePause());
  ensure(await permissionPage.evaluate(() =>
    window.__foregroundRuntime.notificationVisible &&
    JSON.stringify(window.__foregroundRuntimeNotificationActions()) ===
      JSON.stringify(['Exit','Acquire wakelock'])),
    'Leaving the Activity again must restore notification with exactly two actions');
  await permissionPage.evaluate(() => {
    window.__foregroundRuntimeAction('Acquire wakelock');
  });
  ensure(await permissionPage.evaluate(() => window.__foregroundRuntimeWakeLockHeld &&
    window.__foregroundRuntimeActions.includes('notification:acquire') &&
    JSON.stringify(window.__foregroundRuntimeNotificationActions()) ===
      JSON.stringify(['Exit','Release wakelock'])),
    'Notification Acquire wakelock must explicitly transition a running session to held');
  await permissionPage.evaluate(() => window.__foregroundRuntimeResume());
  ensure(await permissionPage.evaluate(() =>
    window.__foregroundRuntimeServiceAlive && !window.__foregroundRuntime.running &&
    !window.__foregroundRuntime.notificationVisible &&
    window.__foregroundRuntimeWakeLockHeld &&
    window.__foregroundRuntimeNotificationActions().length === 0),
    'Returning to the app must hide both notification actions without silently dropping the wake lock');
  await permissionPage.evaluate(() => window.__foregroundRuntimePause());
  ensure(await permissionPage.evaluate(() =>
    window.__foregroundRuntime.notificationVisible &&
    JSON.stringify(window.__foregroundRuntimeNotificationActions()) ===
      JSON.stringify(['Exit','Release wakelock'])),
    'Backgrounding after resume must restore Release wakelock when a wake lock is held');
  await permissionPage.evaluate(() => {
    window.__foregroundRuntimeAction('Release wakelock');
  });
  ensure(await permissionPage.evaluate(() =>
    !window.__foregroundRuntimeWakeLockHeld &&
    JSON.stringify(window.__foregroundRuntimeNotificationActions()) ===
      JSON.stringify(['Exit','Acquire wakelock'])),
    'Notification Release wakelock must clear held state without exiting');
  await permissionPage.evaluate(() => {
    window.__foregroundRuntimeAction('Acquire wakelock');
    window.__foregroundRuntimeAction('Exit');
  });
  ensure(await permissionPage.evaluate(() =>
    !window.__foregroundRuntime.running && !window.__foregroundRuntimeWakeLockHeld &&
    !window.__foregroundRuntimeServiceAlive &&
    window.__foregroundRuntimeExitSuppressed &&
    !window.__foregroundRuntime.notificationVisible &&
    window.__foregroundRuntimeNotificationActions().length === 0),
    'Notification Exit must release the wake lock and shut down the foreground session');
  await permissionPage.evaluate(() => window.__foregroundRuntimePause());
  ensure(!await permissionPage.evaluate(() => window.__foregroundRuntime.running),
    'Exit must suppress automatic paused lifecycle restart until the user reopens the app');
  await permissionPage.evaluate(() => {
    window.__foregroundRuntimeReopen();
    window.__foregroundRuntimePause();
  });
  ensure(await permissionPage.evaluate(() =>
    window.__foregroundRuntime.running && window.__foregroundRuntime.notificationVisible &&
    !window.__foregroundRuntimeWakeLockHeld &&
    JSON.stringify(window.__foregroundRuntimeNotificationActions()) ===
      JSON.stringify(['Exit','Acquire wakelock'])),
    'A fresh Activity reopen permits its next background transition without acquiring a wake lock');
  await permissionPage.evaluate(() => {
    window.__foregroundRuntimeAction('Exit');
    window.__permissionTestData.notifications.granted = false;
  });
  ensure(await permissionPage.evaluate(() =>
    !window.__foregroundRuntimeActions.some(action => action.startsWith('js:'))),
    'No renderer status, Start or Stop calls may occur during any native notification lifecycle');
  await permissionPage.locator('#androidSettingsBack').click();
  await permissionPage.locator('[data-android-settings-category="setup"]').click();
  ensure(await permissionPage.locator('[id^="androidForegroundRuntime"]').count() === 0 &&
    !await permissionPage.evaluate(() => window.__foregroundRuntime.running),
    'Setup Permissions must remain separate from foreground notification lifecycle');
  await permissionPage.waitForFunction(() =>
    document.getElementById('storageAccessState').textContent === 'Not allowed' &&
    document.getElementById('overlayAccessState').textContent === 'Allowed' &&
    document.getElementById('accessibilityPermissionState').textContent === 'Not enabled');
  ensure(await permissionPage.locator('[data-permission-row]').count() === 5 &&
    await permissionPage.locator('#accessibilityManage').isEnabled() &&
    await permissionPage.locator('#accessibilityFeedback,#storageAccessFeedback,#androidCosAllowNext,#refreshAndroidPermissions').count() === 0 &&
    await permissionPage.locator('#screenCaptureOne').count() === 0 &&
    await permissionPage.locator('#screenCapturePreview').count() === 0,
    'Android Setup preserves Accessibility Allow but removes lower helper text and controls');
  await permissionPage.waitForFunction(() =>
    ['androidOptionalCameraState','androidOptionalMicrophoneState','androidOptionalLocationState']
      .every(id => document.getElementById(id)?.textContent === 'Not allowed'));
  ensure(await permissionPage.locator('#androidOptionalPermissionsSection summary').isVisible() &&
    !await permissionPage.locator('#androidOptionalPermissionsSection').evaluate(node => node.open) &&
    await permissionPage.locator('[data-optional-permission-request="camera"]').isHidden() &&
    await permissionPage.locator('[data-permission-row]').count() === 5 &&
    await permissionPage.locator('[data-optional-permission]').count() === 3 &&
    await permissionPage.locator('[data-optional-permission-request]').count() === 3 &&
    await permissionPage.locator('[data-optional-permission-request]').evaluateAll(buttons =>
      buttons.every(button => !button.disabled && button.textContent.trim() === 'Allow')) &&
    (await permissionPage.locator('#androidOptionalPermissionsSection').innerText()).includes('Optional') &&
    await permissionPage.evaluate(() => window.__optionalPermissionRequests.length === 0) &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending',
    'Step1 must have five visible required rows and three folded optional native permissions with no auto requests');
  await permissionPage.locator('#androidOptionalPermissionsSection summary').click();
  ensure(await permissionPage.locator('#androidOptionalPermissionsSection').evaluate(node => node.open) &&
    await permissionPage.locator('[data-optional-permission-request="camera"]').isVisible(),
    'Step1 Optional disclosure must expand on direct user interaction');
  const optionalSetupProgressBefore = await permissionPage.locator('#androidCosSetupProgress').getAttribute('aria-valuenow');
  const optionalReadsBeforeRequest = await permissionPage.evaluate(() => window.__optionalPermissionStatusReads);
  await permissionPage.locator('[data-optional-permission-request="camera"]').click();
  await permissionPage.waitForFunction(reads =>
    window.__optionalPermissionStatusReads > reads &&
    window.__optionalPermissionRequests.length === 1, optionalReadsBeforeRequest);
  ensure(await permissionPage.locator('#androidOptionalCameraState').innerText() === 'Not allowed' &&
    await permissionPage.locator('[data-optional-permission-request="camera"]').isEnabled() &&
    JSON.stringify(await permissionPage.evaluate(() => window.__optionalPermissionRequests)) ===
      JSON.stringify([{permission:'camera',userInitiated:true}]) &&
    await permissionPage.locator('#androidCosSetupProgress').getAttribute('aria-valuenow') === optionalSetupProgressBefore &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending',
    'An optional Android permission dialog cannot claim Allowed or change required setup progress before OS grants');
  await permissionPage.evaluate(() => { window.__optionalPermissionRequestFail = true; });
  await permissionPage.locator('[data-optional-permission-request="microphone"]').click();
  await permissionPage.waitForFunction(() => window.__optionalPermissionRequests.length === 2);
  ensure(await permissionPage.locator('#androidOptionalMicrophoneState').innerText() !== 'Allowed' &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending' &&
    await permissionPage.locator('#androidCosSetupProgress').getAttribute('aria-valuenow') === optionalSetupProgressBefore,
    'Rejected optional system dialog must not create a grant or block required permissions');
  await permissionPage.evaluate(() => {
    window.__optionalPermissionRequestFail = false;
    window.__optionalPermissionTestData.camera.granted = true;
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.waitForFunction(() => document.getElementById('androidOptionalCameraState')?.textContent === 'Allowed');
  ensure(await permissionPage.locator('[data-optional-permission-request="camera"]').isDisabled() &&
    await permissionPage.locator('[data-optional-permission-request="camera"]').innerText() === 'Allowed' &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending',
    'Only a fresh native OS status receipt can mark optional Camera Allowed and disable its button');
  await permissionPage.evaluate(() => {
    window.__optionalPermissionTestData.camera.granted = false;
    window.__optionalPermissionTestData.location.supported = false;
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidOptionalCameraState')?.textContent === 'Not allowed' &&
    document.getElementById('androidOptionalLocationState')?.textContent === 'Not supported');
  ensure(await permissionPage.locator('[data-optional-permission-request="camera"]').isEnabled() &&
    await permissionPage.locator('[data-optional-permission-request="location"]').isDisabled() &&
    await permissionPage.locator('[data-optional-permission-request="location"]').innerText() === 'Not supported' &&
    await permissionPage.locator('#androidCosSetupProgress').getAttribute('aria-valuenow') === optionalSetupProgressBefore,
    'Native revocation must restore Camera Allow; unsupported coarse location must stay optional and disabled');
  await permissionPage.evaluate(() => {
    window.__optionalPermissionStatusFail = true;
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.waitForFunction(() =>
    [...document.querySelectorAll('[data-optional-permission-request]')]
      .every(button => button.disabled));
  ensure(await permissionPage.locator('#androidOptionalCameraState').innerText() !== 'Allowed' &&
    await permissionPage.locator('#androidCosSetupProgress').getAttribute('aria-valuenow') === optionalSetupProgressBefore &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending',
    'Optional native status failure must fail closed without affecting the five-required-permission gate');
  await permissionPage.evaluate(() => {
    window.__optionalPermissionStatusFail = false;
    window.__optionalPermissionTestData.location.supported = true;
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.waitForFunction(() =>
    ['androidOptionalCameraState','androidOptionalMicrophoneState','androidOptionalLocationState']
      .every(id => document.getElementById(id)?.textContent === 'Not allowed'));
  for (const width of [320,360,390]) {
    await permissionPage.setViewportSize({width,height:844});
    const optionalBounds = await permissionPage.evaluate(() => {
      const optional = document.getElementById('androidOptionalPermissionsSection');
      const rect = optional.getBoundingClientRect();
      const rows = [...optional.querySelectorAll('[data-optional-permission]')];
      return {
        pageWidth:document.documentElement.scrollWidth,
        viewport:window.innerWidth,
        panelRight:rect.right,
        panelWidth:optional.clientWidth,
        scrollWidth:optional.scrollWidth,
        buttonOutOfBounds:rows.some(row => {
          const button = row.querySelector('button').getBoundingClientRect();
          const status = row.querySelector('[role="status"]').getBoundingClientRect();
          return button.right > rect.right + 1 || status.right > rect.right + 1 ||
            button.left < rect.left - 1 || status.left < rect.left - 1;
        })
      };
    });
    ensure(optionalBounds.pageWidth <= optionalBounds.viewport + 1 &&
      optionalBounds.panelRight <= optionalBounds.viewport + 1 &&
      optionalBounds.scrollWidth <= optionalBounds.panelWidth + 2 &&
      !optionalBounds.buttonOutOfBounds,
      `Optional Step1 permission cards must fit ${width}px Android screen without horizontal scrolling: ${JSON.stringify(optionalBounds)}`);
    await permissionPage.screenshot({
      path:path.join(screenshotFolder,`native-android-permissions-optional-${width}.png`),
      fullPage:true
    });
  }
  await permissionPage.setViewportSize({width:390,height:844});
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="folder"] small').textContent === 'Pending');
  const permissionSetupInitial = await permissionPage.evaluate(() => ({
    progress:document.getElementById('androidCosSetupProgress').getAttribute('aria-valuenow'),
    stages:[...document.querySelectorAll('[data-cos-setup-step]')].map(el =>
      [el.dataset.cosSetupStep,el.querySelector('small')?.textContent]),
    currentHidden:document.getElementById('androidCosSetupCurrent').classList.contains('hidden'),
    permissionsVisible:!document.getElementById('storageAccessSection').classList.contains('hidden'),
    extDisabled:document.getElementById('androidCosExtensionInstall').disabled,
    requests:window.__permissionTestRequests,
    tunnels:window.__permissionTunnelEvents
  }));
  ensure(JSON.stringify(await permissionPage.evaluate(() => window.__permissionTestRequests)) === '[]' &&
    JSON.stringify(await permissionPage.evaluate(() => window.__permissionTunnelEvents)) === '[]' &&
    await setupProgressMatchesTiles() &&
    await permissionPage.locator('[data-cos-setup-step="folder"] small').innerText() === 'Pending' &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending' &&
    await permissionPage.locator('#androidCosSetupCurrent').isHidden() &&
    await permissionPage.locator('#storageAccessSection').isVisible() &&
    await permissionPage.locator('#androidCosExtensionInstall').isDisabled(),
    'Opening Android Setup checks native grants but stays on Permissions until five statuses are confirmed: ' +
      JSON.stringify(permissionSetupInitial));
  ensure(await permissionPage.locator('[data-permission-request="files"]').innerText() === 'Allow',
    'Each permission has its own single Allow button; there is no Allow-next shortcut');
  await permissionPage.locator('[data-permission-request="files"]').click();
  ensure(JSON.stringify(await permissionPage.evaluate(() => window.__permissionTestRequests)) === '["files"]' &&
    await permissionPage.locator('#storageAccessState').innerText() === 'Not allowed' &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending',
    'Opening native Allow dialog without a grant must leave Setup pending');
  ensure(await permissionPage.locator('#notificationAccessState').innerText() === 'Not allowed' &&
    await permissionPage.locator('#batteryAccessState').innerText() === 'Optimized',
    'Native bridge snapshot rendered incorrect notification/battery statuses');
  ensure(await permissionPage.locator('[data-permission-request="overlay"]').isDisabled() &&
    await permissionPage.locator('[data-permission-request="overlay"]').innerText() === 'Allowed' &&
    await permissionPage.locator('[data-permission-request="files"]').innerText() === 'Allow' &&
    await permissionPage.locator('[data-permission-request="notifications"]').innerText() === 'Allow',
    'Previously allowed overlay must be grey and locked while ungranted permissions have one Allow button');
  await permissionPage.evaluate(() => document.querySelector('[data-permission-request="overlay"]').click());
  ensure(JSON.stringify(await permissionPage.evaluate(() => window.__permissionTestRequests)) === '["files"]',
    'Programmatic click on disabled Allowed overlay must never launch another permission request');
  await permissionPage.evaluate(() => { window.__permissionTestData.overlay.granted = false; });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() => document.getElementById('overlayAccessState').textContent === 'Not allowed');
  ensure(await permissionPage.locator('[data-permission-request="overlay"]').isEnabled() &&
    await permissionPage.locator('[data-permission-request="overlay"]').innerText() === 'Allow',
    'Revoking overlay permission must re-enable the same Allow button after Android status refresh');
  await permissionPage.locator('[data-permission-request="overlay"]').click();
  ensure(JSON.stringify(await permissionPage.evaluate(() => window.__permissionTestRequests)) === '["files","overlay"]',
    'Overlay button did not call only the allowlisted native permission request');
  await permissionPage.locator('[data-permission-request="files"]').click();
  ensure(JSON.stringify(await permissionPage.evaluate(() => window.__permissionTestRequests)) === '["files","overlay","files"]',
    'Files button did not call the native Settings flow');
  ensure(await permissionPage.locator('#storageAccessState').innerText() === 'Not allowed',
    'Opening Android Settings must not claim that permission was granted');
  ensure(await permissionPage.locator('[data-permission-row="battery"] button').count() === 1 &&
    await permissionPage.locator('#batteryAppInfo').count() === 0 &&
    (await permissionPage.locator('[data-permission-request="battery"]').innerText()) === 'Allow' &&
    await permissionPage.locator('[data-permission-row="files"] button').count() === 1 &&
    await permissionPage.locator('[data-permission-row="overlay"] button').count() === 1 &&
    await permissionPage.locator('[data-permission-row="notifications"] button').count() === 1,
    'Battery optimized state must show exactly one Allow button for the native popup');
  await permissionPage.locator('[data-permission-request="battery"]').click();
  ensure((await permissionPage.evaluate(() => window.__permissionTestRequests)).includes('battery') &&
    await permissionPage.locator('#batteryAccessState').innerText() === 'Optimized' &&
    await permissionPage.locator('#androidCosExtensionInstall').isDisabled(),
    'Requesting package-targeted battery exemption without OS approval must keep the permission gate locked');
  ensure(!(await permissionPage.evaluate(() => window.__permissionTestRequests)).includes('battery_details'),
    'A second battery app-info request must never be exposed by the one-button flow');
  await permissionPage.evaluate(() => {
    window.__permissionTestData.files.granted = true;
    window.__permissionTestData.overlay.granted = true;
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.waitForFunction(() => document.getElementById('storageAccessState').textContent === 'Allowed');
  ensure((await permissionPage.locator('#storageAccessState').getAttribute('data-granted')) === 'true' &&
    await permissionPage.locator('[data-permission-request="files"]').isDisabled() &&
    await permissionPage.locator('[data-permission-request="files"]').innerText() === 'Allowed' &&
    await permissionPage.locator('[data-permission-request="overlay"]').isDisabled() &&
    await permissionPage.locator('[data-permission-request="overlay"]').innerText() === 'Allowed',
    'Returning from Android Settings must gray out and lock both granted Files and Overlay actions');
  ensure((await permissionPage.locator('[data-permission-request="notifications"]').innerText()) === 'Allow' &&
    await setupProgressMatchesTiles() &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending',
    'Setup must advance only the granted permission and keep Access pending until all supported grants exist');
  await permissionPage.evaluate(() => { window.__permissionRequestDenied = true; });
  await permissionPage.locator('[data-permission-request="notifications"]').click();
  ensure((await permissionPage.evaluate(() => window.__permissionTestRequests)).includes('notifications') &&
    await permissionPage.locator('#notificationAccessState').innerText() === 'Not allowed' &&
    await permissionPage.locator('[data-permission-request="notifications"]').isEnabled() &&
    await permissionPage.locator('[data-permission-request="notifications"]').innerText() === 'Allow' &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending',
    'Rejected native Allow request must never grant Android permission or verify Access');
  await permissionPage.evaluate(() => { window.__permissionRequestDenied = false; });
  await permissionPage.evaluate(() => { window.__permissionTestData.notifications.granted = true; });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() => document.getElementById('notificationAccessState').textContent === 'Allowed');
  ensure(await permissionPage.locator('[data-permission-request="notifications"]').isDisabled() &&
    await permissionPage.locator('[data-permission-request="notifications"]').innerText() === 'Allowed',
    'Granting Notifications must disable and gray out the one Allowed button');
  await permissionPage.evaluate(() => { window.__permissionTestData.notifications.granted = false; });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() => document.getElementById('notificationAccessState').textContent === 'Not allowed');
  ensure(await permissionPage.locator('[data-permission-request="notifications"]').isEnabled() &&
    await permissionPage.locator('[data-permission-request="notifications"]').innerText() === 'Allow',
    'Revoking Notifications must re-enable Allow after an OS status refresh');
  await permissionPage.evaluate(() => {
    window.__permissionFolder = {granted:true,treeUri:'content://com.android.externalstorage.documents/tree/primary%3ASynthetic'};
  });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() => document.querySelector('[data-cos-setup-step="folder"] small').textContent === 'Verified');
  ensure(await setupProgressMatchesTiles() &&
    (await permissionPage.locator('#androidCosSetupProgressText').innerText()).includes('1 verified'),
    'Step 4 must verify main shared storage after Android confirms All files access, independently of SAF');
  await permissionPage.evaluate(() => { window.__permissionFolder = {granted:true,treeUri:''}; });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() => document.querySelector('[data-cos-setup-step="folder"] small').textContent === 'Verified');
  ensure(await setupProgressMatchesTiles(),
    'Invalid optional SAF grant cannot remove confirmed main storage access');
  await permissionPage.evaluate(() => { window.__permissionFolder = {granted:false,treeUri:''}; });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() => document.querySelector('[data-cos-setup-step="folder"] small').textContent === 'Verified');
  await permissionPage.evaluate(() => { window.__permissionTestData.files.granted = false; });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() => document.querySelector('[data-cos-setup-step="folder"] small').textContent === 'Pending');
  ensure(await setupProgressMatchesTiles() &&
    (await permissionPage.locator('[data-cos-setup-step] small').allTextContents())
      .filter(value => value === 'Verified').length === 0,
    'Revoked All files access must remove main storage readiness, regardless of optional folder selection');
  await permissionPage.evaluate(() => { window.__permissionTestData.files.granted = true; });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() => document.querySelector('[data-cos-setup-step="folder"] small').textContent === 'Verified');
  await permissionPage.evaluate(() => { window.__permissionTestData.notifications.supported = false; });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() => document.getElementById('notificationAccessState').textContent === 'Not supported');
  ensure(await permissionPage.locator('[data-permission-request="notifications"]').isDisabled() &&
    await permissionPage.locator('[data-permission-request="battery"]').innerText() === 'Allow' &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending',
    'Unsupported Android permission must be disabled and excluded from gating without granting Access');
  await permissionPage.evaluate(() => { window.__permissionTestData.battery.granted = true; });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() => document.getElementById('batteryAccessState').textContent === 'Unrestricted');
  ensure(await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending' &&
    await permissionPage.locator('#accessibilityManage').isEnabled(),
    'Four system grants cannot silently enable or bypass the retained Accessibility service');
  await permissionPage.locator('#accessibilityManage').click();
  ensure(await permissionPage.evaluate(() => window.__accessibilityRequests) === 1 &&
    await permissionPage.locator('#accessibilityPermissionState').innerText() === 'Not enabled',
    'Accessibility Allow opens user-controlled system settings without granting access');
  await permissionPage.evaluate(() => {
    window.__accessibility.enabled = true;
    window.__accessibility.connected = false;
  });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() =>
    document.getElementById('accessibilityPermissionState').textContent === 'Recovering…');
  ensure(await permissionPage.locator('#accessibilityManage').isDisabled() &&
    await permissionPage.locator('#accessibilityManage').innerText() === 'Checking…' &&
    !(await permissionPage.locator('#accessibilityReconnectHint').innerText()).includes('Tap Reconnect') &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending',
    'OS-authorized temporarily unbound Accessibility must show Recovering with no misleading Reconnect action');
  await permissionPage.evaluate(() => { window.__accessibility.needsReconnect = true; });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() =>
    document.getElementById('accessibilityPermissionState').textContent === 'Reconnect needed');
  ensure(await permissionPage.locator('#accessibilityReconnectHint').isVisible() &&
    (await permissionPage.locator('#accessibilityReconnectHint').innerText()).includes('Mcp Agent Ai Accessibility') &&
    await permissionPage.locator('#accessibilityManage').isEnabled() &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending',
    'OS master disabled or exhausted recovery grace requires explicit Settings reauthorization');
  await permissionPage.locator('#accessibilityManage').click();
  ensure(await permissionPage.evaluate(() => window.__accessibilityRequests) === 2,
    'Reconnect action should open Android service Settings again without granting access');
  await permissionPage.evaluate(() => {
    window.__accessibility.connected = true;
    window.__accessibility.needsReconnect = false;
  });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() =>
    document.getElementById('accessibilityPermissionState').textContent === 'Connected');
  // No focus, button click or permission re-grant: the foreground native
  // watchdog must notice both a lost service binding and an OS rebind.
  await permissionPage.evaluate(() => {
    window.__accessibility.connected = false;
    window.__accessibility.needsReconnect = false;
  });
  await permissionPage.waitForFunction(() =>
    document.getElementById('accessibilityPermissionState').textContent === 'Recovering…');
  ensure(await permissionPage.locator('#accessibilityManage').isDisabled() &&
    await permissionPage.locator('#accessibilityManage').innerText() === 'Checking…' &&
    await permissionPage.locator('#accessibilityReconnectHint').isHidden() &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending',
    'Foreground watchdog must revoke stale Connected and show Recovering for temporary unbind');
  await permissionPage.evaluate(() => { window.__accessibility.connected = true; });
  await permissionPage.waitForFunction(() =>
    document.getElementById('accessibilityPermissionState').textContent === 'Connected');
  ensure(await permissionPage.evaluate(() => window.__accessibilityRequests) === 2,
    'Automatic Accessibility liveness recovery must not open OS Settings or request a new grant');
  // Android can preserve the service entry while its global accessibility
  // master is off or the service has crashed. Neither is evidence of binding.
  await permissionPage.evaluate(() => {
    window.__accessibility.connected = false;
    window.__accessibility.needsReconnect = true;
  });
  await permissionPage.waitForFunction(() =>
    document.getElementById('accessibilityPermissionState').textContent === 'Reconnect needed');
  ensure(await permissionPage.locator('#accessibilityManage').isEnabled() &&
    await permissionPage.locator('#accessibilityManage').innerText() === 'Reconnect' &&
    await permissionPage.locator('#accessibilityReconnectHint').isVisible() &&
    await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending' &&
    await permissionPage.evaluate(() => window.__accessibilityRequests) === 2,
    'OS blocked/crashed Accessibility must require a real user Settings action, not silently reconnect');
  await permissionPage.evaluate(() => {
    window.__accessibility.connected = true;
    window.__accessibility.needsReconnect = false;
  });
  await permissionPage.waitForFunction(() =>
    document.getElementById('accessibilityPermissionState').textContent === 'Connected');
  ensure(await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Pending' &&
    await permissionPage.locator('[data-permission-request="battery"]').isDisabled() &&
    await permissionPage.locator('[data-permission-request="battery"]').innerText() === 'Allowed' &&
    await permissionPage.locator('[data-cos-setup-step="ready"] small').innerText() === 'Pending' &&
    await setupProgressMatchesTiles() &&
    await permissionPage.locator('#accessibilityManage').isDisabled() &&
    await permissionPage.locator('#accessibilityReconnectHint').isHidden() &&
    await permissionPage.locator('#storageAccessSection').isVisible() &&
    await permissionPage.locator('[data-cos-setup-step="access"]').getAttribute('aria-current') === 'step' &&
    setupNetworkCalls.length === 0 &&
    await permissionPage.locator('#screenCaptureOne').count() === 0,
    'Unsupported notification status cannot trigger automatic five-permission transition');
  ensure(await permissionPage.locator('#androidCosSetupPrev,#androidCosSetupNext,#androidCosSetupRefresh').count() === 0 &&
    await permissionPage.locator('[data-cos-setup-step="extension"]').isEnabled() &&
    await permissionPage.locator('#androidCosBrowserChoices').isHidden(),
    'Chrome extension navigation is available without duplicate Back/Next/Check buttons');
  // The Android 11 notification permission is supported; only 5/5 OS grants
  // plus a connected Accessibility service can advance automatically.
  await permissionPage.evaluate(() => {
    window.__permissionTestData.notifications.supported = true;
    window.__permissionTestData.notifications.granted = true;
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="browser"]').getAttribute('aria-current') === 'step' &&
    document.querySelector('[data-cos-setup-step="extension"] small')?.textContent === 'Not needed');
  ensure(await permissionPage.locator('#androidCosBrowserChoices').isVisible() &&
    await permissionPage.locator('[data-optional-permission]').count() === 3 &&
    await permissionPage.locator('#androidOptionalMicrophoneState').innerText() === 'Not allowed' &&
    await permissionPage.locator('#androidOptionalLocationState').innerText() === 'Not allowed' &&
    await permissionPage.locator('#androidOptionalCameraState').innerText() === 'Not allowed' &&
    await permissionPage.locator('#androidCosSetupStepTitle').isHidden() &&
    await permissionPage.locator('#storageAccessSection').isHidden() &&
    await permissionPage.locator('#storageAccessSection').getAttribute('aria-hidden') === 'true' &&
    await permissionPage.locator('#androidCosSetupCurrent').isVisible() &&
    await permissionPage.locator('#notificationAccessState').innerText() === 'Allowed' &&
    await permissionPage.locator('#androidCosChooseBuiltIn').count() === 0 &&
    setupNetworkCalls.length === 0,
    'After fifth required OS grant, Step2 Not needed must be skipped for pending Step3; three optional grants remain non-gating');
  await permissionPage.locator('[data-cos-setup-step="access"]').click();
  ensure(await permissionPage.locator('#storageAccessSection').isVisible() &&
    await permissionPage.locator('#androidCosSetupCurrent').isHidden(),
    'Explicitly revisiting Permissions must keep all five Allow/Allowed controls visible without auto-bouncing');
  await permissionPage.locator('[data-cos-setup-step="extension"]').click();
  ensure(await permissionPage.locator('#storageAccessSection').isHidden(),
    'Switching back to Extensions must hide the entire App permissions card again');
  // Step 6 saves ONLY a non-secret Core ID. Persistent local storage confirms
  // Step 6, while Step 7 must separately authenticate against OpenAI.
  await permissionPage.evaluate(() => { window.__tunnelDeferSavedIdRead = true; });
  await permissionPage.locator('[data-cos-setup-step="tunnel"]').click();
  await permissionPage.waitForFunction(() =>
    typeof window.__tunnelReleaseSavedIdRead === 'function');
  ensure(await permissionPage.locator('#androidTunnelOpenTunnels').isVisible() &&
    await permissionPage.locator('#androidTunnelSaveId').isVisible() &&
    await permissionPage.locator('#androidTunnelSaveId').isDisabled() &&
    await permissionPage.locator('#androidCosTunnelAdvanced,#androidCosAdvancedFooter').count() === 0 &&
    await permissionPage.locator('#androidTunnelLegacyCompat').isHidden() &&
    await permissionPage.locator('#androidTunnelOpenApiKeys').isHidden() &&
    await permissionPage.locator('#androidTunnelSaveKey').isHidden() &&
    await permissionPage.locator('#androidTunnelRemoveStoredKey').isHidden() &&
    await permissionPage.locator('#androidOpenAiTunnelConnect').isHidden() &&
    await permissionPage.locator('#androidTunnelKeyNeededDialog').count() === 0 &&
    await permissionPage.locator('.android-tunnel-desktop-optional').count() === 0,
    'Step6 contains only Core ID and Save, no key dialog or old advanced/desktop controls');
  await permissionPage.locator('#androidTunnelOpenTunnels').click();
  ensure(JSON.stringify(await permissionPage.evaluate(() => window.__tunnelOpenPlatformEvents)) ===
    '["tunnels"]',
    'The only visible external action opens the allowlisted OpenAI Tunnels management');
  await permissionPage.locator('#androidOpenAiTunnelId').fill('tunnel_invalid!');
  ensure(await permissionPage.locator('#androidTunnelSaveId').isDisabled() &&
    await permissionPage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Pending',
    'Invalid Core ID cannot be saved or automatically advance');
  const firstValidId = 'tunnel_' + 'a'.repeat(32);
  await permissionPage.locator('#androidOpenAiTunnelId').fill(firstValidId);
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidTunnelSaveId')?.disabled === false);
  ensure(await permissionPage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Pending',
    'Valid-looking Core ID is not an authenticated tunnel');
  await permissionPage.evaluate(() => { window.__tunnelSaveRejectNext = true; });
  await permissionPage.locator('#androidTunnelSaveId').click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidTunnelIdStatus')?.textContent?.includes('Could not save') &&
    !document.getElementById('androidTunnelSaveId')?.disabled);
  ensure(await permissionPage.locator('[data-cos-setup-step="tunnel"]').getAttribute('aria-current') === 'step' &&
    await permissionPage.evaluate(() => window.__tunnelCoreIdSaved === null) &&
    await permissionPage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Pending',
    'Backend draft storage failure must not advance or falsely mark a Core tunnel ID saved');
  await permissionPage.locator('#androidTunnelSaveId').click();
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="key"]')?.getAttribute('aria-current') === 'step');
  ensure(await permissionPage.evaluate(() => window.__tunnelCoreIdSaved) === firstValidId &&
    JSON.stringify(await permissionPage.evaluate(() => window.__tunnelSaveDraftCalls)) === JSON.stringify([firstValidId]) &&
    await permissionPage.locator('#androidTunnelKeyNeededInput').isVisible() &&
    await permissionPage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Verified' &&
    await permissionPage.locator('[data-cos-setup-step="connect"] small').innerText() === 'Pending' &&
    await permissionPage.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() === 'Pending' &&
    await permissionPage.locator('#androidTunnelVerifyCore,#androidTunnelKeyNeededCancel').count() === 0 &&
    await permissionPage.evaluate(() => window.__tunnelVerifyCalls.length === 0),
    'Saved Core ID verifies local Step6 and opens Step7, while remote MCP remains pending');
  await permissionPage.evaluate(() => { window.__tunnelReleaseSavedIdRead(); });
  await permissionPage.waitForFunction(() => window.__tunnelSavedIdReadReturned === true);
  ensure((await permissionPage.locator('#androidTunnelStep7Status').innerText()).includes('automatically') &&
    await permissionPage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Verified' &&
    await permissionPage.locator('#androidTunnelKeyNeededConfirm').isHidden() &&
    await permissionPage.locator('#androidTunnelRemoveStoredKey').isHidden() &&
    await permissionPage.locator('[data-cos-setup-step="key"]').getAttribute('aria-current') === 'step',
    'Late native ID read must not overwrite a newer successful Step6 Save');
  const keySectionLayout = await permissionPage.locator('#androidCosKeyPanel').evaluate(panel => ({
    steps:panel.querySelectorAll('.android-tunnel-step7-steps li').length,
    inputType:panel.querySelector('#androidTunnelKeyNeededInput')?.getAttribute('type'),
    includesProvider:!!panel.querySelector('#androidCosProvider, #androidCosModel, #androidCosApiKey, #androidCosUseKey, #androidCosClearKey'),
    hasLegacyActions:!!panel.querySelector('#androidTunnelVerifyCore, #androidTunnelKeyNeededCancel'),
  }));
  ensure(keySectionLayout.steps === 3 && keySectionLayout.inputType === 'password' &&
    !keySectionLayout.includesProvider && !keySectionLayout.hasLegacyActions &&
    await permissionPage.locator('#androidTunnelKeyNeededOpenApiKeys').isVisible() &&
    await permissionPage.locator('#androidCosProvider').isHidden(),
    `Step7 must contain only three API key instructions, masked input, contextual Save/Remove and status: ${JSON.stringify(keySectionLayout)}`);
  await permissionPage.screenshot({
    path:path.join(screenshotFolder,'native-android-create-api-key-step7-390.png'),fullPage:true,
  });
  await permissionPage.locator('#androidTunnelKeyNeededOpenApiKeys').click();
  ensure(JSON.stringify(await permissionPage.evaluate(() => window.__tunnelOpenPlatformEvents)) ===
    '["tunnels","api_keys"]',
    'Step7 opens allowlisted API keys with a direct human action');
  await permissionPage.evaluate(() => {
    window.__tunnelVerifyResponse = 'unauthorized';
  });
  const syntheticRuntimeKey = 'synthetic-secure-runtime-key';
  await permissionPage.locator('#androidTunnelKeyNeededInput').fill(syntheticRuntimeKey);
  ensure(await permissionPage.locator('#androidTunnelKeyNeededConfirm').isVisible() &&
    await permissionPage.locator('#androidTunnelKeyNeededConfirm').isEnabled() &&
    await permissionPage.locator('#androidTunnelRemoveStoredKey').isHidden(),
    'Step7 Save appears only with a typed key and Remove appears only after native secure storage');
  await permissionPage.locator('#androidTunnelKeyNeededConfirm').click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidTunnelStep7Status')?.textContent?.includes('denied') &&
    window.__tunnelSavedCredential.stored === true &&
    document.getElementById('androidTunnelRemoveStoredKey')?.hidden === false);
  ensure(await permissionPage.locator('[data-cos-setup-step="key"]').getAttribute('aria-current') === 'step' &&
    await permissionPage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Verified' &&
    await permissionPage.locator('[data-cos-setup-step="key"] small').innerText() === 'Pending' &&
    await permissionPage.locator('#androidTunnelKeyNeededInput').inputValue() === '' &&
    await permissionPage.locator('#androidTunnelKeyNeededConfirm').isHidden() &&
    await permissionPage.locator('#androidTunnelRemoveStoredKey').isVisible() &&
    JSON.stringify(await permissionPage.evaluate(() => window.__tunnelCredentialEvents)) === '["store"]',
    'Step7 key stored securely, but OpenAI unauthorized keeps Step7 Pending while saved Step6 stays Verified');
  const secretState = await permissionPage.evaluate(() => {
    const input = document.getElementById('androidTunnelKeyNeededInput');
    const status = document.getElementById('androidTunnelKeyNeededStatus');
    const secret = 'synthetic-secure-runtime-key';
    return {placeholder:input.placeholder,value:input.value,
      disclosed:document.body.innerText.includes(secret) ||
        (status?.textContent || '').includes(secret),
      type:input.type,stored:window.__tunnelSavedCredential.stored};
  });
  ensure(secretState.stored === true && secretState.type === 'password' &&
    secretState.value === '' && !secretState.disclosed &&
    /stored|saved|•|\*/i.test(secretState.placeholder) &&
    !secretState.placeholder.includes(syntheticRuntimeKey),
    `Stored Runtime key must have a masked placeholder and never echo the secret: ${JSON.stringify({...secretState,placeholder:secretState.placeholder})}`);
  await permissionPage.screenshot({
    path:path.join(screenshotFolder,'native-android-create-api-key-stored-step7-390.png'),fullPage:true,
  });
  await permissionPage.evaluate(() => { window.__tunnelVerifyResponse = 'not_found'; });
  await permissionPage.locator('#androidTunnelKeyNeededInput').fill(syntheticRuntimeKey);
  await permissionPage.locator('#androidTunnelKeyNeededConfirm').click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidTunnelStep7Status')?.textContent?.includes('was not found'));
  ensure(await permissionPage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Verified' &&
    await permissionPage.locator('[data-cos-setup-step="key"] small').innerText() === 'Pending',
    'Auto-verifying a new API key must keep Step7 pending after OpenAI metadata 404');
  await permissionPage.evaluate(() => { window.__tunnelVerifyResponse = 'ok'; });
  await permissionPage.locator('#androidTunnelKeyNeededInput').fill(syntheticRuntimeKey);
  await permissionPage.locator('#androidTunnelKeyNeededConfirm').click();
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="key"] small')?.textContent === 'Verified');
  ensure(await permissionPage.evaluate(() => window.__tunnelCoreIdSaved) === firstValidId &&
    await permissionPage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Verified' &&
    await permissionPage.locator('[data-cos-setup-step="key"] small').innerText() === 'Verified' &&
    await permissionPage.locator('#androidTunnelKeyNeededInput').inputValue() === '' &&
    await permissionPage.locator('#androidTunnelRemoveStoredKey').isVisible() &&
    JSON.stringify(await permissionPage.evaluate(() => window.__tunnelCredentialEvents)) === '["store","store","store"]' &&
    (await permissionPage.evaluate(() => window.__tunnelVerifyCalls)).length === 3 &&
    await permissionPage.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() === 'Pending' &&
    await permissionPage.locator('[data-cos-setup-step="ready"] small').innerText() === 'Pending' &&
    JSON.stringify(await permissionPage.evaluate(() => window.__permissionTunnelEvents)) === '[]',
    'Step7 Save must encrypt/store and auto-verify metadata without starting the native tunnel or proving Step9 remote tool access');
  await permissionPage.locator('[data-cos-setup-step="connect"]').click();
  ensure(await permissionPage.locator('#androidCosChatgptConnect').isEnabled() &&
    await permissionPage.locator('#androidCosTunnelDisconnect').isDisabled() &&
    await permissionPage.locator('[data-cos-setup-step="connect"] small').innerText() === 'Pending' &&
    await permissionPage.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() === 'Pending',
    'Only Step8 enables the native tunnel Connect after the saved Core ID and restricted key are authorized');
  await permissionPage.locator('#androidCosChatgptConnect').click();
  await permissionPage.waitForFunction(() =>
    window.__permissionTunnelEvents.includes('connect_stored') &&
    document.querySelector('[data-cos-setup-step="connect"] small')?.textContent === 'Verified' &&
    document.querySelector('[data-cos-setup-step="mcpPlugin"] small')?.textContent === 'Pending');
  ensure(await permissionPage.locator('#androidCosChatgptConnect').isDisabled() &&
    await permissionPage.locator('#androidCosTunnelDisconnect').isEnabled() &&
    JSON.stringify(await permissionPage.evaluate(() => window.__permissionTunnelEvents)) === '["connect_stored"]',
    'Step8 native tunnel Connect succeeds in polling state without falsely verifying Step9 inbound tool access');
  await permissionPage.screenshot({path:path.join(screenshotFolder,'native-android-secure-tunnel-step8-connected-390.png'),fullPage:true});
  await permissionPage.locator('[data-cos-setup-step="mcpPlugin"]').click();
  ensure(await permissionPage.locator('#androidCosMcpPluginRefresh').count() === 0 &&
    await permissionPage.locator('#androidCosMcpPluginConnect,#androidCosMcpPluginDisconnect').count() === 0,
    'Step9 must auto-check native inbound tool evidence without Refresh or duplicate Connect/Disconnect');
  ensure(await permissionPage.locator('#androidCosMcpPluginOpen').isVisible() &&
    await permissionPage.locator('#androidCosMcpPhoneName').isVisible() &&
    await permissionPage.locator('#androidCosMcpCopyName').isVisible() &&
    await permissionPage.locator('#androidCosMcpCopyDescription').isVisible() &&
    await permissionPage.locator('#androidCosMcpName').inputValue() === 'Mcp Agent Ai Core' &&
    !(await permissionPage.evaluate(() => window.__tunnelOpenPlatformEvents)).includes('chatgpt_plugins'),
    'Step9 must show the editable Core Plugin details and never open ChatGPT Plugins automatically');
  const emptyCopyStatusLayout = await permissionPage.locator('#androidCosMcpPluginCopyStatus')
    .evaluate(node => ({
      empty:node.textContent.trim().length === 0,
      display:getComputedStyle(node).display,
      height:node.getBoundingClientRect().height
    }));
  ensure(emptyCopyStatusLayout.empty && emptyCopyStatusLayout.display === 'none' &&
    emptyCopyStatusLayout.height === 0,
    `Step9 empty Copy status must have no blank spacer: ${JSON.stringify(emptyCopyStatusLayout)}`);
  await permissionPage.locator('#androidCosMcpPhoneName').fill('Haris');
  const corePluginName = 'Mcp Agent Ai Core (Haris)';
  const corePluginDescription = await permissionPage.locator('#androidCosMcpDescription').inputValue();
  ensure(await permissionPage.locator('#androidCosMcpName').inputValue() === corePluginName &&
    corePluginDescription === 'Connect ChatGPT to the approved MCP tools of Mcp Agent Ai on this Android phone.' &&
    await permissionPage.evaluate(() =>
      localStorage.getItem('mcp-agent-ai-android-core-plugin-phone-v1')) === 'Haris' &&
    await permissionPage.locator('#androidCosMcpPhoneName').getAttribute('aria-invalid') === 'false',
    'User-edited phone label must update the exact copyable Core name and persist a validated suffix');
  await permissionPage.locator('#androidCosMcpPhoneName').fill('!untrusted suffix');
  ensure(await permissionPage.locator('#androidCosMcpPhoneName').getAttribute('aria-invalid') === 'true' &&
    await permissionPage.locator('#androidCosMcpPhoneError').isVisible() &&
    await permissionPage.locator('#androidCosMcpCopyName').isDisabled() &&
    await permissionPage.locator('#androidCosMcpCopyDescription').isDisabled() &&
    await permissionPage.evaluate(() =>
      localStorage.getItem('mcp-agent-ai-android-core-plugin-phone-v1')) === 'Haris',
    'Invalid phone label must disable copying, show validation feedback and preserve prior valid saved name');
  await permissionPage.locator('#androidCosMcpPhoneName').fill('Haris');
  await permissionPage.evaluate(() => {
    window.__corePluginClipboardWrites = [];
    Object.defineProperty(navigator,'clipboard',{configurable:true,value:{writeText:async value => {
      window.__corePluginClipboardWrites.push(value);
    }}});
  });
  await permissionPage.locator('#androidCosMcpCopyName').click();
  await permissionPage.locator('#androidCosMcpCopyDescription').click();
  await permissionPage.waitForFunction(() => window.__corePluginClipboardWrites.length === 2);
  ensure(JSON.stringify(await permissionPage.evaluate(() => window.__corePluginClipboardWrites)) ===
    JSON.stringify([corePluginName,corePluginDescription]) &&
    (await permissionPage.locator('#androidCosMcpPluginCopyStatus').innerText()).includes('Description copied'),
    'Copy buttons must place exact Name and Description text on the clipboard and acknowledge success');
  ensure(await permissionPage.locator('#androidCosMcpPluginCopyStatus').evaluate(node =>
    getComputedStyle(node).display !== 'none' && node.getBoundingClientRect().height > 0),
    'Step9 live copy feedback must become visible after a Copy action');
  await permissionPage.locator('#androidCosMcpPhoneName').fill('');
  ensure(await permissionPage.locator('#androidCosMcpName').inputValue() === 'Mcp Agent Ai Core' &&
    await permissionPage.locator('#androidCosMcpPhoneName').getAttribute('aria-invalid') === 'false' &&
    await permissionPage.locator('#androidCosMcpPhoneError').isHidden() &&
    await permissionPage.locator('#androidCosMcpCopyName').isEnabled() &&
    await permissionPage.locator('#androidCosMcpCopyDescription').isEnabled() &&
    await permissionPage.evaluate(() =>
      localStorage.getItem('mcp-agent-ai-android-core-plugin-phone-v1')) === '',
    'Clearing the optional Core name must show the unsuffixed Mcp Agent Ai Core and keep Copy available');
  await permissionPage.locator('#androidCosMcpCopyName').click();
  await permissionPage.waitForFunction(() => window.__corePluginClipboardWrites.length === 3);
  ensure((await permissionPage.evaluate(() => window.__corePluginClipboardWrites.at(-1))) === 'Mcp Agent Ai Core',
    'The Copy action must copy the plain Core name when the optional field is empty');
  await permissionPage.locator('#androidCosMcpPhoneName').fill('Haris');
  await permissionPage.evaluate(() => {
    Object.defineProperty(navigator,'clipboard',{configurable:true,value:{writeText:async () => {
      throw Error('Synthetic clipboard permission denial');
    }}});
  });
  await permissionPage.locator('#androidCosMcpCopyName').click();
  ensure((await permissionPage.locator('#androidCosMcpPluginCopyStatus').innerText()).includes('selected for manual copying') &&
    await permissionPage.locator('#androidCosMcpName').evaluate(input =>
      document.activeElement === input && input.selectionStart === 0 &&
      input.selectionEnd === input.value.length),
    'Clipboard failure must visibly explain the manual fallback and select the exact name');
  await permissionPage.locator('[data-cos-setup-step="connect"]').click();
  await permissionPage.locator('[data-cos-setup-step="mcpPlugin"]').click();
  ensure(await permissionPage.locator('#androidCosMcpPhoneName').inputValue() === 'Haris' &&
    await permissionPage.locator('#androidCosMcpName').inputValue() === corePluginName,
    'Core Plugin phone suffix must survive navigating out of and back into Step9');
  const pluginOpenBefore = await permissionPage.evaluate(() => window.__tunnelOpenPlatformEvents.length);
  ensure(pluginOpenBefore === 2,
    'Opening Step9 must not automatically launch a browser (previous explicit Tunnels and API Keys only)');
  const deniedPluginOpen = await permissionPage.evaluate(async () => {
    const invoke = window.__TAURI_INTERNALS__.invoke;
    try { await invoke('android_tunnel_open_platform_page',{kind:'chatgpt_plugins',userInitiated:false}); return false; }
    catch { return true; }
  });
  ensure(deniedPluginOpen &&
    await permissionPage.evaluate(() => window.__tunnelOpenPlatformEvents.length) === pluginOpenBefore,
    'Native ChatGPT Plugins destination must reject calls without an explicit user gesture');
  await permissionPage.locator('#androidCosMcpPluginOpen').click();
  await permissionPage.waitForFunction(() =>
    window.__tunnelOpenPlatformEvents.includes('chatgpt_plugins'));
  ensure(JSON.stringify(await permissionPage.evaluate(() => window.__tunnelOpenPlatformEvents)) ===
    '["tunnels","api_keys","chatgpt_plugins"]' &&
    (await permissionPage.locator('#androidCosMcpPluginCopyStatus').innerText()).includes('ChatGPT Plugins opened') &&
    await permissionPage.locator('#androidCosMcpPluginStatus').isVisible(),
    'Only the Open ChatGPT Plugins button may launch the allowlisted native Custom Tab; opening does not verify Step9');
  // Reproduce the reported user journey: creating a Core Plugin in ChatGPT is
  // an external account action. Returning to Setup supplies no native proof
  // unless ChatGPT also made an allowed MCP tools/call and OpenAI ACKed it.
  await permissionPage.evaluate(() => {
    window.__chatgptCorePluginCreated = true;
    window.dispatchEvent(new Event('focus'));
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="mcpPlugin"] small')?.textContent === 'Pending');
  const pluginReturn = await permissionPage.evaluate(() => ({
    created:window.__chatgptCorePluginCreated,
    tunnel:structuredClone(window.__permissionTunnel),
    connected:document.querySelector('[data-cos-setup-step="connect"] small')?.textContent,
    step9:document.querySelector('[data-cos-setup-step="mcpPlugin"] small')?.textContent,
    evidence:document.getElementById('androidCosMcpPluginEvidence')?.textContent,
    progress:Number(document.getElementById('androidCosSetupProgress')?.getAttribute('aria-valuenow')),
    badges:[...document.querySelectorAll('[data-cos-setup-step] small')].map(node => node.textContent),
  }));
  ensure(pluginReturn.created && pluginReturn.tunnel.state === 'polling' &&
    pluginReturn.tunnel.pollActive && pluginReturn.tunnel.toolCallsVerified === 0 &&
    pluginReturn.connected === 'Verified' && pluginReturn.step9 === 'Pending' &&
    /No tools\/call response has been accepted yet/.test(pluginReturn.evidence || '') &&
    pluginReturn.progress === pluginReturn.badges
      .filter(label => label === 'Verified' || label === 'Not needed').length,
    'Creating the ChatGPT Core Plugin then returning to the app must remain Pending without a native inbound tools/call receipt: ' +
      JSON.stringify(pluginReturn));
  // A successful inbound tool call can arrive while Step9 is open. The next
  // read-only native status poll should recognize it without pressing Refresh.
  const statusReadsBeforeTool = await permissionPage.evaluate(() => {
    const before = window.__tunnelNativeStatusReads;
    window.__permissionTunnel = {state:'tool_verified',pollActive:true,toolCallsVerified:1};
    return before;
  });
  await permissionPage.waitForFunction(readsBefore =>
    window.__tunnelNativeStatusReads > readsBefore &&
    document.querySelector('[data-cos-setup-step="mcpPlugin"] small')?.textContent === 'Verified',
    statusReadsBeforeTool, {timeout:12000});
  ensure(await permissionPage.locator('[data-cos-setup-step="connect"] small').innerText() === 'Verified' &&
    await setupProgressMatchesTiles(),
    'Step9 must automatically recognize a new valid native tool response while its setup page is visible');
  const expectAutomaticTunnelStatus = async (snapshot, expectedBadge) => {
    const previousReads = await permissionPage.evaluate(status => {
      window.__permissionTunnel = status;
      return window.__tunnelNativeStatusReads;
    }, snapshot);
    await permissionPage.waitForFunction(({reads,badge}) =>
      window.__tunnelNativeStatusReads > reads &&
      document.querySelector('[data-cos-setup-step="mcpPlugin"] small')?.textContent === badge,
      {reads:previousReads,badge:expectedBadge},{timeout:15000});
  };
  await expectAutomaticTunnelStatus({state:'polling',pollActive:true,toolCallsVerified:0},'Pending');
  ensure(await permissionPage.locator('#androidCosMcpPluginRefresh').count() === 0,
    'A new native receipt must revoke prior Step9 proof without exposing manual refresh');
  // This is a synthetic *native status* fixture, not a fake permission receipt:
  // Step9 must check current native state and a positive tool-call count.
  await expectAutomaticTunnelStatus({state:'polling',pollActive:true,toolCallsVerified:17},'Pending');
  ensure(await permissionPage.locator('[data-cos-setup-step="connect"] small').innerText() === 'Verified' &&
    await permissionPage.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() === 'Pending',
    'A positive counter while native tunnel state is only polling cannot verify Step9');
  await expectAutomaticTunnelStatus({state:'tool_verified',pollActive:false,toolCallsVerified:1},'Unable to check');
  ensure(await permissionPage.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() === 'Unable to check',
    'Inconsistent native tool_verified plus inactive polling is rejected as an invalid receipt, never accepted as Step9 proof');
  await expectAutomaticTunnelStatus({state:'tool_verified',pollActive:true,toolCallsVerified:0},'Pending');
  ensure(await permissionPage.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() === 'Pending',
    'Native tool_verified label without a positive completed-call counter cannot verify Step9');
  await expectAutomaticTunnelStatus({state:'tool_verified',pollActive:true,toolCallsVerified:1},'Verified');
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="mcpPlugin"] small')?.textContent === 'Verified');
  ensure(await permissionPage.locator('[data-cos-setup-step="ready"] small').innerText() === 'Pending' &&
    (await permissionPage.locator('#androidCosMcpPluginStatus').innerText()).includes('1') &&
    await setupProgressMatchesTiles(),
    'Only an active tool_verified native status with one completed inbound call verifies Step9');
  // Reopen with fresh renderer memory and native-only persisted pairing. The
  // original page observed a genuine, ACKed tool response above. A cold start
  // resets the session counter, while native storage retains that ACK proof.
  const restartedCorePage = await browser.newPage({
    viewport:{width:390,height:844},isMobile:true,hasTouch:true,
    userAgent:'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36'
  });
  restartedCorePage.on('pageerror',error => errors.push(error.message));
  await restartedCorePage.addInitScript(coreId => {
    window.isTauri = true;
    window.__coldCoreId = coreId;
    window.__coldCoreStatus = {
      state:'polling',pollActive:true,toolCallsVerified:0,corePluginLinked:true
    };
    window.__coldCoreStatusReads = 0;
    window.__coldCoreNativeCalls = [];
    const capabilities = ['browseFolders','searchFiles','readFiles','fileMetadata',
      'createFiles','editFiles','moveFiles','deleteFiles','browserControl','terminal'];
    const requested = Object.fromEntries(capabilities.map((key,index) => [key,index < 4]));
    const unavailable = Object.fromEntries(capabilities.map(key => [key,false]));
    window.__TAURI_INTERNALS__ = {invoke:async command => {
      window.__coldCoreNativeCalls.push(command);
      if (command === 'android_openai_tunnel_status') {
        window.__coldCoreStatusReads++;
        return structuredClone(window.__coldCoreStatus);
      }
      if (command === 'android_tunnel_saved_id_status')
        return {tunnelId:coreId,verified:false};
      if (command === 'android_tunnel_credentials_status')
        return {stored:true,secure:true};
      if (command === 'android_tunnel_verify_stored_access')
        return {tunnelId:coreId,verified:true,reason:'ok'};
      if (command === 'android_permissions_status')
        return Object.fromEntries(['files','overlay','notifications','battery']
          .map(key => [key,{supported:true,granted:true}]));
      if (command === 'accessibility_status')
        return {supported:true,enabled:true,connected:true,needsReconnect:false};
      if (command === 'android_optional_permissions_status')
        return Object.fromEntries(['camera','microphone','location']
          .map(key => [key,{supported:true,granted:false}]));
      if (command === 'android_main_storage_status')
        return {supported:true,granted:true,readable:true,rootPath:'/storage/emulated/0'};
      if (command === 'android_main_storage_selection_status')
        return {path:['Download'],selected:true};
      if (command === 'document_tree_status')
        return {granted:false,treeUri:''};
      if (command === 'custom_gecko_list_extensions')
        return {runtimeStarted:true,extensions:[]};
      if (command === 'android_chatgpt_oauth_status')
        return {supported:true,state:'linked',pending:false,signInVerified:true,
          sessionLinked:true,planEnabled:true,message:'Native OAuth remains authorized.'};
      if (command === 'android_mcp_permissions_status')
        return {preset:'read_only',requested,effective:unavailable,available:unavailable};
      throw Error('Unexpected native cold-restart check: ' + command);
    }};
  },firstValidId);
  await restartedCorePage.goto(app.url);
  await restartedCorePage.locator('#sidebarToggle').click();
  await restartedCorePage.locator('#sidebarCustomize').click();
  await restartedCorePage.locator('[data-android-settings-category="setup"]').click();
  await restartedCorePage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="mcpPlugin"] small')?.textContent === 'Verified' &&
    document.querySelector('[data-cos-setup-step="ready"] small')?.textContent === 'Verified' &&
    document.querySelector('[data-cos-setup-step="ready"]')?.getAttribute('aria-current') === 'step',
    null,{timeout:15000});
  const coldCorePairing = await restartedCorePage.evaluate(() => ({
    statuses:[...document.querySelectorAll('[data-cos-setup-step] small')].map(node => node.textContent),
    status:document.getElementById('androidCosMcpPluginStatus')?.textContent,
    evidence:document.getElementById('androidCosMcpPluginEvidence')?.textContent,
    progress:Number(document.getElementById('androidCosSetupProgress')?.getAttribute('aria-valuenow')),
    sessionCalls:window.__coldCoreStatus.toolCallsVerified,
    nativeCalls:window.__coldCoreNativeCalls
  }));
  ensure(coldCorePairing.sessionCalls === 0 && coldCorePairing.statuses[8] === 'Verified' &&
    coldCorePairing.statuses[9] === 'Verified' && coldCorePairing.progress === 10 &&
    coldCorePairing.statuses.every(value => value === 'Verified' || value === 'Not needed') &&
    /linked previously/i.test(coldCorePairing.status || '') &&
    /No new tool call has been claimed for this session/i.test(coldCorePairing.evidence || '') &&
    !coldCorePairing.nativeCalls.some(call => /connect_stored|credentials_store|oauth_start/.test(call)),
    'Cold restart must restore Ready from the native ACKed historical pairing, without a fresh tools/call or hidden connection: ' +
      JSON.stringify(coldCorePairing));
  const checkColdCoreStatus = async (snapshot,expectedStep9,expectedReady) => {
    const reads = await restartedCorePage.evaluate(status => {
      window.__coldCoreStatus = status;
      window.dispatchEvent(new Event('focus'));
      return window.__coldCoreStatusReads;
    },snapshot);
    await restartedCorePage.waitForFunction(({before,plugin,ready}) =>
      window.__coldCoreStatusReads > before &&
      document.querySelector('[data-cos-setup-step="mcpPlugin"] small')?.textContent === plugin &&
      document.querySelector('[data-cos-setup-step="ready"] small')?.textContent === ready,
      {before:reads,plugin:expectedStep9,ready:expectedReady},{timeout:15000});
  };
  await checkColdCoreStatus(
    {state:'polling',pollActive:true,toolCallsVerified:0,corePluginLinked:false},'Pending','Pending');
  await checkColdCoreStatus(
    {state:'disconnected',pollActive:false,toolCallsVerified:0,corePluginLinked:false},'Pending','Pending');
  await checkColdCoreStatus(
    {state:'error',pollActive:false,toolCallsVerified:0,corePluginLinked:false},'Pending','Pending');
  await checkColdCoreStatus(
    {state:'tool_verified',pollActive:true,toolCallsVerified:1,corePluginLinked:false},'Verified','Verified');
  ensure(/1 inbound MCP tool call/.test(await restartedCorePage.locator('#androidCosMcpPluginEvidence').innerText()) &&
    await restartedCorePage.locator('[data-cos-setup-step="ready"] small').innerText() === 'Verified',
    'Fresh ACKed tool_verified receipt must report a current-session call, not historical linkage');
  await restartedCorePage.locator('#androidSettingsBack').click();
  await restartedCorePage.locator('[data-android-settings-category="workspace"]').click();
  await restartedCorePage.waitForFunction(() =>
    document.querySelector('#androidWorkspaceFoldersSlot #androidCosSelectedPath')?.textContent ===
      '/storage/emulated/0/Download' &&
    document.querySelector('input[name="androidMcpAccessPreset"][value="read_only"]')?.checked);
  ensure(await restartedCorePage.locator('#androidWorkspaceReadOnly').getAttribute('aria-pressed') === 'true' &&
    await restartedCorePage.locator('#androidCosFolderReset').isEnabled() &&
    await restartedCorePage.locator('#androidWorkspacePluginStatus').innerText() === 'linked' &&
    await restartedCorePage.locator('#androidWorkspaceVerifiedLink').innerText() === '—' &&
    await restartedCorePage.locator('#androidWorkspaceLastCall').innerText() === '—' &&
    await restartedCorePage.locator('#androidWorkspaceActivityFeed .android-workspace-activity-row').count() === 0 &&
    await restartedCorePage.locator('#androidWorkspaceCopyActivity').isDisabled(),
    'Fresh native renderer restores folder and policy, but a current tool counter alone cannot fabricate event timestamps or activity entries');
  await restartedCorePage.locator('#androidSettingsBack').click();
  await restartedCorePage.locator('[data-android-settings-category="setup"]').click();
  await restartedCorePage.locator('[data-cos-setup-step="folder"]').click();
  ensure(await restartedCorePage.locator('#androidCosSelectedPath').innerText() === '/storage/emulated/0/Download',
    'Cold restart must restore the exact same selected folder in Setup Step4');
  await restartedCorePage.locator('[data-cos-setup-step="tools"]').click();
  ensure(await restartedCorePage.locator('input[name="androidMcpAccessPreset"][value="read_only"]').isChecked(),
    'Cold restart must restore saved native policy in Setup Step5');
  await restartedCorePage.close();
  await expectAutomaticTunnelStatus({state:'polling',pollActive:true,toolCallsVerified:0},'Pending');
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="mcpPlugin"] small')?.textContent === 'Pending');
  ensure(await setupProgressMatchesTiles(),
    'Losing native tool-call evidence must revoke Step9 verification and update progress');
  await permissionPage.locator('[data-cos-setup-step="connect"]').click();
  await permissionPage.locator('#androidCosTunnelDisconnect').click();
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="connect"] small')?.textContent === 'Pending');
  ensure(await permissionPage.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() === 'Pending',
    'Disconnect in Step8 must revoke native tunnel readiness without claiming a verified MCP Plugin');
  await permissionPage.locator('[data-cos-setup-step="mcpPlugin"]').click();
  ensure(await permissionPage.locator('#androidCosMcpPluginSkip,#androidOpenAiTunnelStandalone').count() === 0 &&
    await permissionPage.locator('[data-cos-setup-step="mcpPlugin"] small').innerText() === 'Pending' &&
    await permissionPage.locator('[data-cos-setup-step="ready"] small').innerText() === 'Pending' &&
    await setupProgressMatchesTiles(),
    'Step 9 has no standalone bypass; Disconnect leaves Core Plugin and Ready pending');
  await permissionPage.locator('[data-cos-setup-step="key"]').click();
  await permissionPage.locator('#androidTunnelRemoveStoredKey').click();
  await permissionPage.waitForFunction(() =>
    window.__tunnelSavedCredential.stored === false &&
    document.getElementById('androidTunnelRemoveStoredKey')?.hidden === true &&
    document.querySelector('[data-cos-setup-step="key"] small')?.textContent === 'Pending');
  ensure(await permissionPage.locator('#androidTunnelRemoveStoredKey').isHidden() &&
    await permissionPage.locator('#androidTunnelKeyNeededConfirm').isHidden() &&
    await permissionPage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Verified' &&
    await permissionPage.locator('[data-cos-setup-step="key"] small').innerText() === 'Pending' &&
    (await permissionPage.evaluate(() => window.__tunnelCoreIdSaved)) === firstValidId,
    'Removing Runtime key revokes Step7 auth proof but preserves saved Step6 Core ID');
  await permissionPage.locator('[data-cos-setup-step="tunnel"]').click();
  ensure((await permissionPage.locator('#androidTunnelIdStatus').innerText()).includes('saved') &&
    await permissionPage.locator('#androidOpenAiTunnelId').inputValue() === firstValidId,
    'Revisited Step6 still shows its saved Core ID without a runtime key dialog');
  await permissionPage.locator('#androidOpenAiTunnelId').fill('tunnel_' + 'b'.repeat(32));
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidTunnelSaveId')?.disabled === false);
  ensure(await permissionPage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Pending',
    'Unsaved edits to a previously saved ID must revoke Step6 local ID match');
  await permissionPage.locator('[data-cos-setup-step="extension"]').click();
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="extension"] small')?.textContent === 'Not needed');
  ensure(await permissionPage.locator('#androidCosExtensionControls').isVisible() &&
    await permissionPage.locator('#androidCosExtensionControls button').count() === 2 &&
    await permissionPage.locator('#androidCosExtensionOpen strong').innerText() === 'Open Extensions' &&
    await permissionPage.locator('#androidCosExtensionInstall strong').innerText() === 'Install Extension' &&
    await permissionPage.locator('#androidCosExtensionControls .android-cos-extension-action-icon').count() === 2 &&
    await permissionPage.locator('#storageAccessSection').isHidden() &&
    await permissionPage.locator('#androidCosExtensionOpen').isEnabled() &&
    await permissionPage.locator('#androidCosExtensionInstall').isEnabled() &&
    (await permissionPage.locator('#androidCosExtensionStatus').innerText()).includes('Not installed') &&
    await permissionPage.locator('#customBrowserSettings').count() === 0,
    'Native auto-check must report Not installed without Phase A or duplicate Custom Browser panel');
  const extensionLayout = await permissionPage.locator('#androidCosExtensionControls').evaluate(node => {
    const actions = [...node.querySelectorAll('.android-cos-extension-action')];
    const bounds = actions.map(button => button.getBoundingClientRect());
    return {x:bounds.map(rect=>rect.x),right:bounds.map(rect=>rect.right),
      top:bounds.map(rect=>rect.top),bottom:bounds.map(rect=>rect.bottom),
      width:bounds.map(rect=>rect.width),height:bounds.map(rect=>rect.height),
      viewport:document.documentElement.clientWidth,
      overflow:node.scrollWidth>node.clientWidth+1};
  });
  ensure(!extensionLayout.overflow &&
    extensionLayout.x.length === 2 &&
    Math.abs(extensionLayout.x[0]-extensionLayout.x[1]) <= 2 &&
    extensionLayout.top[1] >= extensionLayout.bottom[0] &&
    extensionLayout.width.every(width=>width>=200) &&
    extensionLayout.height.every(height=>height>=44) &&
    extensionLayout.right.every(right=>right<=extensionLayout.viewport+1),
    'Step 2 compact full-width action rows must be stacked, touch-friendly and fully visible');
  await permissionPage.screenshot({path:path.join(screenshotFolder,'native-android-cos-extension-step.png'),fullPage:true});
  await permissionPage.locator('#androidCosExtensionOpen').click();
  const managerBounds = await permissionPage.locator('#androidCosExtensionManager').evaluate(node => {
    const rect = node.getBoundingClientRect();
    return {x:rect.x,y:rect.y,width:rect.width,height:rect.height,
      viewportWidth:innerWidth,viewportHeight:innerHeight,
      documentWidth:document.documentElement.scrollWidth};
  });
  ensure(Math.abs(managerBounds.x) <= 1 && Math.abs(managerBounds.y) <= 1 &&
    Math.abs(managerBounds.width-managerBounds.viewportWidth) <= 1 &&
    Math.abs(managerBounds.height-managerBounds.viewportHeight) <= 1 &&
    managerBounds.documentWidth <= managerBounds.viewportWidth+1,
    'Open Extensions must show a full-screen browser-style manager, not a small modal');
  ensure(await permissionPage.locator('#androidCosExtensionSearch').isVisible() &&
    await permissionPage.locator('#androidCosExtensionRefresh').isVisible() &&
    await permissionPage.locator('#androidCosExtensionCount').innerText() === '0 installed',
    'The full-screen page must have search, refresh and backend-sourced extension count');
  ensure(await permissionPage.locator('#androidCosExtensionManager').isVisible() &&
    (await permissionPage.locator('#androidCosExtensionList').innerText()).includes('No extensions installed'),
    'Open Extensions must open the real extension page without installing anything');
  await permissionPage.screenshot({path:path.join(screenshotFolder,'native-android-extensions-page-empty.png')});
  await permissionPage.locator('#androidCosExtensionRefresh').click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidCosExtensionCount').textContent === '0 installed');
  await permissionPage.locator('#androidCosExtensionDone').click();
  ensure(JSON.stringify(await permissionPage.evaluate(() => window.__permissionGeckoCalls)).includes('list') &&
    !JSON.stringify(await permissionPage.evaluate(() => window.__permissionGeckoCalls)).includes('install'),
    'Read-only native extension list must not install without a click');
  await permissionPage.locator('#androidCosExtensionInstall').click();
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="extension"] small')?.textContent === 'Verified');
  ensure(await permissionPage.locator('#androidCosExtensionInstall').isDisabled() &&
    (await permissionPage.locator('#androidCosExtensionStatus').innerText()).includes('1 installed') &&
    JSON.stringify(await permissionPage.evaluate(() => window.__permissionGeckoCalls)).includes('install'),
    'Install Exten must call native install once and only then report Verified');
  await permissionPage.locator('#androidCosExtensionOpen').click();
  ensure(await permissionPage.locator('#androidCosExtensionList .android-cos-extension-card').count() === 1,
    'Open Extensions must display the GeckoView extension card reported by native backend');
  await permissionPage.locator('#androidCosExtensionSearch').fill('does-not-exist');
  ensure(await permissionPage.locator('#androidCosExtensionList .android-cos-extension-card').count() === 0 &&
    (await permissionPage.locator('#androidCosExtensionList').innerText()).includes('No matching extensions') &&
    await permissionPage.locator('#androidCosExtensionCount').innerText() === '0 of 1',
    'Search must filter the native installed-extension list');
  await permissionPage.locator('#androidCosExtensionSearch').fill('Mcp Agent Ai');
  ensure(await permissionPage.locator('#androidCosExtensionList .android-cos-extension-card').count() === 1,
    'Searching by extension name must restore matching native cards');
  await permissionPage.locator('#androidCosExtensionList').getByRole('button',{name:'Details'}).click();
  ensure((await permissionPage.locator('#androidCosExtensionList').innerText()).includes('Permissions: None') &&
    (await permissionPage.locator('#androidCosExtensionList').innerText()).includes('Website access: None'),
    'Details must show available native extension metadata without inventing permissions');
  await permissionPage.screenshot({path:path.join(screenshotFolder,'native-android-extensions-page-installed.png')});
  await permissionPage.locator('#androidCosExtensionList').getByRole('button',{name:'Disable'}).click();
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="extension"] small')?.textContent === 'Not needed');
  await permissionPage.locator('#androidCosExtensionList').getByRole('button',{name:'Enable'}).click();
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="extension"] small')?.textContent === 'Verified');
  await permissionPage.locator('#androidCosExtensionList').getByRole('button',{name:'Remove'}).click();
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="extension"] small')?.textContent === 'Not needed');
  await permissionPage.locator('#androidCosExtensionDone').click();
  // The user previously inspected another step; opening Setup again must
  // discard that saved tab choice and pick the first genuinely unresolved
  // stage after refreshing native checks for ALL ten stages, not one at a time.
  await permissionPage.locator('[data-cos-setup-step="ready"]').click();
  const setupReadMark = await permissionPage.evaluate(() => window.__setupNativeStatusReads.length);
  await permissionPage.locator('#androidSettingsBack').click();
  await permissionPage.locator('[data-android-settings-category="setup"]').click();
  const requiredStatusReads = ['android_permissions_status','android_optional_permissions_status',
    'accessibility_status','custom_gecko_list_extensions','android_main_storage_status',
    'android_mcp_permissions_status','android_tunnel_saved_id_status',
    'android_tunnel_credentials_status','android_openai_tunnel_status','android_chatgpt_oauth_status'];
  await permissionPage.waitForFunction(({mark,commands}) => {
    const seen = new Set(window.__setupNativeStatusReads.slice(mark));
    return commands.every(command => seen.has(command)) &&
      document.querySelector('[data-cos-setup-step="browser"]')?.getAttribute('aria-current') === 'step';
  }, {mark:setupReadMark,commands:requiredStatusReads}, {timeout:15000});
  ensure(await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Verified' &&
    await permissionPage.locator('[data-cos-setup-step="extension"] small').innerText() === 'Not needed' &&
    await permissionPage.locator('[data-cos-setup-step="browser"] small').innerText() !== 'Verified' &&
    await permissionPage.locator('[data-cos-setup-step="ready"]').getAttribute('aria-current') !== 'step' &&
    await setupProgressMatchesTiles(),
    'Opening Setup must skip previously completed/Not needed steps and select unresolved Step3 despite previously viewing Step10');
  ensure(await permissionPage.locator('[data-cos-setup-step="extension"] small').innerText() === 'Not needed' &&
    await permissionPage.locator('#androidCosBrowserChoices').isVisible() &&
    await permissionPage.locator('#storageAccessSection').isHidden(),
    'Browser login does not require any GeckoView extension or Chrome pairing');
  ensure(await permissionPage.locator('#androidCosBrowserChoices').isVisible() &&
    await permissionPage.locator('#androidCosSetupStepTitle').isHidden() &&
    await permissionPage.locator('#androidCosAdvancedFooter').count() === 0 &&
    await permissionPage.locator('#androidCosBrowserChoices button').count() === 2 &&
    await permissionPage.locator('#androidCosBrowserOpen strong').innerText() === 'Sign in to ChatGPT' &&
    await permissionPage.locator('#androidCosBrowserBring strong').innerText() === 'Bring Session' &&
    await permissionPage.locator('#androidCosChooseGoogle,#androidCosChooseBuiltIn,#androidCosBrowserCheck,#androidCosBrowserShow,#androidCosBrowserStop').count() === 0 &&
    await permissionPage.locator('#storageAccessSection').isHidden(),
    'Step 3 must expose only two full-text actions without chooser, Check, Show or Stop');
  await permissionPage.screenshot({path:path.join(screenshotFolder,'native-android-cos-browser-choice.png'),fullPage:true});
  await permissionPage.waitForFunction(() => !document.getElementById('androidCosBrowserOpen').disabled);
  ensure(await permissionPage.locator('#androidCosBrowserBring').isDisabled() &&
    await permissionPage.locator('[data-cos-setup-step="browser"] small').innerText() === 'Pending',
    'No successful account check or Bring Session can be inferred from the setup page');
  // Native OAuth status is the only authority for Step3. A failed read,
  // forged token field or incomplete plan consent must fail closed.
  await permissionPage.evaluate(() => {
    window.__permissionOAuthStatusFailure = true;
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.waitForFunction(() =>
    /unable|unavailable|could not/i.test(document.getElementById('androidCosBrowserSignInStatus')?.textContent || ''),
    null, {timeout:8000});
  ensure(await permissionPage.locator('#androidCosBrowserBring').isDisabled() &&
    await permissionPage.locator('[data-cos-setup-step="browser"] small').innerText() !== 'Verified',
    'Native OAuth status failure must be visible without claiming a ChatGPT account');
  await permissionPage.evaluate(() => {
    window.__permissionOAuthStatusFailure = false;
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.waitForFunction(() =>
    !document.getElementById('androidCosBrowserOpen')?.disabled,
    null, {timeout:8000});
  await permissionPage.evaluate(() => { window.__permissionOAuthStartFailure = true; });
  await permissionPage.locator('#androidCosBrowserOpen').click();
  await permissionPage.waitForFunction(() =>
    window.__permissionOAuthCalls.includes('start'),
    null, {timeout:8000});
  ensure(await permissionPage.locator('#androidCosBrowserBring').isDisabled() &&
    await permissionPage.locator('[data-cos-setup-step="browser"] small').innerText() !== 'Verified',
    'Failed Chrome OAuth launch cannot count as app authorization');
  await permissionPage.evaluate(() => { window.__permissionOAuthStartFailure = false; });
  await permissionPage.locator('#androidCosBrowserOpen').click();
  await permissionPage.waitForFunction(() =>
    window.__permissionOAuth.state === 'authorizing' &&
    (document.getElementById('androidCosBrowserSignInStatus')?.textContent || '').includes('Waiting'));
  ensure(await permissionPage.locator('#androidCosBrowserBring').isDisabled() &&
    await permissionPage.locator('[data-cos-setup-step="browser"] small').innerText() !== 'Verified' &&
    !(await permissionPage.evaluate(() => window.__permissionChatgptCalls)).includes('open') &&
    await permissionPage.locator('[data-cos-setup-step="connect"] small').innerText() !== 'Verified',
    'Chrome login pending user approval must not open owned WebView or complete Step3/8');
  // Approving identity without both ChatGPT plan scopes cannot enable Bring.
  await permissionPage.evaluate(() => {
    window.__permissionOAuthGrant = 'identity_only';
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.waitForFunction(() =>
    window.__permissionOAuth.state === 'verified' &&
    document.getElementById('androidCosBrowserBring')?.disabled === true);
  ensure(await permissionPage.locator('[data-cos-setup-step="browser"] small').innerText() !== 'Verified',
    'Account identity without plan permission must fail closed even after native callback');
  // A new attempt gets fresh native PKCE and waits for explicit account approval.
  await permissionPage.locator('#androidCosBrowserOpen').click();
  await permissionPage.waitForFunction(() => window.__permissionOAuth.state === 'authorizing');
  await permissionPage.evaluate(() => {
    window.__permissionOAuthGrant = 'approved';
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidCosBrowserBring')?.disabled === false,
    null, {timeout:12000});
  ensure(await permissionPage.locator('[data-cos-setup-step="browser"] small').innerText() !== 'Verified' &&
    await permissionPage.locator('#provider').inputValue() === 'chatgpt-browser' &&
    await permissionPage.locator('[data-cos-setup-step="connect"] small').innerText() !== 'Verified',
    'Approved native OAuth grant alone must still await explicit Bring and independent Step8');
  await permissionPage.screenshot({path:path.join(screenshotFolder,'native-android-chatgpt-step3-signed-in.png'),fullPage:true});
  await permissionPage.evaluate(() => {
    window.__permissionOAuthStatusForgery = true;
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidCosBrowserBring')?.disabled === true);
  ensure(await permissionPage.locator('[data-cos-setup-step="browser"] small').innerText() !== 'Verified',
    'OAuth receipts leaking accessToken or extra fields must be rejected');
  await permissionPage.evaluate(() => {
    window.__permissionOAuthStatusForgery = false;
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidCosBrowserBring')?.disabled === false);
  await permissionPage.evaluate(() => {window.__permissionOAuthBringRejected = true;});
  await permissionPage.locator('#androidCosBrowserBring').click();
  ensure(await permissionPage.locator('[data-cos-setup-step="browser"] small').innerText() !== 'Verified' &&
    await permissionPage.locator('[data-cos-setup-step="browser"]').getAttribute('aria-current') === 'step',
    'Rejected native Bring must leave Step3 incomplete');
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidCosBrowserBring')?.disabled === false,
    null,{timeout:12000});
  await permissionPage.evaluate(() => {window.__permissionOAuthBringRejected = false;});
  await permissionPage.locator('#androidCosBrowserBring').click();
  try {
    await permissionPage.waitForFunction(() => {
      const stages=[...document.querySelectorAll('[data-cos-setup-step]')];
      const firstUnresolved=stages.slice(0,-1).find(stage=>!['Verified','Not needed']
        .includes(stage.querySelector('small')?.textContent || ''))?.dataset.cosSetupStep || 'ready';
      return document.querySelector('[data-cos-setup-step="browser"] small')?.textContent === 'Verified' &&
        stages.find(stage=>stage.getAttribute('aria-current')==='step')?.dataset.cosSetupStep === firstUnresolved;
    },
    null,{timeout:12000});
  } catch {
    const receipt=await permissionPage.evaluate(() => ({
      selected:document.querySelector('[data-cos-setup-step][aria-current="step"]')?.dataset.cosSetupStep,
      browserBadge:document.querySelector('[data-cos-setup-step="browser"] small')?.textContent,
      nativeOAuth:structuredClone(window.__permissionOAuth),
      nativeCalls:[...window.__permissionOAuthCalls],
      browserBringDisabled:document.getElementById('androidCosBrowserBring')?.disabled,
      status:document.getElementById('androidCosBrowserSignInStatus')?.textContent
    }));
    throw Error('Native OAuth Bring did not link/advance after rejected retry: '+JSON.stringify(receipt));
  }
  ensure((await permissionPage.evaluate(() => window.__permissionOAuthCalls)).includes('bring') &&
    !(await permissionPage.evaluate(() => window.__permissionChatgptCalls)).includes('bring_session') &&
    await permissionPage.locator('[data-cos-setup-step="connect"] small').innerText() !== 'Verified',
    'Approved native OAuth Bring follows the first unresolved Setup stage; website cookies and tunnel remain separate');
  await permissionPage.screenshot({path:path.join(screenshotFolder,'native-android-chatgpt-step3-linked.png'),fullPage:true});
  // Later revocation of the token grant must revoke Step3, never promote Step8.
  await permissionPage.evaluate(() => {
    window.__permissionOAuthGrant = 'revoked';
    window.dispatchEvent(new Event('focus'));
  });
  await permissionPage.locator('[data-cos-setup-step="browser"]').click();
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="browser"] small')?.textContent !== 'Verified');
  ensure(await permissionPage.locator('#androidCosBrowserBring').isDisabled() &&
    await permissionPage.locator('[data-cos-setup-step="connect"] small').innerText() !== 'Verified',
    'Revoked native OpenAI grant must invalidate Step3 and never satisfy Step8');
  ensure(await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Verified' &&
    await permissionPage.locator('#accessibilityManage').isDisabled() &&
    await permissionPage.locator('#accessibilityPermissionState').innerText() === 'Connected',
    'Accessibility permission and service remain verified after browser selection');
  for (const step of ['folder','tools','tunnel']) {
    await permissionPage.locator(`[data-cos-setup-step="${step}"]`).click();
    ensure(await permissionPage.locator('#storageAccessSection').isHidden() &&
      await permissionPage.locator('#storageAccessSection').getAttribute('aria-hidden') === 'true' &&
      await permissionPage.locator('#androidCosSetupCurrent').isVisible(),
      `App permissions leaked into ${step} step instead of being exclusive to Permissions`);
    if (step === 'folder') {
      await permissionPage.waitForFunction(() =>
        document.getElementById('androidCosMainState')?.textContent === 'Ready' &&
        document.getElementById('androidCosSelectedPath')?.textContent === '/storage/emulated/0');
      ensure(await permissionPage.locator('[data-cos-setup-step="folder"] small').innerText() === 'Verified' &&
        await permissionPage.locator('#androidCosMainPath').innerText() === '/storage/emulated/0' &&
        await permissionPage.locator('#androidCosSetupStepTitle').isHidden() &&
        await permissionPage.locator('#androidCosFolderBrowse').isEnabled() &&
        await permissionPage.locator('#androidCosFolderReset').isDisabled() &&
        await permissionPage.locator('#androidCosFolderNext').isEnabled() &&
        await permissionPage.locator('#androidCosFolderFiles,#androidCosFolderCheck').count() === 0,
        'Step 4 auto-verifies readable storage with only Select folder and a disabled Reset at root');
      await permissionPage.locator('#androidCosFolderBrowse').click();
      await permissionPage.waitForFunction(() =>
        document.getElementById('androidCosFolderEntries')?.textContent.includes('Download'));
      ensure(await permissionPage.locator('#androidCosFolderEntries button').count() === 3 &&
        await permissionPage.locator('#androidCosFolderEntries').getByRole('button',{name:/Android/}).isDisabled() &&
        await permissionPage.locator('#androidCosFolderSet').isDisabled(),
        'Native folder browser must list only bounded folders and keep restricted paths disabled');
      await permissionPage.locator('#androidCosFolderEntries').getByRole('button',{name:/Download/}).click();
      await permissionPage.waitForFunction(() =>
        document.getElementById('androidCosFolderCurrentPath')?.textContent.includes('Download') &&
        document.getElementById('androidCosFolderEntries')?.textContent.includes('Receipts'));
      ensure(await permissionPage.locator('#androidCosFolderCandidate').innerText() === '/storage/emulated/0/Download' &&
        await permissionPage.locator('#androidCosFolderSet').isEnabled(),
        'Selecting a verified child must offer Set folder for that exact path');
      await permissionPage.locator('#androidCosFolderSet').click();
      await permissionPage.waitForFunction(() =>
        document.getElementById('androidCosSelectedPath')?.textContent === '/storage/emulated/0/Download' &&
        document.getElementById('androidCosFolderExplorer')?.classList.contains('hidden') === true);
      ensure(await permissionPage.locator('#androidCosFolderReset').isEnabled() &&
        (await permissionPage.evaluate(() => window.__mainStorageSelectionCalls)).some(entry =>
          entry.action === 'select' && entry.path.join('/') === 'Download'),
        'Set folder must receive native persistence receipt and update current selection');
      await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
      await permissionPage.waitForFunction(() =>
        document.getElementById('androidCosSelectedPath')?.textContent === '/storage/emulated/0/Download');
      await permissionPage.locator('#androidCosFolderBrowse').click();
      await permissionPage.waitForFunction(() =>
        document.getElementById('androidCosFolderEntries')?.textContent.includes('Download'));
      await permissionPage.locator('#androidCosFolderEntries').getByRole('button',{name:/Download/}).click();
      await permissionPage.waitForFunction(() =>
        document.getElementById('androidCosFolderCurrentPath')?.textContent.includes('Download'));
      await permissionPage.locator('#androidCosFolderUp').click();
      await permissionPage.waitForFunction(() =>
        document.getElementById('androidCosFolderCurrentPath')?.textContent === 'Internal storage');
      ensure((await permissionPage.evaluate(() => window.__mainStorageCalls)).some(path =>
        Array.isArray(path) && path.join('/') === 'Download'),
        'Browsing a phone folder must invoke the native confined read-only listing');
      await permissionPage.locator('#androidCosFolderReset').click();
      await permissionPage.waitForFunction(() =>
        document.getElementById('androidCosSelectedPath')?.textContent === '/storage/emulated/0' &&
        document.getElementById('androidCosFolderExplorer')?.classList.contains('hidden') === true);
      ensure(await permissionPage.locator('[data-cos-setup-step="folder"] small').innerText() === 'Verified' &&
        await permissionPage.locator('#androidCosFolderReset').isDisabled() &&
        (await permissionPage.evaluate(() => window.__mainStorageSelectionCalls)).some(entry =>
          entry.action === 'reset' && entry.path.length === 0),
        'Reset must persist default /storage/emulated/0, remain verified and gray the Reset button');
      await permissionPage.screenshot({path:path.join(screenshotFolder,'native-android-folder-auto-390.png'),fullPage:true});
      await permissionPage.locator('#androidCosFolderNext').click();
      ensure(await permissionPage.locator('[data-cos-setup-step="tools"]').getAttribute('aria-current') === 'step',
        'Continue from verified main storage must advance to Step 5 Access without requiring SAF selection');
    }
    if (step === 'tools') {
      // Requested permissions resemble the COS categories, but must NOT
      // silently unlock unsupported remote file, browser or shell routes.
      await permissionPage.waitForFunction(() =>
        !document.querySelector('input[name="androidMcpAccessPreset"]').disabled);
      ensure(await permissionPage.locator('#androidMcpAccessPresets').isVisible() &&
        await permissionPage.locator('input[name="androidMcpAccessPreset"]').count() === 3 &&
        await permissionPage.locator('input[name="androidMcpAccessPreset"]:checked').count() === 0 &&
        await permissionPage.locator('#androidMcpAccessNext').count() === 0 &&
        await permissionPage.locator('[data-cos-setup-step="tools"] small').innerText() === 'Pending' &&
        await permissionPage.locator('#androidMcpAllowLook').isEnabled() &&
        await permissionPage.locator('#androidMcpAccessStatus').getAttribute('data-verified') === 'false' &&
        await permissionPage.evaluate(() => window.__mcpAccessCalls.length) === 0,
        'Step5 opens with all tools denied and no falsely selected Read only preset');
      await permissionPage.evaluate(() => { window.__mcpAccessRejectNext = true; });
      // A rejected native write immediately restores the unchecked radio;
      // click() (not Playwright check()) intentionally tests that rollback.
      await permissionPage.locator('input[name="androidMcpAccessPreset"][value="full"]').click();
      await permissionPage.waitForFunction(() =>
        document.querySelector('#androidMcpAccessStatus')?.textContent?.includes('not confirmed') &&
        !document.querySelector('input[name="androidMcpAccessPreset"][value="full"]').disabled);
      ensure(await permissionPage.locator('[data-cos-setup-step="tools"]').getAttribute('aria-current') === 'step' &&
        await permissionPage.locator('[data-cos-setup-step="tools"] small').innerText() === 'Pending' &&
        await permissionPage.locator('input[name="androidMcpAccessPreset"]:checked').count() === 0 &&
        await permissionPage.evaluate(() => window.__mcpAccessCalls.length) === 0,
        'Failed native access save must not mark Step5 Verified, retain a fake preset or auto-open Tunnel');
      await permissionPage.locator('input[name="androidMcpAccessPreset"][value="read_only"]').check();
      await permissionPage.waitForFunction(() =>
        window.__mcpAccessPolicy.preset === 'read_only' &&
        window.__mcpAccessPolicy.requested.browseFolders === true &&
        document.querySelector('[data-cos-setup-step="tunnel"]')?.getAttribute('aria-current') === 'step');
      ensure(await permissionPage.locator('[data-cos-setup-step="tools"] small').innerText() === 'Verified' &&
        await permissionPage.evaluate(() => window.__mcpAccessCalls.length) === 1,
        'First Read only tap must save 4 requested read permissions, mark Step5 Verified and auto-open Step6');
      await permissionPage.locator('[data-cos-setup-step="tools"]').click();
      ensure(await permissionPage.locator('input[name="androidMcpAccessPreset"][value="read_only"]').isChecked(),
        'Returning to Step5 restores the selected and saved Read only preset');
      await permissionPage.locator('input[name="androidMcpAccessPreset"][value="full"]').check();
      await permissionPage.waitForFunction(() =>
        window.__mcpAccessCalls.some(call => call.preset === 'full') &&
        document.querySelector('[data-cos-setup-step="tunnel"]')?.getAttribute('aria-current') === 'step');
      ensure(await permissionPage.locator('[data-cos-setup-step="tools"] small').innerText() === 'Verified' &&
        await permissionPage.locator('#androidMcpAccessStatus').getAttribute('data-verified') === 'false' &&
        (await permissionPage.evaluate(() =>
          Object.values(window.__mcpAccessPolicy.effective).every(value => value === false))) &&
        await permissionPage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Pending',
        'Selecting Full saves chosen policy and advances even if native tools are unavailable; the real permissions remain denied');
      await permissionPage.locator('[data-cos-setup-step="tools"]').click();
      ensure(await permissionPage.locator('input[name="androidMcpAccessPreset"][value="full"]').isChecked(),
        'Full preset must remain selected after returning from Step6');
      await permissionPage.locator('input[name="androidMcpAccessPreset"][value="files_terminal"]').check();
      await permissionPage.waitForFunction(() =>
        window.__mcpAccessPolicy.preset === 'files_terminal' &&
        document.querySelector('[data-cos-setup-step="tunnel"]')?.getAttribute('aria-current') === 'step');
      ensure(await permissionPage.locator('[data-cos-setup-step="tools"] small').innerText() === 'Verified' &&
        (await permissionPage.evaluate(() => window.__mcpAccessPolicy.effective.terminal)) === false,
        'Files and terminal preset must save, verify the selection and auto-advance without granting fake shell access');
      await permissionPage.locator('[data-cos-setup-step="tools"]').click();
      await permissionPage.locator('.android-mcp-fine-tune > summary').click();
      const fineTuneGroups = await permissionPage.locator(
        '#androidCosAccessPanel .android-mcp-permission-group > summary > span').allTextContents();
      ensure(JSON.stringify(fineTuneGroups.map(label=>label.trim())) ===
        JSON.stringify(['Look at files','Change files','Automation modes','Run programs','Sub-agents']) &&
        await permissionPage.locator('#androidMcpAllowLook,#androidMcpAllowChange,#androidMcpAllowTerminal').count() === 3 &&
        await permissionPage.locator('#androidCosAccessPanel #androidAutomationModesGroup').count() === 1 &&
        await permissionPage.locator('#androidAutomationBrowserMode,#androidAutomationAppMode').count() === 2 &&
        await permissionPage.locator('[data-setting-category="agents"] #androidAutomationBrowserMode, [data-setting-category="agents"] #androidAutomationAppMode').count() === 0 &&
        await permissionPage.locator('#androidAutomationBrowserUrl,#androidAutomationBrowserOpen,#androidAutomationBrowserStop').count() === 0,
        'Step5 Fine-tune must present all five COS groups and one pair of mode dropdowns, with no URL/Open/Stop controls');
      const subagentsGroup = permissionPage.locator('.android-mcp-subagents-group');
      await subagentsGroup.locator(':scope > summary').click();
      ensure(/informational|no separate.*permission/i.test(await subagentsGroup.innerText()) &&
        await subagentsGroup.locator('input[type="checkbox"]').count() === 0,
        'Sub-agents group is informational, not a fabricated native permission');
      await permissionPage.locator('#androidAutomationModesGroup > summary').click();
      await permissionPage.locator('#androidAutomationBrowserBadge').getByText('Available').waitFor();
      ensure(await permissionPage.locator('#androidAutomationBrowserMode').inputValue() === 'headless' &&
        await permissionPage.locator('#androidAutomationAppMode').inputValue() === 'realtime' &&
        (await permissionPage.locator('#androidAutomationAppMode option[value="realtime"]').textContent()).trim() === 'RealTime Screen' &&
        /unavailable/i.test(await permissionPage.locator('#androidAutomationAppStatus').innerText()),
        'Fine-tune Automation modes must show persisted choices and honest native unsupported status');
      const mcpCallsBeforeModes = await permissionPage.evaluate(() => window.__mcpAccessCalls.length);
      for (const mode of ['right_sidebar','open_browser_app','headless']) {
        await permissionPage.locator('#androidAutomationBrowserMode').selectOption(mode);
        await permissionPage.waitForFunction(selected =>
          window.__automationModesSaved.browserMode === selected,mode);
      }
      await permissionPage.locator('#androidAutomationAppMode').selectOption('screenshot_click');
      await permissionPage.waitForFunction(() => window.__automationModesSaved.androidMode === 'screenshot_click');
      ensure(await permissionPage.locator('#androidAutomationAppMode').inputValue() === 'screenshot_click' &&
        await permissionPage.evaluate(() => window.__automationLaunchCalls.length) === 0 &&
        await permissionPage.evaluate(() => window.__automationPreferenceCalls.length) === 4 &&
        await permissionPage.evaluate(() => window.__mcpAccessCalls.length) === mcpCallsBeforeModes,
        'Browser/App mode dropdowns save native preferences without URL, browser launch, or extra file/terminal grants');
      await permissionPage.evaluate(() => {window.__automationFailNextSave = true;});
      await permissionPage.locator('#androidAutomationAppMode').selectOption('realtime');
      await permissionPage.locator('#androidAutomationAppStatus').waitFor({state:'visible'});
      await permissionPage.waitForFunction(() =>
        document.getElementById('androidAutomationAppMode')?.value === 'screenshot_click');
      ensure(await permissionPage.evaluate(() => window.__automationModesSaved.androidMode === 'screenshot_click') &&
        await permissionPage.evaluate(() => window.__automationLaunchCalls.length === 0),
        'Rejected Android app mode save must restore previous preference and launch nothing');
      await permissionPage.locator('#androidMcpAllowLook').uncheck();
      await permissionPage.waitForFunction(() =>
        window.__mcpAccessPolicy.preset === 'custom' &&
        window.__mcpAccessPolicy.requested.browseFolders === false);
      ensure(await permissionPage.locator('input[name="androidMcpAccessPreset"]:checked').count() === 0 &&
        await permissionPage.locator('#androidMcpAllowChange').isChecked() &&
        await permissionPage.locator('#androidMcpAllowLook').isChecked() === false &&
        await permissionPage.locator('#androidMcpAccessStatus').getAttribute('data-verified') === 'false',
        'Fine tuning one group derives a custom policy without changing other requested categories');
      await permissionPage.locator('input[name="androidMcpAccessPreset"][value="read_only"]').check();
      await permissionPage.waitForFunction(() => window.__mcpAccessPolicy.preset === 'read_only' &&
        window.__mcpAccessPolicy.requested.browseFolders === true &&
        window.__mcpAccessPolicy.requested.terminal === false &&
        document.querySelector('[data-cos-setup-step="tunnel"]')?.getAttribute('aria-current') === 'step');
      await permissionPage.locator('[data-cos-setup-step="tools"]').click();
      ensure(await permissionPage.locator('#androidMcpAllowLook').isChecked() &&
        !await permissionPage.locator('#androidMcpAllowTerminal').isChecked() &&
        (await permissionPage.evaluate(() =>
          Object.values(window.__mcpAccessPolicy.available).every(value => value === false))),
        'Read-only preset must request inspection only; unsupported remote tools remain unavailable');
      await permissionPage.locator('input[data-mcp-capability="readFiles"]').uncheck();
      await permissionPage.waitForFunction(() =>
        window.__mcpAccessPolicy.preset === 'custom' &&
        window.__mcpAccessPolicy.requested.readFiles === false);
      ensure(await permissionPage.locator('input[data-mcp-capability="browseFolders"]').isChecked() &&
        !await permissionPage.locator('input[data-mcp-capability="readFiles"]').isChecked() &&
        await permissionPage.locator('#androidMcpAllowLook').evaluate(input => input.indeterminate) &&
        await permissionPage.locator('#androidMcpAccessStatus').getAttribute('data-verified') === 'false',
        'Each fine-tune file permission must be independently editable and group indicator becomes mixed');
      await permissionPage.locator('input[name="androidMcpAccessPreset"][value="read_only"]').check();
      await permissionPage.waitForFunction(() =>
        window.__mcpAccessPolicy.preset === 'read_only' &&
        window.__mcpAccessPolicy.requested.readFiles === true &&
        document.querySelector('[data-cos-setup-step="tunnel"]')?.getAttribute('aria-current') === 'step');
      await permissionPage.locator('[data-cos-setup-step="tools"]').click();
      // Real backend supports these four read-only routes only when the
      // current Android-approved folder can be resolved. Prove that Step 5
      // status is computed from live availability, not the preset label.
      await permissionPage.evaluate(() => {
        for (const key of ['browseFolders','searchFiles','readFiles','fileMetadata']) {
          window.__mcpAccessPolicy.available[key] = true;
          window.__mcpAccessPolicy.effective[key] =
            window.__mcpAccessPolicy.requested[key];
        }
        window.dispatchEvent(new Event('focus'));
      });
      await permissionPage.waitForFunction(() =>
        document.querySelector('[data-cos-setup-step="tools"] small')?.textContent === 'Verified');
      ensure(await permissionPage.locator('#androidMcpAccessStatus').getAttribute('data-verified') === 'true' &&
        await permissionPage.locator('[data-cos-setup-step="tunnel"] small').innerText() === 'Pending',
        'Native verified read-only file tools may verify Access without falsely verifying Tunnel');
      await permissionPage.evaluate(() => {
        for (const key of ['browseFolders','searchFiles','readFiles','fileMetadata']) {
          window.__mcpAccessPolicy.available[key] = false;
          window.__mcpAccessPolicy.effective[key] = false;
        }
        window.dispatchEvent(new Event('focus'));
      });
      await permissionPage.waitForFunction(() =>
        document.querySelector('#androidMcpAccessStatus')?.getAttribute('data-verified') === 'false');
      ensure(await permissionPage.locator('[data-cos-setup-step="tools"] small').innerText() === 'Verified',
        'An explicitly saved access choice stays complete even when actual tool availability changes');
      await permissionPage.screenshot({path:path.join(screenshotFolder,'native-android-cos-tool-permissions-390.png'),fullPage:true});
      ensure(await permissionPage.locator('#androidCosTunnelMethod').isHidden() &&
        await permissionPage.locator('#androidOpenAiTunnelCredentials').isHidden() &&
        await permissionPage.locator('#androidCosAdvancedFooter').count() === 0,
        'Step5 Access must not expose OpenAI Tunnel credentials, controls or Advanced.');
      ensure(await permissionPage.locator('#androidMcpAccessNext').count() === 0,
        'Step5 has no Continue to Tunnel button: a successful preset choice alone advances');
    }
    if (step === 'tunnel') {
      ensure(await permissionPage.locator('#androidCosAccessPanel').isHidden() &&
        await permissionPage.locator('#androidMcpAccessPresets').isHidden() &&
        await permissionPage.locator('#androidCosChatgptPanel').isHidden() &&
        await permissionPage.locator('#androidTunnelLegacyCompat').isHidden() &&
        await permissionPage.locator('#androidCosAdvancedFooter').count() === 0 &&
        await permissionPage.locator('#androidTunnelSaveId').isVisible(),
        'Step6 shows Core Tunnel ID + Save only; local server, legacy runtime and desktop controls stay hidden');
      await permissionPage.screenshot({path:path.join(screenshotFolder,'native-android-core-tunnel-390.png'),fullPage:true});
    }
  }
  // Both views borrow the *same two DOM panels*. Test the shared persisted
  // native receipts through genuine buttons, not duplicated local state.
  const switchAndroidSettingsCategory = async category => {
    await permissionPage.locator('#androidSettingsBack').click();
    await permissionPage.locator(`[data-android-settings-category="${category}"]`).click();
  };
  await switchAndroidSettingsCategory('workspace');
  await permissionPage.waitForFunction(() =>
    document.querySelector('#androidWorkspaceFoldersSlot #androidCosFolderPanel') &&
    document.querySelector('#androidWorkspacePermissionsSlot #androidCosAccessPanel') &&
    !document.getElementById('androidWorkspaceRunChecks').disabled);
  const sharedWorkspace = await permissionPage.evaluate(() => ({
    oneFolder:document.querySelectorAll('#androidCosFolderPanel').length === 1,
    oneAccess:document.querySelectorAll('#androidCosAccessPanel').length === 1,
    onePolicy:document.querySelectorAll('#androidMcpAccessPresets').length === 1,
    folder:document.getElementById('androidCosSelectedPath').textContent,
    preset:document.querySelector('input[name="androidMcpAccessPreset"]:checked')?.value,
    readOnly:document.getElementById('androidWorkspaceReadOnly').getAttribute('aria-pressed'),
    labels:[...document.querySelectorAll('#androidWorkspaceDashboard .android-workspace-card h2')]
      .map(node => node.textContent)
  }));
  ensure(sharedWorkspace.oneFolder && sharedWorkspace.oneAccess && sharedWorkspace.onePolicy &&
    sharedWorkspace.folder === '/storage/emulated/0' && sharedWorkspace.preset === 'read_only' &&
    sharedWorkspace.readOnly === 'true' &&
    ['Permissions','Activity','Folders','Health'].every(title => sharedWorkspace.labels.includes(title)),
    `Workspace must share the current native Folder and saved Step5 policy: ${JSON.stringify(sharedWorkspace)}`);
  ensure(await permissionPage.locator('#projectStorageSettings').isHidden() &&
    await permissionPage.locator('#androidSettingsOpenFiles').isHidden() &&
    await permissionPage.locator('#androidWorkspaceReadOnly').isDisabled() &&
    await permissionPage.locator('#androidWorkspaceCopyActivity').isDisabled(),
    'Workspace must hide legacy migration/Files controls and keep Activity Copy disabled without native events');
  // Workspace -> Setup: select an explicitly readable child in the moved panel.
  await permissionPage.locator('#androidWorkspaceChooseFolder').click();
  await permissionPage.waitForFunction(() =>
    document.querySelector('#androidCosFolderEntries')?.textContent.includes('DCIM'));
  await permissionPage.locator('#androidCosFolderEntries').getByRole('button',{name:/DCIM/}).click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidCosFolderCandidate')?.textContent === '/storage/emulated/0/DCIM');
  await permissionPage.locator('#androidCosFolderSet').click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidCosSelectedPath')?.textContent === '/storage/emulated/0/DCIM');
  ensure((await permissionPage.evaluate(() => window.__mainStorageSelectionCalls)).some(record =>
    record.action === 'select' && record.path.join('/') === 'DCIM'),
    'Workspace Choose and Set must persist the native selected folder once');
  await switchAndroidSettingsCategory('setup');
  await permissionPage.locator('[data-cos-setup-step="folder"]').click();
  ensure(await permissionPage.locator('#androidCosSetupCurrent #androidCosFolderPanel').isVisible() &&
    await permissionPage.locator('#androidCosSelectedPath').innerText() === '/storage/emulated/0/DCIM' &&
    await permissionPage.locator('#androidCosFolderReset').isEnabled(),
    'Step4 must restore the exact Workspace-selected native folder after relocation');
  // Setup -> Workspace: change the same selection from the other surface.
  await permissionPage.locator('#androidCosFolderBrowse').click();
  await permissionPage.waitForFunction(() =>
    document.querySelector('#androidCosFolderEntries')?.textContent.includes('Download'));
  await permissionPage.locator('#androidCosFolderEntries').getByRole('button',{name:/Download/}).click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidCosFolderCandidate')?.textContent === '/storage/emulated/0/Download');
  await permissionPage.locator('#androidCosFolderSet').click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidCosSelectedPath')?.textContent === '/storage/emulated/0/Download');
  await switchAndroidSettingsCategory('workspace');
  ensure(await permissionPage.locator('#androidWorkspaceFoldersSlot #androidCosSelectedPath').innerText() ===
    '/storage/emulated/0/Download' &&
    await permissionPage.locator('#androidCosFolderReset').isEnabled(),
    'Workspace must reflect Setup Step4 folder changes from the same native receipt');
  await permissionPage.locator('#androidCosFolderReset').click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidCosSelectedPath')?.textContent === '/storage/emulated/0');
  await switchAndroidSettingsCategory('setup');
  await permissionPage.locator('[data-cos-setup-step="folder"]').click();
  ensure(await permissionPage.locator('#androidCosSelectedPath').innerText() === '/storage/emulated/0' &&
    await permissionPage.locator('#androidCosFolderReset').isDisabled(),
    'Workspace Reset must restore Step4 to native shared storage root without a phantom selected child');
  // A saved policy is shared while controls move. Workspace can also change
  // the policy and Setup must show the persisted native response.
  await switchAndroidSettingsCategory('workspace');
  await permissionPage.locator('input[name="androidMcpAccessPreset"][value="full"]').check();
  await permissionPage.waitForFunction(() =>
    window.__mcpAccessPolicy.preset === 'full' &&
    document.querySelector('input[name="androidMcpAccessPreset"][value="full"]')?.checked);
  ensure(await permissionPage.locator('#androidWorkspaceReadOnly').isEnabled() &&
    await permissionPage.locator('#androidWorkspaceReadOnly').getAttribute('aria-pressed') === 'false' &&
    (await permissionPage.evaluate(() => Object.values(window.__mcpAccessPolicy.effective).every(v => v === false))),
    'Workspace Full selection saves requested policy but must not grant unavailable native tools');
  await switchAndroidSettingsCategory('setup');
  await permissionPage.locator('[data-cos-setup-step="tools"]').click();
  ensure(await permissionPage.locator('#androidCosSetupCurrent #androidCosAccessPanel').isVisible() &&
    await permissionPage.locator('input[name="androidMcpAccessPreset"][value="full"]').isChecked() &&
    await permissionPage.locator('[data-cos-setup-step="tools"] small').innerText() === 'Verified',
    'Step5 must show the policy last saved through Workspace, with no clone or fake access');
  await switchAndroidSettingsCategory('workspace');
  await permissionPage.locator('#androidWorkspaceReadOnly').click();
  await permissionPage.waitForFunction(() =>
    window.__mcpAccessPolicy.preset === 'read_only' &&
    document.getElementById('androidWorkspaceReadOnly')?.getAttribute('aria-pressed') === 'true');
  await switchAndroidSettingsCategory('setup');
  await permissionPage.locator('[data-cos-setup-step="tools"]').click();
  ensure(await permissionPage.locator('input[name="androidMcpAccessPreset"][value="read_only"]').isChecked() &&
    await permissionPage.evaluate(() => window.__mcpAccessPolicy.requested.terminal === false),
    'Workspace Read only shortcut must be reflected in Setup Step5 with its exact native policy');
  // The COS-style Health facts and Activity feed are native receipts. An
  // absent timestamp, poll count or uptime may never turn into a plausible
  // synthetic value or new activity row.
  await permissionPage.evaluate(() => {
    window.__permissionTunnel = {state:'disconnected',pollActive:false,
      toolCallsVerified:0,corePluginLinked:false};
  });
  await switchAndroidSettingsCategory('workspace');
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidWorkspaceTunnelStatus')?.textContent === 'not running' &&
    document.getElementById('androidWorkspacePluginStatus')?.textContent === 'not verified');
  const disconnectedFacts = await permissionPage.evaluate(() =>
    Object.fromEntries(['VerifiedLink','LastCall','Route','PollErrors','Uptime','LastTool',
      'TunnelClient','LocalServer','ToolCount'].map(id =>
      [id,document.getElementById('androidWorkspace'+id)?.textContent?.trim()])));
  ensure(['VerifiedLink','LastCall','PollErrors','Uptime','LastTool','TunnelClient',
      'LocalServer','ToolCount'].every(id => disconnectedFacts[id] === '—') &&
    await permissionPage.locator('#androidWorkspaceActivityFeed').getAttribute('role') === 'log' &&
    await permissionPage.locator('#androidWorkspaceActivityFeed .android-workspace-activity-row').count() === 0 &&
    await permissionPage.locator('#androidWorkspaceCopyActivity').isDisabled() &&
    await permissionPage.locator('#androidWorkspaceChecksResult').isHidden(),
    `Disconnected native status cannot invent timestamps, poll errors, uptime, events or available Copy: ${JSON.stringify(disconnectedFacts)}`);

  const workspaceReferenceMs = await permissionPage.evaluate(() => Date.now());
  const historicalHealth = {
    verifiedLinkAtMs:workspaceReferenceMs - 180000,
    lastChatgptCallAtMs:null,pollErrors:0,uptimeSeconds:125,
    route:'api.openai.com:443 · direct',tunnelClient:'Android native tunnel',
    localServer:'127.0.0.1:8765',probe:null,toolCount:12
  };
  await permissionPage.evaluate(health => {
    window.__permissionTunnel = {state:'polling',pollActive:true,
      toolCallsVerified:0,corePluginLinked:true,health,events:[]};
  },historicalHealth);
  await permissionPage.locator('#androidWorkspaceRunChecks').click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidWorkspaceTunnelStatus')?.textContent === 'ok · polling' &&
    document.getElementById('androidWorkspacePluginStatus')?.textContent === 'linked' &&
    document.getElementById('androidWorkspaceUptime')?.textContent === '2m 5s');
  const historicalFacts = await permissionPage.evaluate(() => ({
    link:document.getElementById('androidWorkspaceVerifiedLink').textContent,
    lastCall:document.getElementById('androidWorkspaceLastCall').textContent,
    lastTool:document.getElementById('androidWorkspaceLastTool').textContent,
    route:document.getElementById('androidWorkspaceRoute').textContent,
    errors:document.getElementById('androidWorkspacePollErrors').textContent,
    uptime:document.getElementById('androidWorkspaceUptime').textContent,
    client:document.getElementById('androidWorkspaceTunnelClient').textContent,
    server:document.getElementById('androidWorkspaceLocalServer').textContent,
    tools:document.getElementById('androidWorkspaceToolCount').textContent
  }));
  ensure(historicalFacts.link !== '—' && historicalFacts.lastCall === '—' &&
    historicalFacts.lastTool === '—' && historicalFacts.errors === '0' &&
    historicalFacts.uptime === '2m 5s' &&
    historicalFacts.route === historicalHealth.route &&
    historicalFacts.client === historicalHealth.tunnelClient &&
    historicalFacts.server === historicalHealth.localServer &&
    historicalFacts.tools === '12' &&
    await permissionPage.locator('#androidWorkspaceActivityFeed .android-workspace-activity-row').count() === 0 &&
    await permissionPage.locator('#androidWorkspaceCopyActivity').isDisabled(),
    `Historical Linked proof must show a true link age, not a made-up recent ChatGPT call: ${JSON.stringify(historicalFacts)}`);
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidWorkspaceChecksSummary')?.textContent === 'Native checks completed' &&
    document.querySelectorAll('#androidWorkspaceChecksList li').length === 4);
  ensure(await permissionPage.locator('#androidWorkspaceChecksResult').isVisible() &&
    await permissionPage.locator('#androidWorkspaceCloseChecks').getAttribute('aria-label') === 'Close check results',
    'Run checks must reveal four native-derived diagnostic checks with an accessible Close control');
  await permissionPage.locator('#androidWorkspaceCloseChecks').click();
  ensure(await permissionPage.locator('#androidWorkspaceChecksResult').isHidden(),
    'Closing native checks must hide the panel without changing tunnel status');

  // A positive aggregate tool-call counter does not supply event times or
  // prove poll-error totals/uptime. Missing native fields stay genuinely unknown.
  await permissionPage.evaluate(() => {
    window.__permissionTunnel = {state:'polling',pollActive:true,
      toolCallsVerified:17,corePluginLinked:true,events:[]};
  });
  await permissionPage.locator('#androidWorkspaceRunChecks').click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidWorkspaceVerifiedLink')?.textContent === '—' &&
    document.getElementById('androidWorkspacePollErrors')?.textContent === '—');
  ensure(await permissionPage.locator('#androidWorkspaceLastCall').innerText() === '—' &&
    await permissionPage.locator('#androidWorkspaceUptime').innerText() === '—' &&
    await permissionPage.locator('#androidWorkspaceLastTool').innerText() === '—' &&
    await permissionPage.locator('#androidWorkspaceActivityFeed .android-workspace-activity-row').count() === 0 &&
    await permissionPage.locator('#androidWorkspaceCopyActivity').isDisabled(),
    'Missing health/events on a healthy polling snapshot must not invent a recent call, poll count, uptime or log entries');

  // Actual native timestamps and events, newest first. A distinct aggregate
  // count does not generate or alter activity rows.
  const recentEvents = [
    {timestampMs:workspaceReferenceMs-45000,kind:'tool',
      detail:'OpenAI acknowledged tools/call'},
    {timestampMs:workspaceReferenceMs-95000,kind:'request',
      detail:'ChatGPT request received'}
  ];
  const liveHealth = {...historicalHealth,
    lastChatgptCallAtMs:recentEvents[0].timestampMs,
    pollErrors:2,uptimeSeconds:192,toolCount:13};
  await permissionPage.evaluate(({health,events}) => {
    window.__permissionTunnel = {state:'tool_verified',pollActive:true,
      toolCallsVerified:27,corePluginLinked:true,health,events};
  },{health:liveHealth,events:recentEvents});
  await permissionPage.locator('#androidWorkspaceRunChecks').click();
  await permissionPage.waitForFunction(() =>
    document.querySelectorAll('#androidWorkspaceActivityFeed .android-workspace-activity-row').length === 2 &&
    document.getElementById('androidWorkspacePollErrors')?.textContent === '2' &&
    document.getElementById('androidWorkspaceUptime')?.textContent === '3m 12s');
  const renderedEvents = await permissionPage.evaluate(() =>
    [...document.querySelectorAll('#androidWorkspaceActivityFeed .android-workspace-activity-row')]
      .map(row => ({
        timestamp:row.querySelector('time')?.getAttribute('datetime'),
        clock:row.querySelector('time')?.textContent,
        kind:row.querySelector('.android-workspace-activity-kind')?.textContent,
        detail:row.querySelector('.android-workspace-activity-detail')?.textContent
      })));
  const expectedEvents = recentEvents.map(event => ({
    timestamp:new Date(event.timestampMs).toISOString(),
    kind:event.kind,detail:event.detail
  }));
  ensure(renderedEvents.every((row,index) =>
    row.timestamp === expectedEvents[index].timestamp &&
    row.kind === expectedEvents[index].kind &&
    row.detail === expectedEvents[index].detail &&
    !!row.clock) &&
    await permissionPage.locator('#androidWorkspaceVerifiedLink').innerText() !== '—' &&
    await permissionPage.locator('#androidWorkspaceLastCall').innerText() !== '—' &&
    await permissionPage.locator('#androidWorkspaceLastTool').innerText() !== '—' &&
    await permissionPage.locator('#androidWorkspaceToolCount').innerText() === '13' &&
    await permissionPage.locator('#androidWorkspaceCopyActivity').isEnabled(),
    `Live native ACK must display only the two exact event receipts in order: ${JSON.stringify(renderedEvents)}`);
  await permissionPage.locator('#androidWorkspaceRunChecks').click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidWorkspaceChecksSummary')?.textContent === 'Native checks completed');
  const afterRefresh = await permissionPage.evaluate(() =>
    [...document.querySelectorAll('#androidWorkspaceActivityFeed .android-workspace-activity-row')]
      .map(row => ({
        timestamp:row.querySelector('time')?.getAttribute('datetime'),
        clock:row.querySelector('time')?.textContent,
        kind:row.querySelector('.android-workspace-activity-kind')?.textContent,
        detail:row.querySelector('.android-workspace-activity-detail')?.textContent
      })));
  ensure(JSON.stringify(afterRefresh) === JSON.stringify(renderedEvents),
    'Native activity timestamps, type and details must remain unchanged across a repeated status poll');

  await permissionPage.evaluate(() => {
    window.__workspaceClipboardWrites = [];
    Object.defineProperty(navigator,'clipboard',{configurable:true,value:{
      writeText:async text => { window.__workspaceClipboardWrites.push(text); }
    }});
  });
  await permissionPage.locator('#androidWorkspaceCopyActivity').click();
  await permissionPage.waitForFunction(() =>
    document.getElementById('androidWorkspaceActivityCopyStatus')?.textContent === 'Copied 2 event(s).');
  const copiedActivity = await permissionPage.evaluate(() => window.__workspaceClipboardWrites);
  const expectedCopy = renderedEvents.map(row =>
    `${row.clock}  ${row.kind}  ${row.detail}`).join('\n');
  ensure(copiedActivity.length === 1 && copiedActivity[0] === expectedCopy &&
    !copiedActivity[0].includes('API key') &&
    !copiedActivity[0].includes('Bearer '),
    'Activity Copy must write exact native time/type/detail lines without extra secrets or fabricated rows');
  await permissionPage.evaluate(() => {
    Object.defineProperty(navigator,'clipboard',{configurable:true,value:{
      writeText:async () => { throw Error('Synthetic clipboard denial'); }
    }});
  });
  await permissionPage.locator('#androidWorkspaceCopyActivity').click();
  ensure((await permissionPage.locator('#androidWorkspaceActivityCopyStatus').innerText()).includes('Clipboard unavailable') &&
    await permissionPage.locator('#androidWorkspaceActivityFeed .android-workspace-activity-row').count() === 2,
    'Denied clipboard must report failure while keeping exact native activity events visible');

  for (const width of [320,360,390]) {
    await permissionPage.setViewportSize({width,height:844});
    const workspaceLiveLayout = await permissionPage.evaluate(() => {
      const metrics = [...document.querySelectorAll('#androidWorkspaceDashboard .android-workspace-health-metrics > div')];
      const facts = [...document.querySelectorAll('#androidWorkspaceDashboard .android-workspace-health-row')];
      const rows = [...document.querySelectorAll('#androidWorkspaceActivityFeed .android-workspace-activity-row')];
      const controls = ['androidWorkspaceRunChecks','androidWorkspaceCopyActivity','androidWorkspaceCloseChecks'];
      const ids = ['androidWorkspaceHealthTitle','androidWorkspaceActivityTitle',
        'androidWorkspaceVerifiedLink','androidWorkspaceLastCall','androidWorkspaceRoute',
        'androidWorkspacePollErrors','androidWorkspaceTunnelStatus',
        'androidWorkspaceUptime','androidWorkspaceLastTool','androidWorkspaceTunnelClient',
        'androidWorkspaceLocalServer','androidWorkspacePluginStatus',
        'androidWorkspaceToolCount','androidWorkspaceActivityFeed',...controls];
      const nodes = ids.map(id => {
        const node = document.getElementById(id),rect=node.getBoundingClientRect();
        return {id,left:rect.left,right:rect.right,height:rect.height};
      });
      const overflow = ['androidWorkspaceDashboard','androidWorkspaceActivityFeed',
        'androidWorkspaceChecksResult'].map(id => {
        const node=document.getElementById(id);
        return {id,scroll:node.scrollWidth,client:node.clientWidth};
      });
      return {viewport:document.documentElement.clientWidth,
        document:document.documentElement.scrollWidth,metrics:metrics.length,
        facts:facts.length,rows:rows.length,nodes,overflow};
    });
    ensure(workspaceLiveLayout.document <= width+1 &&
      workspaceLiveLayout.metrics === 2 && workspaceLiveLayout.facts === 9 &&
      workspaceLiveLayout.rows === 2 &&
      workspaceLiveLayout.nodes.every(box => box.left >= -1 && box.right <= width+1) &&
      workspaceLiveLayout.nodes.filter(box => /RunChecks|CopyActivity|CloseChecks/.test(box.id))
        .every(box => box.height >= 40) &&
      workspaceLiveLayout.overflow.every(box => box.scroll <= box.client+2),
      `COS live Health/Activity metrics, 9 fact rows, 2 event rows and Copy/Close controls must fit ${width}px: ${JSON.stringify(workspaceLiveLayout)}`);
    for (const card of ['health','activity']) {
      await permissionPage.locator(`.android-workspace-${card}`).evaluate(node => {
        const scroller=document.getElementById('androidSettingsContent');
        scroller.scrollTop += node.getBoundingClientRect().top -
          scroller.getBoundingClientRect().top - 12;
      });
      await permissionPage.screenshot({
        path:path.join(screenshotFolder,`native-android-workspace-${card}-live-${width}.png`)});
    }
  }
  await permissionPage.setViewportSize({width:390,height:844});
  await switchAndroidSettingsCategory('setup');
  // Audit the actual active panel, not just the wizard grid. Exercise all ten
  // stages at narrow Android widths with native statuses already populated,
  // including keyboard focus, accessible names and usable touch surfaces.
  const responsiveSetupPanels = {
    access:'#storageAccessSection',extension:'#androidCosExtensionControls',
    browser:'#androidCosBrowserChoices',folder:'#androidCosFolderPanel',
    tools:'#androidCosAccessPanel',tunnel:'#androidCosTunnelMethod',
    key:'#androidCosKeyPanel',connect:'#androidCosConnectPanel',
    mcpPlugin:'#androidCosMcpPluginPanel',ready:'#androidCosReadyPanel'
  };
  const responsiveSetupLabels = {
    access:'PERMISSIONS',extension:'EXTENSIONS',browser:'BROWSER',folder:'FOLDER',
    tools:'ACCESS',tunnel:'TUNNEL',key:'API KEY',connect:'CONNECT',
    mcpPlugin:'MCP PLUGIN',ready:'READY'
  };
  const responsiveSetupOrder = Object.keys(responsiveSetupPanels);
  const responsiveSetupIssues = [];
  for (const {width,height} of [{width:320,height:568},{width:360,height:640},{width:390,height:844}]) {
    await permissionPage.setViewportSize({width,height});
    await permissionPage.locator('[data-cos-setup-step="access"]').focus();
    await permissionPage.keyboard.press('Tab');
    const keyboardTile = await permissionPage.evaluate(() => {
      const focus = document.activeElement;
      const style = focus && getComputedStyle(focus);
      return {
        step:focus?.getAttribute('data-cos-setup-step'),
        focusVisible:focus?.matches(':focus-visible') === true,
        ring:!!style && style.outlineStyle !== 'none' &&
          parseFloat(style.outlineWidth) >= 1
      };
    });
    if (keyboardTile.step !== 'extension' || !keyboardTile.focusVisible || !keyboardTile.ring)
      responsiveSetupIssues.push(
        `keyboard ${width}px: ${JSON.stringify(keyboardTile)}`);
    for (const [step,panelSelector] of Object.entries(responsiveSetupPanels)) {
      await permissionPage.locator(`[data-cos-setup-step="${step}"]`).click();
      // The selected panel may animate/scroll into place after the click.
      await permissionPage.waitForFunction(selector => {
        const rect = document.querySelector(selector)?.getBoundingClientRect();
        return !!rect && rect.top < innerHeight - 40 && rect.bottom > 48;
      },panelSelector,{timeout:1200}).catch(() => {});
      const panelEntry = await permissionPage.locator(panelSelector).boundingBox();
      if (!panelEntry || panelEntry.y >= height - 40 || panelEntry.y + panelEntry.height <= 48)
        responsiveSetupIssues.push(
          `entry ${step} ${width}x${height}: ${JSON.stringify(panelEntry)}`);
      await permissionPage.locator(panelSelector).scrollIntoViewIfNeeded();
      await permissionPage.locator(panelSelector).evaluate(node => {
        const scroller = document.getElementById('androidSettingsContent');
        if (!scroller) return;
        scroller.scrollTop += node.getBoundingClientRect().top -
          scroller.getBoundingClientRect().top - 12;
      });
      const audit = await permissionPage.evaluate(({step,panelSelector,expectedHeader}) => {
        const node = document.querySelector(panelSelector);
        const wizard = document.getElementById('androidCosSetupWizard');
        const scroller = document.getElementById('androidSettingsContent');
        const stepButton = document.querySelector(`[data-cos-setup-step="${step}"]`);
        const viewport = document.documentElement.clientWidth;
        const panelRect = node?.getBoundingClientRect();
        const isRendered = element => {
          if (!element || element.closest('[hidden],[aria-hidden="true"],.hidden')) return false;
          const style = getComputedStyle(element);
          const rect = element.getBoundingClientRect();
          return style.display !== 'none' && style.visibility !== 'hidden' &&
            rect.width > 0 && rect.height > 0;
        };
        const accessibleName = element => {
          const ariaLabel = element.getAttribute('aria-label')?.trim();
          if (ariaLabel) return ariaLabel;
          const refs = element.getAttribute('aria-labelledby')?.split(/\s+/).filter(Boolean);
          if (refs?.length) {
            const resolved = refs.map(id => document.getElementById(id)?.textContent || '').join(' ').trim();
            if (resolved) return resolved;
          }
          const native = element.labels?.length
            ? [...element.labels].map(label => label.textContent).join(' ').trim()
            : '';
          if (native) return native;
          const wrapped = element.closest('label')?.textContent?.trim();
          if (wrapped) return wrapped;
          return (element.textContent || element.getAttribute('title') || '').trim();
        };
        const missingNames = [],smallTargets = [],offscreenTargets = [];
        const selectors = 'button,summary,a[href],input:not([type="hidden"]),select,textarea,[role="button"]';
        for (const element of node?.querySelectorAll(selectors) || []) {
          if (!isRendered(element)) continue;
          const type = element.getAttribute('type');
          const target = (type === 'checkbox' || type === 'radio') &&
            (element.labels?.[0] || element.closest('label'))
            ? element.labels?.[0] || element.closest('label') : element;
          const rect = target.getBoundingClientRect();
          const id = element.id || element.getAttribute('name') || element.tagName.toLowerCase();
          if (!accessibleName(element)) missingNames.push(id);
          if (rect.left < -1 || rect.right > viewport + 1)
            offscreenTargets.push(`${id} ${Math.round(rect.left)}..${Math.round(rect.right)}`);
          if (!element.disabled && element.getAttribute('aria-disabled') !== 'true' &&
              (rect.width < 40 || rect.height < 40))
            smallTargets.push(`${id} ${Math.round(rect.width)}x${Math.round(rect.height)}`);
        }
        // Inspect the rendered *content heading*. The nav tiles, nested guide
        // steps, and hidden compatibility controls must never satisfy this.
        // Checking order and the complete label catches missing badges, old
        // suffixes ('MCP ACCESS · STEP 5'), duplicate step numbers, and titles
        // that are visually disconnected from their description.
        const visible = element => isRendered(element) &&
          !element.closest('details:not([open])');
        const headings = [...node.querySelectorAll('h2,h3,h4,h5,h6')]
          .filter(visible);
        const title = headings[0] || null;
        const header = title?.closest('header') || title?.parentElement || null;
        const headerHeadings = [...(header?.querySelectorAll('h2,h3,h4,h5,h6') || [])]
          .filter(visible);
        const descriptions = [...(header?.querySelectorAll('p') || [])].filter(visible);
        const badges = [...(header?.querySelectorAll('span,small') || [])]
          .filter(visible)
          .filter(element => /\bSTEP\s*\d{1,2}\b/i.test(element.textContent || ''));
        const badge = badges[0] || null;
        const badgeText = badge?.textContent?.replace(/\s+/g,' ').trim() || '';
        const titleRect = title?.getBoundingClientRect();
        const badgeRect = badge?.getBoundingClientRect();
        const descriptionRect = descriptions[0]?.getBoundingClientRect();
        const headerWords = (header?.textContent || '').match(/\bSTEP\s*\d{1,2}\b/gi) || [];
        const contentHeader = {
          expected:expectedHeader,
          eyebrow:badgeText,
          eyebrowCount:badges.length,
          numberedMentions:headerWords.length,
          titleCount:headerHeadings.length,
          title:title?.textContent?.trim() || '',
          description:descriptions[0]?.textContent?.trim() || '',
          topOrder:!!badgeRect && !!titleRect && !!descriptionRect &&
            badgeRect.top < titleRect.top && titleRect.top < descriptionRect.top,
          earlyHeader:!!badgeRect && !!panelRect && badgeRect.top >= panelRect.top - 2 &&
            badgeRect.top - panelRect.top < 130
        };
        return {
          selected:stepButton?.getAttribute('aria-current'),
          panelVisible:isRendered(node),
          viewport,documentWidth:document.documentElement.scrollWidth,
          scrollerWidth:scroller?.scrollWidth,scrollerClient:scroller?.clientWidth,
          wizardWidth:wizard?.scrollWidth,wizardClient:wizard?.clientWidth,
          panelWidth:node?.scrollWidth,panelClient:node?.clientWidth,
          panelLeft:panelRect?.left,panelRight:panelRect?.right,
          missingNames,smallTargets,offscreenTargets,contentHeader
        };
      },{step,panelSelector,
        expectedHeader:`STEP ${String(responsiveSetupOrder.indexOf(step)+1).padStart(2,'0')} · ${responsiveSetupLabels[step]}`});
      if (audit.selected !== 'step' || !audit.panelVisible ||
          audit.documentWidth > width + 1 ||
          audit.scrollerWidth > audit.scrollerClient + 2 ||
          audit.wizardWidth > audit.wizardClient + 2 ||
          audit.panelWidth > audit.panelClient + 2 ||
          audit.panelLeft < -1 || audit.panelRight > width + 1 ||
          audit.missingNames.length || audit.smallTargets.length ||
          audit.offscreenTargets.length)
        responsiveSetupIssues.push(
          `layout ${step} ${width}px: ${JSON.stringify(audit)}`);
      if (audit.contentHeader.eyebrow !== audit.contentHeader.expected ||
          audit.contentHeader.eyebrowCount !== 1 ||
          audit.contentHeader.numberedMentions !== 1 ||
          audit.contentHeader.titleCount !== 1 ||
          !audit.contentHeader.title || !audit.contentHeader.description ||
          !audit.contentHeader.topOrder || !audit.contentHeader.earlyHeader)
        responsiveSetupIssues.push(
          `heading ${step} ${width}px: ${JSON.stringify(audit.contentHeader)}`);
      await permissionPage.screenshot({
        path:path.join(screenshotFolder,`native-android-setup-polish-${step}-${width}.png`)
      });
    }
  }
  ensure(responsiveSetupIssues.length === 0,
    'Responsive Android Setup 10-panel findings:\n' + responsiveSetupIssues.join('\n'));
  await permissionPage.setViewportSize({width:390,height:844});
  await permissionPage.evaluate(() => { window.__permissionTestFail = true; });
  await permissionPage.evaluate(() => window.dispatchEvent(new Event('focus')));
  await permissionPage.waitForFunction(() => document.getElementById('overlayAccessState').textContent === 'Unknown');
  ensure(await permissionPage.locator('[data-permission-request="overlay"]').isDisabled() &&
    await permissionPage.locator('[data-permission-request="battery"]').isDisabled(),
    'Unknown permission state must fail closed and disable Settings actions');
  await permissionPage.waitForFunction(() =>
    document.querySelector('[data-cos-setup-step="access"] small').textContent === 'Unable to check' &&
    document.querySelector('[data-cos-setup-step="ready"] small').textContent === 'Unable to check');
  ensure(await permissionPage.locator('[data-cos-setup-step="access"] small').innerText() === 'Unable to check' &&
    await permissionPage.locator('[data-cos-setup-step="ready"] small').innerText() === 'Unable to check',
    'Unreachable native status must revoke current Access proof and distinguish uncertainty from denial');
  await permissionPage.close();
  // Verify Android MCP and bounded local-worker panel lifecycles with synthetic
  // Tauri commands only. No DNS, provider, remote MCP or private file access.
  const nativeTools = await browser.newPage({
    viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true,
    userAgent: 'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36',
  });
  nativeTools.on('pageerror', error => errors.push(error.message));
  await nativeTools.route('**/api/state', async route => {
    const response = await route.fetch();
    const payload = await response.json();
    await route.fulfill({ response, json: { ...payload, platform: 'android',
      providers:['chatgpt-browser'],hasKeys: [],
      sessions: [{ id: 'c23ec71e-bfe9-4fe5-b931-48074cf30b2d', provider: 'chatgpt-browser',
        model: 'chatgpt.com', messages: [{role: 'user', text: 'Synthetic review export'}] },
        {id:'d4be29af-35ca-4191-8ae2-f0377b1da9df',provider:'chatgpt-browser',model:'chatgpt.com',
          messages:[{role:'user',text:'Synthetic second rules chat'}]}] } });
  });
  await nativeTools.goto(app.url);
  await nativeTools.evaluate(() => {
    window.isTauri = true;
    window.__mcpTestCalls = [];
    window.__mcpTestConnected = false;
    window.__browserTest = { opened: false, activeTabId: 0, automationAllowed: false,
      isolatedProfile: false, profileIsolationSupported: true, lastProfileCleanup: 'not_attempted', tabs: [] };
    window.__browserCalls = [];
    window.__browserPreviewMode = 'success';
    window.__ownedChatgpt = {supported:true,running:false,visible:false,
      engine:'Synthetic owned WebView',message:'Not started'};
    window.__ownedChatgptCalls = [];
    window.__ownedChatgptTurn = null;
    window.__ownedChatgptSends = 0;
    window.__ownedChatgptBridgeCalls = [];
    window.__scopedRulesAndroid = {global:{enabled:false,text:''},sessions:{}};
    window.__scopedRulesAndroidCalls = [];
    window.__lastBrowserPreparedPrompt = null;
    window.__toolTestBrowserSession = null;
    window.__browserPreviewPrepareCount = 0;
    window.__browserPreviewCaptureCount = 0;
    window.__treeTestGranted = false;
    window.__treeWriteGranted = false;
    window.__treeFolders = [];
    window.__treeImageVisible = false;
    window.__treeImageMode = 'success';
    window.__treeImageReadCompleted = 0;
    window.__treeFolderMode = 'create';
    window.__treeRenameMode = 'success';
    window.__nativeImportedSkills = [];
    window.__skillTestSource = '---\nname: Synthetic Imported Skill\ndescription: Explicit import fixture\n---\nReview the requested task carefully.';
    window.__treeCalls = [];
    window.__nativeProjectsQA = { version: 1, projects: [], assignments: {} };
    window.__projectFolderBindings = {};
    window.__projectFolderCalls = [];
    window.__projectFolderMode = 'success';
    window.__androidNativeToolCalls = [];
    window.__mcpAuditEvents = [];
    window.__mcpAuditQueries = 0;
    window.__goalReviewQueries = 0;
    window.__goalReviewMode = 'success';
    window.__goalReviewReceipt = null;
    window.__goalReviewRelease = null;
    window.__goalRunMode = 'error';
    window.__goalRunCalls = [];
    window.__goalMcpReviewMode = 'success';
    window.__goalMcpReviewCalls = [];
    window.__goalMcpExecuted = 0;
    window.__goalMcpPending = null;
    window.__goalMcpReviewRelease = null;
    window.__goalDraftMode = 'valid';
    window.__goalDraftCalls = [];
    window.__delegateJobs = [];
    window.__delegateCalls = [];
    window.__delegateMode = 'success';
    window.__delegateReleases = [];
    window.__primeTestTask = null;
    window.__primeTestCalls = [];
    window.__petOverlay = {supported:true,permissionGranted:false,visible:false,
      foreground:true,pendingConsent:false,sizeDp:56,mode:'app-scoped'};
    window.__petOverlayCalls = [];
    window.__petOverlayMode = 'success';
    window.__speechTestMode = 'success';
    window.__speechCalls = [];
    window.__speechResolve = null;
    window.__ptyHandler = null;
    window.__ptySessionId = null;
    window.__ptySessionIds = [];
    window.__ptyWrites = [];
    window.__TAURI_INTERNALS__ = {
      transformCallback: callback => {
        window.__ptyHandler = callback;
        return 101;
      },
      invoke: async (command, args) => {
        if (command.startsWith('android_foreground_runtime_')) {
          window.__androidNativeToolCalls.push('fgs:unexpected-js-call');
          throw Error('Foreground runtime must be controlled solely by Android lifecycle');
        }
        if (command === 'plugin:event|listen') {
          if (args.event !== 'mcp-android-pty' || args.handler !== 101)
            throw Error('Unexpected PTY event subscription');
          window.__androidNativeToolCalls.push('pty:listen');
          return 1;
        }
        if (command === 'android_pty_status') {
          window.__androidNativeToolCalls.push('pty:status');
          return {supported:true,kind:'android-in-app-pty',shell:'/system/bin/sh',
            active:window.__ptySessionIds.length,maxSessions:4,userInitiatedRequired:true,
            automatedCommandsAllowed:false,sandboxed:false,termuxRequired:false};
        }
        if (command === 'android_pty_create') {
          if (args.userInitiated !== true ||
              !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(args.id) ||
              args.cols < 2 || args.rows < 1 ||
              window.__ptySessionIds.includes(args.id) || window.__ptySessionIds.length >= 4)
            throw Error('PTY session start is not explicitly approved or scoped');
          window.__ptySessionIds.push(args.id);
          window.__ptySessionId = args.id;
          window.__androidNativeToolCalls.push('pty:create');
          return {id:args.id,shell:'sh',started:true,
            cwd:'/data/user/0/ai.mcpagent.app/files/terminal-workspaces/default'};
        }
        if (command === 'android_pty_write') {
          if (args.userInitiated !== true || !window.__ptySessionIds.includes(args.id) ||
              new TextEncoder().encode(args.data).length > 4096)
            throw Error('Unexpected or automated PTY write');
          window.__ptyWrites.push({id:args.id,data:args.data});
          window.__androidNativeToolCalls.push('pty:write:' + args.data);
          return null;
        }
        if (command === 'android_pty_resize') {
          if (!window.__ptySessionIds.includes(args.id) || args.cols < 2 || args.rows < 1)
            throw Error('Unexpected PTY resize');
          window.__androidNativeToolCalls.push('pty:resize');
          return null;
        }
        if (command === 'android_pty_ack') {
          if (!window.__ptySessionIds.includes(args.id) || args.count > 4096)
            throw Error('PTY acknowledgement is not scoped');
          window.__androidNativeToolCalls.push('pty:ack:' + args.count);
          return null;
        }
        if (command === 'android_pty_close') {
          if (args.userInitiated !== true || !window.__ptySessionIds.includes(args.id))
            throw Error('PTY close needs explicit user initiation');
          window.__androidNativeToolCalls.push('pty:close');
          window.__ptySessionIds = window.__ptySessionIds.filter(id => id !== args.id);
          window.__ptySessionId = window.__ptySessionIds.at(-1) ?? null;
          return null;
        }
        if (command.startsWith('chatgpt_browser_')) {
          const action = command.slice('chatgpt_browser_'.length);
          window.__ownedChatgptBridgeCalls.push(action);
          const turn = window.__ownedChatgptTurn;
          if (action === 'prepare') {
            if (!window.__ownedChatgpt.running || turn || args.sessionId !== null ||
                !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(args.requestId))
              throw Error('Owned browser or request lease unavailable');
            window.__lastBrowserPreparedPrompt = args.prompt;
            window.__ownedChatgptTurn = {requestId:args.requestId,prompt:args.prompt,send:false};
            return {requestId:args.requestId,state:'prepared',accepted:false,completed:false};
          }
          if (!turn || args.requestId !== turn.requestId) throw Error('Stale browser request');
          if (action === 'cancel') {
            if (turn.send) throw Error('Cannot cancel after possible Send');
            window.__ownedChatgptTurn = null;
            return {requestId:args.requestId,state:'cancelled',accepted:false,completed:false};
          }
          if (action === 'send') {
            if (args.userInitiated !== true || turn.send) throw Error('Send requires one explicit approval');
            turn.send = true; window.__ownedChatgptSends++;
            return {requestId:args.requestId,state:'submitted',accepted:false,completed:false};
          }
          if (action === 'observe') {
            return turn.send
              ? {requestId:turn.requestId,state:'completed',accepted:true,completed:true,
                  answer:'Synthetic exact owned ChatGPT answer'}
              : {requestId:turn.requestId,state:'prepared',accepted:false,completed:false};
          }
          if (action === 'finalize') {
            if (!turn.send || window.__toolTestBrowserSession)
              throw Error('Duplicate or unverified ChatGPT save');
            const id = '1ce23b91-a226-49e8-8248-ac4ed602164b';
            window.__toolTestBrowserSession = {id,provider:'chatgpt-browser',model:'chatgpt.com',
              messages:[{role:'user',text:turn.prompt},
                {role:'assistant',text:'Synthetic exact owned ChatGPT answer'}]};
            window.__ownedChatgptTurn = null;
            return {sessionId:id,answer:'Synthetic exact owned ChatGPT answer',actions:[]};
          }
          throw Error('Unknown owned browser command');
        }
        if (command.startsWith('android_chatgpt_')) {
          const action = command.slice('android_chatgpt_'.length);
          if (!['status','probe','open','show','hide','close'].includes(action)) throw Error('Unknown ChatGPT browser operation');
          window.__ownedChatgptCalls.push(action);
          if (action === 'probe') {
            if (!window.__ownedChatgpt.running || !window.__ownedChatgpt.visible)
              throw Error('Cannot probe a hidden or stopped ChatGPT browser');
            if (window.__ownedChatgptProbeMode === 'forged')
              return {owned:true,origin:'chatgpt.com',readOnly:true,
                pageGeneration:1,composerReady:true,sendButtonReady:false,privateData:'forbidden'};
            return {owned:true,origin:'chatgpt.com',readOnly:true,
              pageGeneration:1,composerReady:window.__ownedChatgptProbeMode !== 'missing',sendButtonReady:false};
          }
          if (action === 'open') {
            if (args?.userInitiated !== true) throw Error('Browser requires an explicit user gesture');
            window.__ownedChatgpt.running = true; window.__ownedChatgpt.visible = true;
          }
          if (action === 'show' && window.__ownedChatgpt.running) window.__ownedChatgpt.visible = true;
          if (action === 'hide' && window.__ownedChatgpt.running) window.__ownedChatgpt.visible = false;
          if (action === 'close') {window.__ownedChatgpt.running = false;window.__ownedChatgpt.visible = false;}
          window.__ownedChatgpt.message = `Synthetic owned browser ${action}`;
          return {...window.__ownedChatgpt};
        }
        if (command === 'speech_dictation_status') {
          window.__speechCalls.push('status');
          return {supported:true,available:window.__speechTestMode !== 'unavailable',
            listening:false,source:'Android speech recognizer',autoSent:false};
        }
        if (command === 'speech_dictation_recognize') {
          if (args?.userInitiated !== true ||
              !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(args.requestId))
            throw Error('Dictation request requires explicit canonical UUID');
          window.__speechCalls.push('recognize');
          window.__speechRequestId = args.requestId;
          const accepted = (text, cancelled=false, requestId=args.requestId) =>
            ({requestId,recognized:!cancelled,cancelled,text,
              source:'Android speech recognizer',autoSent:false});
          if (window.__speechTestMode === 'cancel') return accepted('',true);
          if (window.__speechTestMode === 'forged') return accepted('Unsafe injected text',false,'not-the-request');
          if (window.__speechTestMode === 'deferred') return new Promise(resolve => {
            window.__speechResolve = () => resolve(accepted('Delayed voice text'));
          });
          return accepted('Recognized synthetic speech');
        }
        if (command === 'pet_overlay_status') {
          window.__petOverlayCalls.push('status');
          return {...window.__petOverlay};
        }
        if (command === 'pet_overlay_show') {
          if (args?.userInitiated !== true || !window.__petOverlay.permissionGranted ||
              !window.__petOverlay.foreground) throw Error('Overlay lacks explicit grant/gesture');
          window.__petOverlayCalls.push('show');
          if (window.__petOverlayMode === 'cancel')
            return {...window.__petOverlay,shown:false,cancelled:true};
          window.__petOverlay.visible = true;
          return {...window.__petOverlay,shown:true,cancelled:false};
        }
        if (command === 'pet_overlay_hide') {
          if (args?.userInitiated !== true) throw Error('Explicit overlay hide required');
          window.__petOverlayCalls.push('hide');
          window.__petOverlay.visible = false;
          return {...window.__petOverlay,hidden:true};
        }
        if (command === 'android_permissions_status') return {
          files:{supported:true,granted:false},
          overlay:{supported:true,granted:false},
          notifications:{supported:true,granted:false},
          battery:{supported:true,granted:false},
        };
        if (command === 'android_termux_status') {
          window.__androidNativeToolCalls.push('termux:status');
          return {supported:true,installed:!!window.__termuxInstalled,launchable:!!window.__termuxInstalled,
            mode:'external-termux-app',inAppShellAvailable:false,setupVerified:false,
            installIsUserManaged:true,commandsAvailableInApp:false};
        }
        if (command === 'android_termux_install') {
          if (args?.userInitiated !== true) throw Error('Termux install requires user action');
          window.__androidNativeToolCalls.push('termux:install');
          return {opened:true,installedByApp:false,shellStartedByApp:false};
        }
        if (command === 'android_termux_open') {
          if (args?.userInitiated !== true || !window.__termuxInstalled)
            throw Error('Termux open requires installed app and user action');
          window.__androidNativeToolCalls.push('termux:open');
          return {opened:true,installedByApp:false,shellStartedByApp:false};
        }
        if (command === 'android_terminal_status') return {
          supported:true,permissionRequired:false,running:false,scope:'Dedicated app-private terminal-public folder',
        };
        if (command === 'android_terminal_run') {
          if (!args.userInitiated || args.command !== 'pwd' || args.name !== null ||
              !/^[0-9a-f-]{36}$/.test(args.requestId)) throw Error('Unexpected real native terminal action');
          window.__androidNativeToolCalls.push('terminal:pwd');
          return {requestId:args.requestId,output:'/data/user/0/ai.mcpagent.app/files/terminal-public',
            readOnly:true,scope:'terminal-public',exitCode:0};
        }
        if (command === 'android_terminal_cancel') {
          if (!args.userInitiated) throw Error('Explicit terminal stop required');
          window.__androidNativeToolCalls.push('terminal:cancel');
          return {requestId:args.requestId,cancelRequested:true};
        }
        if (command === 'review_share_markdown') {
          if (!args.userInitiated || !args.text.startsWith('# Mcp Agent Ai conversation\n\nProvider: '))
            throw Error('A reviewed actual native Markdown export is required');
          window.__androidNativeToolCalls.push('review:share');
          return {opened:true};
        }
        if (command === 'accessibility_status')
          return {supported:true,enabled:true,connected:true,needsReconnect:false};
        if (command === 'accessibility_request_settings') {
          if (!args.userInitiated) throw Error('Explicit request required');
          window.__androidNativeToolCalls.push('accessibility:settings');
          return {opened:true,enabled:true};
        }
        if (command.startsWith('in_app_browser_')) {
          window.__browserCalls.push(command);
          const state = window.__browserTest;
          if (command === 'in_app_browser_status' || command === 'in_app_browser_list_tabs') return {...state};
          if (command === 'in_app_browser_open') {
            if (!args.userInitiated) throw Error('User initiation required');
            state.opened = true;
            state.isolatedProfile = true;
            state.activeTabId = 1;
            state.tabs = [{ tabId: 1, title: 'Synthetic HTTPS page', url: 'https://synthetic.example/', active: true }];
            return { opened: true, tab: state.tabs[0] };
          }
          if (command === 'in_app_browser_switch_tab') {
            if (!state.tabs.some(tab => tab.tabId === args.tabId)) throw Error('Tab missing');
            state.activeTabId = args.tabId;
            state.tabs.forEach(tab => {tab.active = tab.tabId === args.tabId;});
            return {...state};
          }
          if (command === 'in_app_browser_close_tab') {
            state.tabs = state.tabs.filter(tab => tab.tabId !== args.tabId);
            state.activeTabId = state.tabs[0]?.tabId || 0;
            state.opened = state.tabs.length > 0;
            return {...state};
          }
          if (command === 'in_app_browser_prepare_preview') {
            if (!args.userInitiated || !state.opened || args.tabId !== state.activeTabId)
              throw Error('One-time visible tab user gesture required');
            window.__browserPreviewPrepareCount++;
            return {tabId:args.tabId,previewToken:'00000000-0000-4000-8000-000000000001',
              pageGeneration:1,expiresInMs:60000};
          }
          if (command === 'in_app_browser_capture_preview') {
            if (!args.userInitiated || args.previewToken !== '00000000-0000-4000-8000-000000000001')
              throw Error('Captured preview must use the issued one-time token');
            window.__browserPreviewCaptureCount++;
            if (window.__browserPreviewMode === 'pending')
              await new Promise(resolve => {window.__browserPreviewRelease = resolve;});
            if (window.__browserPreviewMode === 'cancel')
              return {captured:false,cancelled:true,previewTokenConsumed:true};
            const base64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/mZoAAAAASUVORK5CYII=';
            return {captured:true,mimeType:'image/png',base64,width:1,height:1,
              pngBytes:atob(base64).length,tabId:args.tabId,pageGeneration:1,
              previewTokenConsumed:true};
          }
          if (command === 'in_app_browser_automation') {
            if (!args.userInitiated) throw Error('Consent required');
            state.automationAllowed = true;
            return {allowed: true};
          }
          if (command === 'in_app_browser_snapshot') {
            if (!state.automationAllowed) throw Error('Automation is not allowed');
            return { tabId: 1, snapshotId: 91, title: 'Synthetic HTTPS page',
              text: 'Synthetic HTML from owned tab', elements: [
                { index: 1, tag: 'button', label: 'Synthetic button', fillable: false },
                { index: 2, tag: 'input', label: 'Synthetic input', fillable: true },
              ] };
          }
          if (command === 'in_app_browser_click' || command === 'in_app_browser_fill' ||
              command === 'in_app_browser_scroll') {
            if (args.snapshotId !== 91) throw Error('Stale synthetic DOM snapshot');
            return {ok:true,snapshotInvalidated:true};
          }
          if (command === 'in_app_browser_inspect') {
            if (!state.automationAllowed || !state.opened ||
                args.tabId !== state.activeTabId || args.snapshotId !== 91 || args.index !== 1)
              throw Error('Stale or unconsented indexed inspection');
            return {ok:true,tabId:args.tabId,snapshotId:91,index:1,
              snapshotInvalidated:false,tag:'button',role:'button',label:'Synthetic button',
              attributes:{type:'button',disabled:false,readOnly:false,required:false,
                ariaExpanded:false,ariaPressed:false,ariaChecked:false}};
          }
          if (command === 'in_app_browser_hover') {
            if (!state.automationAllowed || !state.opened ||
                args.tabId !== state.activeTabId || args.snapshotId !== 91 || args.index !== 1)
              throw Error('Stale or unconsented hover');
            return {ok:true,tabId:args.tabId,snapshotInvalidated:true,
              syntheticEvents:true,physicalPointerMoved:false};
          }
          if (command === 'in_app_browser_close_all') {
            state.opened = false; state.activeTabId = 0; state.automationAllowed = false;
            state.isolatedProfile = false; state.lastProfileCleanup = 'requested_async_deletion'; state.tabs = [];
            return {...state};
          }
          throw Error('Unexpected in-app browser command: '+command);
        }
        if (command.startsWith('document_tree_project_folder_')) {
          const id = args?.projectId;
          if (typeof id !== 'string' || !window.__nativeProjectsQA.projects.some(p => p.id === id))
            throw Error('Project SAF grant must reference an existing Project');
          const action = command.slice('document_tree_project_folder_'.length);
          window.__projectFolderCalls.push({action, id, userInitiated:args?.userInitiated === true});
          if (action === 'status') {
            const linked = !!window.__projectFolderBindings[id];
            return {projectId:id, linked, granted:linked, readOnly:true,
              displayName:linked ? 'QA Phone Folder' : '', pickerOpen:false,
              treeUri:linked ? 'content://qa.documents/tree/project-only' : ''};
          }
          if (args?.userInitiated !== true) throw Error('Explicit Project SAF gesture required');
          if (action === 'request') {
            if (window.__projectFolderMode === 'cancel')
              return {projectId:id,selected:false,cancelled:true};
            if (window.__projectFolderMode === 'invalid')
              return {projectId:id,selected:true,granted:false,persisted:false,readOnly:true};
            window.__projectFolderBindings[id] = true;
            return {projectId:id,selected:true,granted:true,persisted:true,readOnly:true,
              displayName:'QA Phone Folder',treeUri:'content://qa.documents/tree/project-only'};
          }
          if (action === 'release') {
            const released = !!window.__projectFolderBindings[id];
            delete window.__projectFolderBindings[id];
            return {projectId:id,released,linked:false,filesDeleted:false};
          }
          throw Error('Unexpected Project SAF operation');
        }
        if (command === 'document_tree_status') return {
          granted: window.__treeTestGranted,
          treeUri: window.__treeTestGranted ? 'content://qa.documents/tree/synthetic' : '',
          pickerOpen: false,
          writeGranted: window.__treeWriteGranted,
        };
        if (command === 'document_tree_request') {
          window.__treeCalls.push('request');
          window.__treeTestGranted = true;
          return { selected: true };
        }
        if (command === 'document_tree_request_write') {
          if (!args.userInitiated || !window.__treeTestGranted) throw Error('Explicit read tree and user gesture required');
          window.__treeCalls.push('request-write');
          window.__treeWriteGranted = true;
          return {selected:true,writeGranted:true};
        }
        if (command === 'document_tree_create_text') {
          if (!args.userInitiated || !window.__treeWriteGranted || args.treeUri !== 'content://qa.documents/tree/synthetic' ||
              !args.name.endsWith('.txt') || args.path.length) throw Error('Unapproved or unsafe file creation');
          window.__treeCalls.push('create:' + args.name + ':' + args.content);
          return {created:true,name:args.name,treeUri:args.treeUri};
        }
        if (command === 'document_tree_create_folder') {
          if (!args.userInitiated || !window.__treeWriteGranted ||
              args.treeUri !== 'content://qa.documents/tree/synthetic' ||
              !Array.isArray(args.path) || args.path.length ||
              !/^[\w -]{1,128}$/.test(args.name))
            throw Error('Invalid native SAF folder consent, selected tree or folder name');
          window.__treeCalls.push('create-folder:' + args.name);
          if (window.__treeFolderMode === 'cancel') return {created:false,cancelled:true};
          if (window.__treeFolders.includes(args.name)) throw Error('Folder exists; no overwrite');
          window.__treeFolders.push(args.name);
          return {created:true,treeUri:args.treeUri,name:args.name,
            path:[...args.path,args.name],parentPath:args.path,isDirectory:true,
            mimeType:'vnd.android.document/directory'};
        }
        if (command === 'document_tree_rename_entry') {
          const oldName = args.path?.at(-1);
          if (args.userInitiated !== true || !window.__treeWriteGranted ||
              args.treeUri !== 'content://qa.documents/tree/synthetic' ||
              !Array.isArray(args.path) || args.path.length !== 1 ||
              !window.__treeFolders.includes(oldName) || !/^[\w -]{1,128}$/.test(args.name))
            throw Error('Unsafe or unauthorised native rename');
          window.__treeCalls.push('rename:' + oldName + ':' + args.name);
          if (window.__treeRenameMode === 'cancel')
            return {renamed:false,cancelled:true,uncertain:false};
          if (window.__treeRenameMode === 'uncertain')
            return {renamed:false,verified:false,uncertain:true,
              providerAttempted:true,message:'Synthetic provider changed uncertain'};
          if (window.__treeFolders.includes(args.name)) throw Error('Existing sibling cannot be overwritten');
          window.__treeFolders[window.__treeFolders.indexOf(oldName)] = args.name;
          return {renamed:true,verified:true,uncertain:false,treeUri:args.treeUri,
            parentPath:[],oldPath:[oldName],newPath:[args.name],
            oldName,name:args.name,isDirectory:true,mimeType:'vnd.android.document/directory',
            documentIdChanged:true};
        }
        if (command === 'document_tree_properties') {
          if (args.userInitiated !== true || args.treeUri !==
              'content://qa.documents/tree/synthetic' || !Array.isArray(args.path) ||
              args.path.length !== 1 ||
              !['selected.txt', ...window.__treeFolders].includes(args.path[0]))
            throw Error('Invalid selected-tree properties request');
          window.__treeCalls.push('properties:' + args.path[0]);
          const isDirectory = window.__treeFolders.includes(args.path[0]);
          return {treeUri:args.treeUri,path:args.path,name:args.path[0],isDirectory,
            mimeType:isDirectory ? 'vnd.android.document/directory' : 'text/plain',
            size:isDirectory ? undefined : 23,lastModified:1700000000000,
            canWrite:true,canRename:true,canDelete:false};
        }
        if (command === 'document_tree_list') {
          window.__treeCalls.push('list');
          return { entries: [{ name: 'selected.txt', isDirectory: false,
            mimeType: 'text/plain', size: 23 },
            ...(window.__treeImageVisible ? [{name:'preview.png',isDirectory:false,
              mimeType:'image/png',size:70}] : []),
            ...window.__treeFolders.map(name => ({name,isDirectory:true,
              mimeType:'vnd.android.document/directory',size:null}))] };
        }
        if (command === 'document_tree_open') {
          window.__treeCalls.push('open');
          if (args.path.at(-1) === 'preview.png') {
            if (window.__treeImageMode === 'deferred') {
              await new Promise(resolve => { window.__treeImageRelease = resolve; });
            }
            window.__treeImageReadCompleted++;
            // Exact valid synthetic 1x1 RGBA PNG with correct CRCs, not a
            // header-only fake; WebView decode() must succeed before display.
            const good = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNQcEj4DwADBAHAgChw2gAAAABJRU5ErkJggg==';
            const invalid = btoa('not really a PNG');
            const data = window.__treeImageMode === 'badBytes' ? invalid : good;
            return {treeUri:window.__treeImageMode === 'forged' ?
              'content://other.documents/tree/synthetic' : args.treeUri,
              path:args.path,mimeType:'image/png',encoding:'base64',size:atob(data).length,data};
          }
          if (args.path.at(-1) === 'SKILL.md') {
            const text = window.__skillTestSource;
            return {treeUri:args.treeUri,path:args.path,mimeType:'text/plain',
              encoding:'base64',size:text.length,data:btoa(text)};
          }
          const text = 'Selected folder QA text';
          return {treeUri:args.treeUri,path:args.path,mimeType:'text/plain',
            encoding:'base64',size:text.length,data:btoa(text)};
        }
        if (command !== 'api_request') throw Error('Unexpected command');
        const { route, body } = args;
        if (route === '/api/state') return {
          platform:'android', modules:{}, availableModules:[],
          skillsAvailable:true, skills:[...window.__nativeImportedSkills],
          providers:['chatgpt-browser'], hasKeys:[], workspace:'Synthetic native app',
          toolPanels:{review:true,terminal:true,files:true,subagents:true},
          sessions:[{id:'c23ec71e-bfe9-4fe5-b931-48074cf30b2d',provider:'chatgpt-browser',
            model:'chatgpt.com', messages:[{role:'user',text:'Synthetic review export'}]},
            {id:'d4be29af-35ca-4191-8ae2-f0377b1da9df',provider:'chatgpt-browser',model:'chatgpt.com',
              messages:[{role:'user',text:'Synthetic second rules chat'}]},
            ...(window.__toolTestSession ? [window.__toolTestSession] : []),
            ...(window.__toolTestBrowserSession ? [window.__toolTestBrowserSession] : [])],
        };
        if (route === '/api/standing-rules') {
          const id=body.sessionId;
          if (id !== null && !['c23ec71e-bfe9-4fe5-b931-48074cf30b2d',
            'd4be29af-35ca-4191-8ae2-f0377b1da9df'].includes(id))
            throw Error('Unknown native chat session ID');
          window.__scopedRulesAndroidCalls.push(structuredClone(body));
          const saved=window.__scopedRulesAndroid;
          if (body.action === 'get') return {sessionId:id,
            global:structuredClone(saved.global),
            session:structuredClone(saved.sessions[id] || {enabled:false,text:''}),legacy:null};
          if (body.action === 'save' && body.userInitiated === true &&
              ['global','session'].includes(body.scope) &&
              (body.scope === 'global' || id !== null)) {
            const rule={enabled:body.enabled,text:body.text};
            if (body.scope === 'global') saved.global=rule;
            else saved.sessions[id]=rule;
            return {sessionId:id,scope:body.scope,...rule};
          }
          if (body.action === 'reset' && body.userInitiated === true && id !== null &&
              Object.keys(body).length === 3) {
            delete saved.sessions[id];
            return {sessionId:id,scope:'session',enabled:false,text:'',reset:true};
          }
          throw Error('Invalid scoped rules action');
        }
        if (route === '/api/mobile-skills') {
          if (body.action !== 'add') throw Error('Unexpected synthetic skill mutation');
          window.__mcpTestCalls.push('skill:add:' + body.name);
          window.__nativeImportedSkills.push({id:'synthetic-imported-skill',name:body.name,enabled:false});
          return {skills:[...window.__nativeImportedSkills]};
        }
        if (route === '/api/workspace-search' && body.mode === 'content') {
          if (body.userInitiated !== true) throw Error('Explicit content search gesture missing');
          window.__mcpTestCalls.push('private-search-text:' + body.query);
          if (body.query === 'tampered') return {query:body.query,
            scope:'App-private user-files/',readOnly:true,total:1,truncated:false,
            results:[{path:'chat-history/secret.json',line:1,excerpt:'unsafe data'}]};
          return {query:body.query,scope:'App-private user-files/',readOnly:true,
            total:body.query === 'Synthetic' ? 1 : 0,truncated:false,
            results:body.query === 'Synthetic' ?
              [{path:'user-files/synthetic.txt',line:1,excerpt:'Synthetic local text'}] : []};
        }
        if (route === '/api/workspace') {
          if (body.action === 'list') return {path:body.path,entries:[],readOnly:true};
          if (body.action === 'create_text' || body.action === 'replace_text') {
            if (body.allowWrite !== true || !body.path.startsWith('user-files/'))
              throw Error('Private write requires explicit consent and user-files scope');
            window.__mcpTestCalls.push('private-write:' + body.action + ':' + body.path + ':' + body.content);
            return {path:body.path,written:true,created:body.action === 'create_text'};
          }
          throw Error('Unexpected synthetic workspace action');
        }
        if (route === '/api/mcp') {
          window.__mcpTestCalls.push(body.action);
          if (body.action === 'status') return { connected: window.__mcpTestConnected };
          if (body.action === 'connect') {
            window.__mcpTestConnected = true;
            return { connected: true, tools: { tools: [
              { name: 'test_read', description: 'Read synthetic data', inputSchema: { type: 'object' } },
            ] } };
          }
          if (body.action === 'call') {
            if (body.userInitiated !== true) throw Error('Explicit MCP tool gesture is mandatory');
            return { result: { content: [{ type: 'text', text: 'synthetic MCP' }] }, isError: false };
          }
          if (body.action === 'disconnect') { window.__mcpTestConnected = false; return { connected: false }; }
          return { tools: [] };
        }
        if (route === '/api/mobile-agents') {
          window.__mcpTestCalls.push('worker:' + body.action);
          if (body.action === 'status') return { stats: { running: 0, queued: 0, retained: window.__workerJob ? 1 : 0 },
            history: window.__workerJob ? [window.__workerJob] : [] };
          if (body.action === 'submit') {
            window.__workerJob = { id: 'synthetic-job-1', sequence: 1, operation: body.job,
              status: 'queued', progress: 0 };
            return { job: window.__workerJob };
          }
          if (body.action === 'get') {
            window.__workerJob.status = 'completed';
            window.__workerJob.progress = 100;
            window.__workerJob.result = { results: [] };
            return { job: window.__workerJob };
          }
        }
        if (route === '/api/mobile-ai-agents') {
          window.__delegateCalls.push({action:body.action,...structuredClone(body)});
          if (body.action === 'status') return {
            jobs:structuredClone(window.__delegateJobs),
            running:window.__delegateJobs.filter(job => job.status === 'running').length,
            maxConcurrent:2,
          };
          if (body.action === 'run') {
            if (body.userInitiated !== true || body.provider !== 'openai' ||
                body.model !== 'synthetic-model' ||
                !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(body.requestId) ||
                !body.task || new TextEncoder().encode(body.task).length > 3000 ||
                window.__delegateJobs.filter(job => job.status === 'running').length >= 2)
              throw Error('Unsafe or over-capacity synthetic AI delegate request');
            const job = {id:'delegate-' + (window.__delegateJobs.length + 1),
              task:body.task,provider:body.provider,model:body.model,status:'running'};
            window.__delegateJobs.push(job);
            if (window.__delegateMode === 'error') {
              job.status = 'failed';
              job.error = 'Synthetic provider failure; no response confirmed';
              throw Error('Synthetic delegate provider failure');
            }
            if (window.__delegateMode === 'pending') {
              await new Promise(resolve => window.__delegateReleases.push({id:job.id,resolve}));
            }
            if (job.status === 'running') {
              job.status = 'completed';
              job.answer = 'Synthetic provider response for ' + body.task;
            }
            return {job:structuredClone(job)};
          }
          if (body.action === 'cancel') {
            if (!body.userInitiated) throw Error('Native delegate cancellation requires user gesture');
            const job = window.__delegateJobs.find(job => job.id === body.id);
            if (!job || !['running','queued'].includes(job.status))
              throw Error('Only active delegate can be cancelled');
            job.status = 'cancelled';
            return {job:structuredClone(job)};
          }
          throw Error('Unknown delegate action');
        }
        if (route === '/api/mobile-prime') {
          window.__primeTestCalls.push(structuredClone(body));
          if (body.action === 'status') return {task:structuredClone(window.__primeTestTask),
            maxConcurrentWorkers:2,automaticExecution:false,backgroundReplay:false,modelToolsExecuted:false};
          if (body.userInitiated !== true) throw Error('Prime needs explicit user gesture');
          if (body.action === 'start') {
            if (!body.goal || window.__primeTestTask?.status === 'active')
              throw Error('A new Prime needs a nonempty goal and no active run');
            window.__primeTestTask={id:'aa813f83-cc65-4ecd-b7e7-e1d3104978c8',
              goal:body.goal,status:'active',workers:[],summary:null};
            return {task:structuredClone(window.__primeTestTask)};
          }
          if (body.action === 'finish' && !body.workerId) {
            if (body.taskId !== window.__primeTestTask?.id || !body.summary)
              throw Error('Prime finish requires matching task and user summary');
            window.__primeTestTask.status='finished';
            window.__primeTestTask.summary=body.summary;
            return {task:structuredClone(window.__primeTestTask),summaryUserWritten:true,autoVerified:false};
          }
          if (body.action === 'cancel') {
            if (body.taskId !== window.__primeTestTask?.id)
              throw Error('Prime cancellation must be task-scoped');
            window.__primeTestTask.status='cancelled';
            return {task:structuredClone(window.__primeTestTask),cancelRequests:0};
          }
          throw Error('No configured provider was authorized for a Prime worker');
        }
        if (route === '/api/mobile-goal') {
          window.__mcpTestCalls.push('goal:' + body.action);
          if (body.action === 'status') return { goal: structuredClone(window.__goalTest || null) };
          if (body.userInitiated !== true)
            throw Error('Goal mutations need an explicit user gesture');
          if (body.action === 'create') {
            const approval = body.spec.steps[0]?.requires_approval === true;
            window.__goalTest = { id: '018dc513-8808-4bd8-90c0-287c50eeb149',
              status: approval ? 'awaiting_approval' : 'ready',
              approval_reason: approval ? 'Step requires manual approval' : null,
              approved_for_step:false,
              spec: structuredClone(body.spec), current_step: 0, turns_used: 0,
              active_turn: null };
          }
          if (body.action === 'approval') {
            if (window.__goalTest.status !== 'awaiting_approval' ||
                typeof body.approved !== 'boolean')
              throw Error('Approval must be an explicit response to the current step');
            if (body.approved) {
              window.__goalTest.status = 'ready';
              window.__goalTest.approved_for_step = true;
              window.__goalTest.approval_reason = null;
            } else window.__goalTest.status = 'rejected';
          }
          if (body.action === 'pause') {
            if (body.userInitiated !== true ||
                !['ready','awaiting_approval'].includes(window.__goalTest.status))
              throw Error('Only an explicitly paused idle Goal may be persisted');
            window.__goalTest.paused_from = window.__goalTest.status;
            window.__goalTest.status = 'paused';
          }
          if (body.action === 'resume') {
            if (body.userInitiated !== true || window.__goalTest.status !== 'paused' ||
                !['ready','awaiting_approval'].includes(window.__goalTest.paused_from))
              throw Error('Resume requires the saved user-paused gate');
            window.__goalTest.status = window.__goalTest.paused_from;
            window.__goalTest.paused_from = null;
          }
          if (body.action === 'begin') {
            if (window.__goalTest.status !== 'ready' ||
                (window.__goalTest.spec.steps[window.__goalTest.current_step].requires_approval &&
                 !window.__goalTest.approved_for_step))
              throw Error('Step needs separate explicit approval before beginning');
            window.__goalTest.status = 'in_turn';
            window.__goalTest.active_turn = { id: 'synthetic-ticket', turn_number: 1 };
            window.__goalTest.turns_used++;
          }
          if (body.action === 'finish') {
            if (window.__goalTest.status !== 'in_turn' ||
                body.turnId !== window.__goalTest.active_turn?.id)
              throw Error('Ticket mismatch');
            window.__goalTest.current_step++;
            window.__goalTest.active_turn = null;
            window.__goalTest.approved_for_step = false;
            if (window.__goalTest.current_step >= window.__goalTest.spec.steps.length) {
              window.__goalTest.status = 'completed';
            } else if (window.__goalTest.spec.steps[window.__goalTest.current_step].requires_approval) {
              window.__goalTest.status = 'awaiting_approval';
              window.__goalTest.approval_reason = 'Step requires manual approval';
            } else {
              window.__goalTest.status = 'ready';
              window.__goalTest.approval_reason = null;
            }
          }
          if (body.action === 'interrupt') {
            if (window.__goalTest?.status !== 'in_turn' ||
                body.turnId !== window.__goalTest.active_turn?.id)
              throw Error('Explicit interruption must match active Goal ticket');
            window.__goalTest.status = 'ready';
            window.__goalTest.active_turn = null;
          }
          if (body.action === 'archive') {
            window.__goalTest = null;
            return { archived: true };
          }
          if (body.action === 'cancel') {
            if (!window.__goalTest || ['completed','failed','rejected','cancelled'].includes(window.__goalTest.status))
              throw Error('Only active checkpoint can be explicitly cancelled');
            window.__goalTest.status = 'cancelled';
            window.__goalTest.active_turn = null;
          }
          return { goal: structuredClone(window.__goalTest) };
        }
        if (route === '/api/goal-review') {
          if (!body || Object.keys(body).length) throw Error('Goal Review is read-only');
          window.__goalReviewQueries++;
          if (window.__goalReviewMode === 'pending')
            await new Promise(resolve => { window.__goalReviewRelease = resolve; });
          if (window.__goalReviewMode === 'error') throw Error('Synthetic private checkpoint unavailable');
          if (window.__goalReviewMode === 'empty') return {goal:null,provenance:null};
          return structuredClone(window.__goalReviewReceipt || {goal:null,provenance:null});
        }
        if (route === '/api/mobile-goal-run') {
          if (!body.userInitiated || body.provider !== 'openai' ||
              body.model !== 'synthetic-model' || !/^[0-9a-f-]{36}$/.test(body.requestId) ||
              typeof body.delegateToSubagent !== 'boolean' || typeof body.allowMcpTools !== 'boolean' ||
              (body.delegateToSubagent && body.allowMcpTools) ||
              (body.allowMcpTools && !window.__mcpTestConnected) ||
              window.__goalTest?.status !== 'ready')
            throw Error('An explicitly authorized ready-goal provider call is required');
          window.__goalRunCalls.push({...body});
          window.__mcpTestCalls.push('goal:ai-run');
          if (body.allowMcpTools && ['mcp','mcp-invalid','mcp-error'].includes(window.__goalRunMode)) {
            window.__goalTest.status = 'in_turn';
            window.__goalTest.turns_used++;
            window.__goalTest.active_turn = {
              id:'9da3ca10-8d3a-4c0c-9f01-44671a5a2904',
              turn_number:window.__goalTest.turns_used,
            };
            const review = {
              reviewId:'af248a31-379e-4861-8bc7-67762c9773d9',
              requestId:body.requestId,goalId:window.__goalTest.id,
              turnId:window.__goalTest.active_turn.id,
              name:'test_read',
              arguments:{query:'<img src=x onerror=alert(1)>',secret:'SYNTHETIC_GOAL_MCP_ARGS_VISIBLE'},
              expiresInSeconds:120,
            };
            window.__goalMcpPending = structuredClone(review);
            if (window.__goalRunMode === 'mcp-invalid') review.requestId = 'cafebabe-0000-4000-8000-000000000001';
            if (window.__goalRunMode === 'mcp-error') throw Error('Synthetic Goal MCP provider timeout');
            return {
              goal:structuredClone(window.__goalTest),
              ticket:structuredClone(window.__goalTest.active_turn),
              requestId:body.requestId,answer:null,awaitingUserOutcome:false,
              pendingGoalTool:review,modelToolsExecuted:false,savedAsComplete:false,
              delegatedToSubagent:false,delegateJobId:null,delegatePersisted:false,
            };
          }
          if (body.allowMcpTools && window.__goalRunMode === 'mcp-text') {
            window.__goalTest.status = 'in_turn';
            window.__goalTest.turns_used++;
            window.__goalTest.active_turn = {id:'synthetic-ai-ticket',
              turn_number:window.__goalTest.turns_used};
            return {
              goal:structuredClone(window.__goalTest),
              ticket:structuredClone(window.__goalTest.active_turn),
              answer:'Synthetic text-only provider answer with MCP permission; NO tool proposed.',
              awaitingUserOutcome:true,delegatedToSubagent:false,
              delegateJobId:null,delegatePersisted:false,
              modelToolsExecuted:false,savedAsComplete:false,
            };
          }
          if (window.__goalRunMode === 'capacity' && body.delegateToSubagent)
            return {error:'Native AI subagent capacity full; no Goal turn was started'};
          if (window.__goalRunMode === 'error') {
            window.__goalTest.turns_used += 1;
            window.__goalTest.status = 'ready'; // persisted synthetic retryable outcome
            return {error:'Synthetic provider failure; retryable checkpoint recorded'};
          }
          if (window.__goalRunMode === 'pending') {
            await new Promise(resolve => {window.__goalRunResolve = resolve;});
          }
          if (window.__goalRunMode === 'cancelled') {
            window.__goalTest.turns_used += 1;
            window.__goalTest.status = 'ready';
            return {error:'Synthetic Goal AI request cancelled; persisted as interrupted retry'};
          }
          window.__goalTest.status = 'in_turn';
          window.__goalTest.turns_used += 1;
          window.__goalTest.active_turn = {id:'synthetic-ai-ticket',
            turn_number:window.__goalTest.turns_used};
          const jobId = body.delegateToSubagent ? 'eed810da-96fd-492d-9761-18e3155a1892' : null;
          if (jobId) window.__delegateJobs.push({id:jobId,status:'completed',
            provider:body.provider,model:body.model,task:'Synthetic reviewed Goal step',
            answer:'Synthetic live provider answer for the current goal; NOT independently verified.',error:null});
          return {goal:structuredClone(window.__goalTest),
            ticket:structuredClone(window.__goalTest.active_turn),
            answer:'Synthetic live provider answer for the current goal; NOT independently verified.',
            awaitingUserOutcome:true,delegatedToSubagent:body.delegateToSubagent,
            delegateJobId:jobId,delegatePersisted:body.delegateToSubagent,
            modelToolsExecuted:false,savedAsComplete:false};
        }
        if (route === '/api/goal-tool-review') {
          window.__goalMcpReviewCalls.push({...body});
          const p = window.__goalMcpPending;
          if (!p || body.userInitiated !== true ||
              !['approve','reject'].includes(body.action) ||
              body.reviewId !== p.reviewId || body.requestId !== p.requestId ||
              body.goalId !== p.goalId || body.turnId !== p.turnId ||
              window.__goalTest?.status !== 'in_turn' ||
              window.__goalTest.active_turn?.id !== body.turnId)
            throw Error('Synthetic Goal MCP review identity/gesture mismatch');
          if (window.__goalMcpReviewMode === 'pending')
            await new Promise(resolve => {window.__goalMcpReviewRelease = resolve;});
          if (window.__goalMcpReviewMode === 'error') throw Error('Synthetic review failure');
          if (window.__goalMcpReviewMode === 'uncertain') {
            // Backend has crossed durable dispatch_uncertain. The remote
            // side effect is UNKNOWN, never a safe automatic retry.
            window.__goalTest.status = 'awaiting_approval';
            window.__goalTest.active_turn = null;
            window.__goalTest.approval_reason = 'Tool dispatch uncertain: explicit human reconciliation required';
            window.__goalMcpPending = null;
            throw Error('MCP dispatch uncertain; review the remote side effect manually');
          }
          if (body.action === 'reject') {
            window.__goalTest.status = 'ready';
            window.__goalTest.active_turn = null;
            window.__goalMcpPending = null;
            return {rejected:true,executed:false,goal:structuredClone(window.__goalTest)};
          }
          // Test-only model of backend: only explicit matching approve executes.
          if (window.__goalMcpReviewMode !== 'wrong-id') window.__goalMcpExecuted++;
          window.__goalMcpPending = null;
          return {
            goal:structuredClone(window.__goalTest),
            ticket:structuredClone(window.__goalTest.active_turn),
            answer:'Synthetic MCP-assisted provider answer; NOT a verified Goal step.',
            reviewId:window.__goalMcpReviewMode === 'wrong-id'
              ? 'ad248a31-379e-4861-8bc7-67762c9773d9' : p.reviewId,
            requestId:body.requestId,awaitingUserOutcome:true,modelToolsExecuted:true,
            toolExecutionConfirmed:true,savedAsComplete:false,
          };
        }
        if (route === '/api/mobile-goal-draft') {
          if (!body.userInitiated || body.provider !== 'openai' ||
              body.model !== 'synthetic-model' ||
              !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(body.requestId) ||
              !body.objective || new TextEncoder().encode(body.objective).length > 1000 ||
              /[\x00-\x1f\x7f]/u.test(body.objective) || window.__goalTest)
            throw Error('Unapproved or invalid synthetic provider goal-draft request');
          window.__goalDraftCalls.push(structuredClone(body));
          if (window.__goalDraftMode === 'error')
            throw Error('Synthetic goal draft provider failure');
          if (window.__goalDraftMode === 'pending')
            await new Promise(resolve => {window.__goalDraftRelease = resolve;});
          const draft = {title:'Synthetic drafted goal',
            steps:[{description:'Inspect synthetic inputs',requires_approval:true},
              {description:'Review evidence',requires_approval:false},
              {description:'Report independently verified outcome',requires_approval:true}],
            max_turns:5,max_retries_per_step:4};
          if (window.__goalDraftMode === 'malformed') draft.steps = [{description:'\nInvalid injected step',requires_approval:false}];
          if (window.__goalDraftMode === 'approval-required') draft.steps[0].requires_approval = true;
          return {draft,requestId:window.__goalDraftMode === 'wrong-id' ? 'wrong-request' : body.requestId,
            requiresReview:true,goalCreated:false,turnStarted:false,
            toolsExecuted:window.__goalDraftMode === 'tool-used'};
        }
        if (route === '/api/mcp-audit') {
          window.__mcpAuditQueries++;
          if (body && Object.keys(body).length) throw Error('MCP audit is metadata-only');
          return {events:structuredClone(window.__mcpAuditEvents),
            meaning:'Approval and remote response metadata; no arguments or tool results stored'};
        }
        if (route === '/api/mobile-projects') {
          window.__mcpTestCalls.push('project:' + body.action);
          const catalog = window.__nativeProjectsQA;
          if (body.action === 'review-import') return {
            reviewId: 'synthetic-review-1', needsExplicitApproval: true,
            preview: {projects_to_add: 1, assignments_to_add: 0, conflicts: [], unknown_chats: [],
              incoming_projects: [{id:'project-qa',name:'Reviewed local label'}]},
          };
          if (body.action === 'create') catalog.projects.push({
            id: 'f2a19e15-9921-4fc7-87a5-a02c3f38cb4f', name: body.name,
          });
          if (body.action === 'rename') {
            const project = catalog.projects.find(p => p.id === body.id);
            if (project) project.name = body.name;
          }
          if (body.action === 'remove') catalog.projects =
            catalog.projects.filter(p => p.id !== body.id);
          if (body.action === 'assign') {
            if (body.projectId) catalog.assignments[body.chatId] = body.projectId;
            else delete catalog.assignments[body.chatId];
          }
          return { catalog: structuredClone(catalog), legacyLocalStoragePreserved: true };
        }
        if (route === '/api/session-export') {
          window.__mcpTestCalls.push('export');
          return { filename: 'conversation-qa.md', content: '# Mcp Agent Ai conversation\n\nProvider: openai\nModel: qa-model\n\n## User\n\nSynthetic review export' };
        }
        if (route === '/api/chat') {
          window.__mcpTestCalls.push('chat:' + (body.requestId ? 'uuid' : 'missing-id'));
          if (body.allowMcpTools) {
            if (!window.__mcpTestConnected) throw Error('No active synthetic MCP session');
            window.__mcpTestCalls.push('modeltool:proposed');
            window.__pendingToolPrompt = body.prompt;
            return {pendingTool:{reviewId:'9c02899e-174f-433f-aac9-6c7a22f1175d',
              requestId:body.requestId,name:'test_read',arguments:{query:'synthetic'},expiresInSeconds:120},
              answer:null,actions:[],sessionId:null};
          }
          if (window.__toolTestSession && body.sessionId === window.__toolTestSession.id &&
              body.prompt === 'Synthetic follow-up preserving the approved MCP audit') {
            window.__toolTestSession.messages.push(
              {role:'user',text:body.prompt},
              {role:'assistant',text:'Synthetic follow-up saved without another tool call'});
            window.__mcpTestCalls.push('modeltool:followup-saved');
            return {sessionId:window.__toolTestSession.id,
              answer:'Synthetic follow-up saved without another tool call',actions:[]};
          }
          return new Promise(resolve => { window.__cancelTestResolve = resolve; });
        }
        if (route === '/api/chat-tool-review') {
          if (!body.userInitiated || body.reviewId !== '9c02899e-174f-433f-aac9-6c7a22f1175d' ||
              !/^[0-9a-f-]{36}$/.test(body.requestId)) throw Error('Explicit matching tool review required');
          if (body.action === 'reject') {
            window.__mcpTestCalls.push('modeltool:rejected');
            return {rejected:true,executed:false};
          }
          if (body.action === 'approve') {
            if (!window.__mcpTestConnected) throw Error('MCP server disconnected before approval');
            window.__mcpTestCalls.push('modeltool:approved');
            const id = '4efb9b0a-9218-4adb-9d3e-ce5068d29da2';
            window.__toolTestSession = {id,provider:'openai',model:'synthetic-model',
              messages:[{role:'user',text:window.__pendingToolPrompt},
                {role:'assistant',text:'Synthetic model resumed after approved MCP result'}],
              actions:[{type:'mcp_tool_call',name:'test_read',approvedByUser:true,
                resultIsError:false,resultStored:false,
                arguments:{apiKey:'SYNTHETIC_QA_ARGUMENT_SECRET'},
                result:{token:'SYNTHETIC_QA_RESULT_SECRET'}}]};
            return {sessionId:id,answer:'Synthetic model resumed after approved MCP result',
              actions:[{type:'mcp_tool_call',name:'test_read',approvedByUser:true,
                resultIsError:false,resultStored:false}]};
          }
          throw Error('Unsupported synthetic tool review action');
        }
        if (route === '/api/chat-progress') {
          window.__mcpTestCalls.push('progress:' + (body.requestId ? 'uuid' : 'missing-id'));
          return { active: true, streaming: true, text: 'Synthetic streamed chunk' };
        }
        if (route === '/api/chat-cancel') {
          window.__mcpTestCalls.push('cancel:' + (body.requestId ? 'uuid' : 'missing-id'));
          if (window.__goalRunMode === 'pending' &&
              window.__goalRunCalls.at(-1)?.requestId === body.requestId) {
            window.__goalRunMode = 'cancelled';
            window.__goalRunResolve?.();
          }
          window.__cancelTestResolve?.({ error: 'Request cancelled' });
          return { cancelRequested: true };
        }
        throw Error('Unexpected API route: ' + route);
      },
    };
  });
  // Real native session IDs are supplied by the synthetic /api/state snapshot.
  // Exercise A -> B -> A -> New chat through actual sidebar navigation.
  // This fixture never calls a provider or touches a live ChatGPT account.
  await nativeTools.locator('#sidebarToggle').click();
  if (await nativeTools.locator('#chatsToggle').getAttribute('aria-expanded') !== 'true')
    await nativeTools.locator('#chatsToggle').click();
  await nativeTools.locator('[data-session-id="c23ec71e-bfe9-4fe5-b931-48074cf30b2d"]').click();
  await nativeTools.locator('#androidComposeMore').click();
  await nativeTools.locator('#androidStandingRulesText').waitFor({state:'visible'});
  await nativeTools.waitForFunction(() => !document.getElementById('androidStandingRulesText').disabled);
  ensure((await nativeTools.locator('#androidStandingRulesSession').innerText()).includes('c23ec71e') &&
    (await nativeTools.locator('#androidStandingRulesSave').innerText()) === 'Set rules',
    'Selecting an existing chat must resolve real session identity and its editor');
  await nativeTools.locator('#androidStandingRulesText').fill('ONLY FIRST CHAT');
  ensure((await nativeTools.evaluate(() => window.__scopedRulesAndroidCalls)).every(x=>x.action==='get'),
    'Editing this-chat rules must not silently save');
  await nativeTools.locator('#androidStandingRulesSave').click();
  await nativeTools.locator('#androidStandingRulesStatus').getByText('Rules set for this chat only',{exact:false}).waitFor();
  ensure(await nativeTools.evaluate(() =>
    window.__scopedRulesAndroid.sessions['c23ec71e-bfe9-4fe5-b931-48074cf30b2d'].text === 'ONLY FIRST CHAT' &&
    window.__scopedRulesAndroid.global.text === ''),
    'Set rules must affect only the saved chat and never global rules');
  await nativeTools.screenshot({path:path.join(screenshotFolder,'native-android-chat-rules.png')});
  await nativeTools.locator('#androidComposeMore').click();
  await nativeTools.locator('#sidebarToggle').click();
  await nativeTools.locator('[data-session-id="d4be29af-35ca-4191-8ae2-f0377b1da9df"]').click();
  await nativeTools.locator('#androidComposeMore').click();
  await nativeTools.waitForFunction(() => !document.getElementById('androidStandingRulesText').disabled);
  ensure((await nativeTools.locator('#androidStandingRulesSession').innerText()).includes('d4be29af') &&
    (await nativeTools.locator('#androidStandingRulesText').inputValue()) === '' &&
    await nativeTools.locator('#androidStandingRulesReset').isDisabled(),
    'Second chat must not inherit A-only rules');
  await nativeTools.locator('#androidStandingRulesText').fill('UNSAVED SECOND CHAT');
  await nativeTools.locator('#androidComposeMore').click();
  await nativeTools.locator('#sidebarToggle').click();
  await nativeTools.locator('[data-session-id="c23ec71e-bfe9-4fe5-b931-48074cf30b2d"]').click();
  await nativeTools.locator('#androidComposeMore').click();
  await nativeTools.waitForFunction(() => !document.getElementById('androidStandingRulesText').disabled);
  ensure(await nativeTools.locator('#androidStandingRulesText').inputValue() === 'ONLY FIRST CHAT' &&
    await nativeTools.locator('#androidStandingRulesReset').isEnabled(),
    'Returning to first chat must restore its own saved scoped rule');
  // Declined Reset must never mutate storage; accepted Reset clears A alone.
  nativeTools.once('dialog',dialog=>dialog.dismiss());
  await nativeTools.locator('#androidStandingRulesReset').click();
  ensure((await nativeTools.evaluate(() => window.__scopedRulesAndroidCalls.at(-1)?.action)) === 'get',
    'Declining Reset must not invoke a native mutation');
  nativeTools.once('dialog',dialog=>dialog.accept());
  await nativeTools.locator('#androidStandingRulesReset').click();
  await nativeTools.locator('#androidStandingRulesStatus').getByText('Rules reset for this chat only',{exact:false}).waitFor();
  ensure(await nativeTools.evaluate(() =>
    window.__scopedRulesAndroid.sessions['c23ec71e-bfe9-4fe5-b931-48074cf30b2d'] === undefined),
    'Accepted Reset must remove only this session record');
  await nativeTools.locator('#androidComposeMore').click();
  await nativeTools.locator('#sidebarToggle').click();
  await nativeTools.locator('#newChat').click();
  await nativeTools.locator('#androidComposeMore').click();
  ensure((await nativeTools.locator('#androidStandingRulesSession').innerText()) === 'Chat session: None' &&
    await nativeTools.locator('#androidStandingRulesSave').isDisabled() &&
    await nativeTools.locator('#androidStandingRulesReset').isDisabled() &&
    (await nativeTools.locator('#androidStandingRulesText').inputValue()) === '',
    'New unsent chat must show None and cannot inherit/reset another session rules');
  await nativeTools.setViewportSize({width:320,height:568});
  const scopedMenu320=await nativeTools.locator('#androidComposeMoreMenu').boundingBox();
  ensure(!!scopedMenu320 && scopedMenu320.x >= 0 && scopedMenu320.x + scopedMenu320.width <= 321 &&
    scopedMenu320.y >= 0 && scopedMenu320.y + scopedMenu320.height <= 569,
    'Set/Reset rules menu must fit 320px width and height');
  await nativeTools.screenshot({path:path.join(screenshotFolder,'native-android-chat-rules-320.png')});
  await nativeTools.setViewportSize({width:390,height:844});
  await nativeTools.locator('#androidComposeMore').click();
  ensure((await nativeTools.evaluate(() => window.__goalReviewQueries)) === 0,
    'Goal Review must not fetch persisted provenance automatically on chat launch');
  ensure(await nativeTools.locator('#speechDictation').isVisible() &&
    await nativeTools.locator('#speechDictation').isEnabled() &&
    (await nativeTools.evaluate(() => window.__speechCalls.length)) === 0,
    'Dictation must be available without starting speech recognition on app launch');
  await nativeTools.locator('#prompt').fill('Existing draft');
  nativeTools.once('dialog', dialog => dialog.dismiss());
  await nativeTools.locator('#speechDictation').click();
  ensure((await nativeTools.evaluate(() => window.__speechCalls.length)) === 0 &&
    (await nativeTools.locator('#prompt').inputValue()) === 'Existing draft',
    'Refusing speech disclosure must not launch Android recognition or modify draft');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#speechDictation').click();
  await nativeTools.locator('#prompt').getAttribute('value').catch(() => null);
  await nativeTools.waitForFunction(() => document.getElementById('prompt').value.includes('Recognized synthetic speech'));
  ensure((await nativeTools.locator('#prompt').inputValue()) === 'Existing draft Recognized synthetic speech' &&
    JSON.stringify(await nativeTools.evaluate(() => window.__speechCalls)) === '["status","recognize"]' &&
    (await nativeTools.evaluate(() => window.__mcpTestCalls)).every(call => !call.startsWith('chat:')),
    'Real speech receipt must insert into draft without sending chat or touching API provider');
  await nativeTools.evaluate(() => { window.__speechTestMode = 'cancel'; });
  const beforeCancel = await nativeTools.locator('#prompt').inputValue();
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#speechDictation').click();
  await nativeTools.getByText('Dictation cancelled. Draft unchanged.').waitFor();
  ensure((await nativeTools.locator('#prompt').inputValue()) === beforeCancel,
    'Cancelled Android recognizer must not edit or submit draft');
  await nativeTools.evaluate(() => { window.__speechTestMode = 'unavailable'; });
  const countBeforeUnavailable = await nativeTools.evaluate(() => window.__speechCalls.filter(x => x === 'recognize').length);
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#speechDictation').click();
  await nativeTools.getByText('Dictation unavailable:',{exact:false}).waitFor();
  ensure((await nativeTools.evaluate(() => window.__speechCalls.filter(x => x === 'recognize').length)) === countBeforeUnavailable,
    'Unavailable speech service must fail closed before requesting Android recognition');
  await nativeTools.evaluate(() => { window.__speechTestMode = 'deferred'; });
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#speechDictation').click();
  await nativeTools.waitForFunction(() => typeof window.__speechResolve === 'function');
  ensure(await nativeTools.locator('#speechDictation').isDisabled(),
    'In-flight Android recognizer must block a second overlapping invocation');
  await nativeTools.locator('#prompt').fill('User edited while recognition pending');
  await nativeTools.evaluate(() => window.__speechResolve());
  await nativeTools.getByText('Dictation returned after draft/chat changed.',{exact:false}).waitFor();
  ensure((await nativeTools.locator('#prompt').inputValue()) === 'User edited while recognition pending',
    'Stale recognition must not replace a newer user draft');
  await nativeTools.evaluate(() => { window.__speechTestMode = 'forged'; });
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#speechDictation').click();
  await nativeTools.getByText('Dictation unavailable:',{exact:false}).waitFor();
  ensure((await nativeTools.locator('#prompt').inputValue()) === 'User edited while recognition pending',
    'Wrong request-ID speech receipt must never insert or send text');
  await nativeTools.locator('#prompt').fill('');
  await nativeTools.locator('#sidebarToggle').click();
  // The dedicated Plugins Settings page now has an explicit route to
  // independent HTTPS MCP connections; Skills and Pets remain separate.
  await nativeTools.locator('[data-sidebar-extra="plugins"]').click();
  await nativeTools.locator('#androidPluginsOpenExternalMcp').click();
  await nativeTools.waitForFunction(() => window.__mcpTestCalls.includes('status'));
  ensure(await nativeTools.locator('#mobileMcpConnect').isEnabled(),
    'Installed Android plugin panel should allow explicit MCP connection');
  await nativeTools.locator('#mobileMcpEndpoint').fill('https://mcp.vendor.example/mcp');
  await nativeTools.locator('#mobileMcpConnect').click();
  await nativeTools.locator('#mobileMcpToolsList').getByText('test_read').waitFor();
  ensure(await nativeTools.locator('#mobileMcpDisconnect').isEnabled(),
    'Successful MCP connection did not enable Disconnect');
  await nativeTools.locator('#mobileMcpToolsList').getByText('test_read').click();
  await nativeTools.locator('#mobileMcpArgs').fill('{"query":"synthetic"}');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#mobileMcpToolForm button[type="submit"]').click();
  await nativeTools.locator('#mobileMcpResult').getByText('synthetic MCP').waitFor();
  ensure(JSON.stringify(await nativeTools.evaluate(() => window.__mcpTestCalls))
    .includes('"call"'), 'Manual MCP tool confirmation never reached native bridge');
  const mcpAiAccess = {
    modelToolDisabled:await nativeTools.locator('#mobileMcpModelOptIn').isDisabled(),
    modelToolHidden:await nativeTools.locator('#mobileMcpModelOptIn').isHidden(),
    provider:await nativeTools.locator('#provider').inputValue(),
    options:await nativeTools.locator('#provider option').evaluateAll(options =>
      options.map(option => option.value)),
    chatApiCalls:await nativeTools.evaluate(() =>
      window.__mcpTestCalls.filter(call => call.startsWith('chat:')).length),
  };
  ensure((mcpAiAccess.modelToolDisabled || mcpAiAccess.modelToolHidden) &&
    mcpAiAccess.provider === 'chatgpt-browser' &&
    mcpAiAccess.options.includes('chatgpt-browser') &&
    // This fixture deliberately advertises ChatGPT alone. Other Android
    // fixtures cover direct API providers; their absence here is intentional.
    mcpAiAccess.options.every(option =>
      ['chatgpt-browser','openai','anthropic','openrouter'].includes(option)) &&
    mcpAiAccess.chatApiCalls === 0,
    'Android ChatGPT-only mode must not expose provider model-tool proposals: ' + JSON.stringify(mcpAiAccess));
  await nativeTools.locator('#extrasClose').click();
  // A persisted, metadata-only ChatGPT tool receipt may still be reviewed;
  // never dispatch an OpenAI model request to generate it in Android QA.
  await nativeTools.evaluate(() => {
    window.__toolTestSession = {
      id:'4efb9b0a-9218-4adb-9d3e-ce5068d29da2',
      provider:'chatgpt-browser',model:'chatgpt.com',
      messages:[{role:'user',text:'Synthetic audited ChatGPT tool request'},
        {role:'assistant',text:'Synthetic ChatGPT tool evidence stored'}],
      actions:[{type:'mcp_tool_call',name:'test_read',approvedByUser:true,
        resultIsError:false,resultStored:false,
        arguments:{apiKey:'SYNTHETIC_QA_ARGUMENT_SECRET'},
        result:{token:'SYNTHETIC_QA_RESULT_SECRET'}}],
    };
  });
  await nativeTools.evaluate(() => {
    const statuses = ['approved_dispatched','server_confirmed','server_reported_error','answer_saved'];
    const names = ['test_dispatch','test_confirmed','test_error','test_read'];
    window.__mcpAuditEvents = statuses.map((status,index) => ({
      reviewId:`00000000-0000-4000-8000-${String(index+1).padStart(12,'0')}`,
      requestId:`00000000-0000-4000-9000-${String(index+1).padStart(12,'0')}`,
      sessionId:index === 3 ? window.__toolTestSession.id : null,
      name:names[index],status,atUnixMs:1770000000000 + index,
      approvedByUser:true,resultStored:false,
      arguments:{token:'SYNTHETIC_AUDIT_ARGUMENT_SECRET'},
      result:{secret:'SYNTHETIC_AUDIT_RESULT_SECRET'},
    }));
  });
  // Native Review displays persisted state, not a transient test fixture.
  // Reload the synthetic state after the audited receipt is saved.
  await nativeTools.locator('#chatsRefresh').evaluate(button => button.click());
  await nativeTools.waitForFunction(() => document.getElementById('threadCount').textContent === '3');
  await nativeTools.locator('#toolPanelToggle').click();
  await nativeTools.locator('[data-tool-view="review"]').click();
  await nativeTools.locator('#reviewSummary').getByText('4 approval record(s)', {exact:false}).waitFor();
  await nativeTools.locator('#reviewGoalProvenanceState')
    .getByText('No saved Goal checkpoint', {exact:false}).waitFor();
  ensure((await nativeTools.evaluate(() => window.__goalReviewQueries)) === 1 &&
    (await nativeTools.locator('#reviewGoalProvenanceDetails').innerText()) === '',
    'Explicit Android Review open with no saved Goal must show empty provenance without invented events');
  const auditText = await nativeTools.locator('#reviewSummary').innerText();
  ensure(auditText.includes('1 unconfirmed · 1 server responses · 1 server errors · 1 answers saved') &&
    auditText.includes('test_dispatch · Approved dispatch; server execution NOT confirmed') &&
    auditText.includes('test_confirmed · MCP server response confirmed; final answer not yet saved') &&
    auditText.includes('test_error · MCP server reported error; side effects may still have occurred') &&
    auditText.includes('test_read · Server response confirmed; final AI answer saved'),
    'Review MCP audit must distinguish uncertain dispatch, remote response, remote error and saved continuation');
  ensure(!auditText.includes('SYNTHETIC_AUDIT_ARGUMENT_SECRET') &&
    !auditText.includes('SYNTHETIC_AUDIT_RESULT_SECRET'),
    'Metadata-only MCP audit must never reveal arguments or tool results');
  const approvedEvidence = nativeTools.locator('.review-mcp-evidence');
  ensure(await approvedEvidence.count() === 1 &&
    (await approvedEvidence.innerText()).includes('1 recorded user-approved MCP model tool call · test_read'),
    'Review must identify only the real persisted user-approved MCP model tool receipt');
  ensure(await nativeTools.locator('#reviewSummary .review-export-row').filter({hasText:'Synthetic review export'})
    .locator('.review-mcp-evidence').count() === 0,
    'Review must not invent MCP activity for sessions without persisted approved actions');
  ensure(!(await nativeTools.locator('#reviewSummary').innerText()).includes('SYNTHETIC_QA_ARGUMENT_SECRET') &&
    !(await nativeTools.locator('#reviewSummary').innerText()).includes('SYNTHETIC_QA_RESULT_SECRET'),
    'Review must never expose raw MCP arguments or results from Session.actions');
  ensure((await nativeTools.locator('#mobileMcpModelOptIn').isDisabled() ||
      await nativeTools.locator('#mobileMcpModelOptIn').isHidden()) &&
    (await nativeTools.evaluate(() => window.__mcpTestCalls)).every(call => !call.startsWith('chat:')),
    'Reopening Android MCP settings must not enable direct provider model-tool execution');
  await nativeTools.locator('#toolViewClose').click();
  await nativeTools.locator('#sidebarToggle').click();
  await nativeTools.locator('[data-sidebar-extra="plugins"]').click();
  await nativeTools.locator('#androidPluginsOpenExternalMcp').click();
  await nativeTools.locator('#mobileMcpDisconnect').click();
  ensure(await nativeTools.locator('#mobileMcpConnect').isEnabled() &&
    !(await nativeTools.locator('#mobileMcpDisconnect').isEnabled()),
    'MCP disconnect did not restore disconnected state');
  await nativeTools.locator('#extrasClose').click();
  await nativeTools.locator('#toolPanelToggle').click();
  await nativeTools.locator('[data-tool-view="subagents"]').click();
  await nativeTools.locator('#androidPrimeAdvanced').evaluate(node => { node.open = true; });
  await nativeTools.locator('#localWorkerStats').getByText('Running 0 · Queued 0').waitFor();
  await nativeTools.locator('#mobilePrimeFeedback').getByText('Prime ready.',{exact:false}).waitFor();
  ensure(await nativeTools.locator('#mobilePrimeWorkerForm button[type="submit"]').isDisabled() &&
    (await nativeTools.evaluate(() => window.__primeTestCalls.every(call => call.action === 'status'))),
    'Prime cannot auto-spawn a worker with ChatGPT-only provider or on panel open');
  await nativeTools.locator('#mobilePrimeObjective').fill('Coordinate a bounded Android project review');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#mobilePrimeCreateForm button[type="submit"]').click();
  await nativeTools.locator('#mobilePrimeState').getByText('Prime active',{exact:false}).waitFor();
  ensure(await nativeTools.locator('#mobilePrimeWorkerForm button[type="submit"]').isDisabled() &&
    (await nativeTools.evaluate(() => window.__primeTestCalls.filter(call => call.action === 'start').length)) === 1 &&
    (await nativeTools.evaluate(() => window.__primeTestCalls.filter(call => call.action === 'spawn').length)) === 0,
    'Prime start must create only saved coordination metadata, no unauthorized model call');
  await nativeTools.locator('#mobilePrimeSummary').fill('User checked the synthetic review state. No AI worker reports were run.');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#mobilePrimeFinish').click();
  await nativeTools.locator('#mobilePrimeState').getByText('Prime finished',{exact:false}).waitFor();
  ensure((await nativeTools.evaluate(() => window.__primeTestTask.status)) === 'finished' &&
    (await nativeTools.evaluate(() => window.__primeTestCalls.filter(call => call.action === 'finish').length)) === 1,
    'Prime Finish needs a human summary and an explicit native terminal receipt');
  await nativeTools.locator('#localWorkerAction').selectOption('search_names');
  await nativeTools.locator('#localWorkerTarget').fill('synthetic');
  await nativeTools.locator('#localWorkerForm').evaluate(element => element.scrollIntoView({block:'center'}));
  ensure((await nativeTools.evaluate(() => getComputedStyle(document.getElementById('toolSubagents')).justifyContent)) === 'flex-start',
    'Tall Android Sub-agents section must scroll from the top rather than clipping behind the header');
  await nativeTools.locator('#localWorkerForm button[type="submit"]').click();
  await nativeTools.locator('#localWorkerHistory').getByText('completed · 100%').waitFor();
  ensure(JSON.stringify(await nativeTools.evaluate(() => window.__mcpTestCalls))
    .includes('"worker:submit"'), 'Local worker submit not dispatched through real Tauri API route');
  // Android ChatGPT-only deliberately retires direct provider AI delegates,
  // but retains the bounded local worker and manual Goal checkpoint tools.
  ensure(await nativeTools.locator('#mobileAiDelegates').isHidden() &&
    await nativeTools.locator('#mobileAiDelegateForm button[type="submit"]').isDisabled() &&
    (await nativeTools.evaluate(() => window.__delegateCalls)).every(call => call.action === 'status') &&
    await nativeTools.locator('#localWorkerForm').isVisible(),
    'Android must hide direct AI delegate forms without disabling the local workspace worker');
  await nativeTools.locator('#toolViewClose').click();
  await nativeTools.locator('#sidebarToggle').click();
  await nativeTools.locator('#newChat').click();
  await nativeTools.locator('#prompt').fill('Build a synthetic review plan');
  const beforePlanChat = await nativeTools.evaluate(() =>
    window.__mcpTestCalls.filter(call => call.startsWith('chat:')).length);
  await nativeTools.locator('#androidComposePlan').click();
  ensure(await nativeTools.locator('#androidPlanEditor').isVisible() &&
    (await nativeTools.locator('#androidPlanGenerate').isDisabled() ||
      await nativeTools.locator('#androidPlanGenerate').isHidden()) &&
    (await nativeTools.evaluate(() => window.__goalDraftCalls.length)) === 0,
    'Plan editor stays usable manually while direct provider AI drafting is disabled');
  await nativeTools.locator('#androidPlanSteps').fill('Inspect synthetic inputs\nReview evidence\nReport independently verified outcome');
  await nativeTools.locator('#androidPlanUseDraft').click();
  ensure(await nativeTools.locator('#androidPlanEditor').isHidden() &&
    (await nativeTools.locator('#prompt').inputValue()).includes('Inspect synthetic inputs') &&
    (await nativeTools.evaluate(() => window.__mcpTestCalls.filter(call => call.startsWith('chat:')).length)) === beforePlanChat,
    'Manual Plan steps must become an editable unsent prompt without a provider request');
  await nativeTools.locator('#prompt').fill('Review a synthetic Android Goal before Create');
  await nativeTools.locator('#androidComposeMode').click();
  await nativeTools.locator('[data-compose-mode="goal"]').click();
  await nativeTools.locator('#send').click();
  await nativeTools.locator('#mobileGoalCreate').waitFor({state:'visible'});
  ensure(await nativeTools.locator('#mobileGoalDraftObjective').inputValue() ===
      'Review a synthetic Android Goal before Create' &&
    (await nativeTools.evaluate(() => window.__goalTest == null)) &&
    (await nativeTools.evaluate(() => window.__goalDraftCalls.length)) === 0,
    'Goal-mode Send must enter manual Goal review, not call a provider or create a Goal');
  await nativeTools.locator('#toolViewClose').click();
  await nativeTools.locator('#androidComposeMode').click();
  await nativeTools.locator('[data-compose-mode="normal"]').click();
  await nativeTools.locator('#prompt').fill('');
  await nativeTools.locator('#toolPanelToggle').click();
  await nativeTools.locator('[data-tool-view="subagents"]').click();
  // Direct AI-generated Goal drafts have been removed on Android. Manual
  // Goal planning, approval flags and checkpoint execution remain available.
  ensure(await nativeTools.locator('#mobileGoalDraftControls').isHidden() &&
    await nativeTools.locator('#mobileGoalDraftButton').isDisabled() &&
    (await nativeTools.evaluate(() => window.__goalDraftCalls.length)) === 0,
    'ChatGPT-only Android cannot draft a Goal through a local/direct API provider');
  await nativeTools.locator('#mobileGoalTitle').fill('Edited by user after manual review');
  await nativeTools.locator('#mobileGoalSteps').fill('Inspect synthetic inputs\nReview evidence\nReport independently verified outcome');
  await nativeTools.locator('#mobileGoalTurns').fill('5');
  await nativeTools.locator('#mobileGoalRetries').fill('4');
  const reviewFlags = async () => nativeTools.locator('[data-goal-step-approval]')
    .evaluateAll(inputs => inputs.map(input => input.checked));
  ensure(JSON.stringify(await reviewFlags()) === '[false,false,false]',
    'Manual Goal steps must default to no implicit approval');
  await nativeTools.locator('[data-goal-step-approval="0"]').check();
  await nativeTools.locator('[data-goal-step-approval="2"]').check();
  ensure(JSON.stringify(await reviewFlags()) === '[true,false,true]' &&
    (await nativeTools.locator('#mobileGoalApprovalSummary').innerText()).includes('2 require explicit approval'),
    'Manual Goal reviewer can explicitly mark only the approval-gated steps');
  const stepA = 'Inspect synthetic inputs';
  const stepB = 'Review evidence';
  const stepC = 'Report independently verified outcome';
  await nativeTools.locator('#mobileGoalSteps').fill([stepC,stepA,stepB].join('\n'));
  ensure(JSON.stringify(await reviewFlags()) === '[true,true,false]',
    'Reordering UNIQUE unchanged steps must retain the exact approval flag for each description');
  await nativeTools.locator('#mobileGoalSteps').fill([stepC,stepA + ' edited',stepB].join('\n'));
  ensure(JSON.stringify(await reviewFlags()) === '[true,false,false]',
    'Editing a description must invalidate its previous approval flag');
  await nativeTools.locator('#mobileGoalSteps').fill([stepC,stepA,stepB].join('\n'));
  ensure(JSON.stringify(await reviewFlags()) === '[true,false,false]',
    'Restoring an edited description must not resurrect a stale approval');
  await nativeTools.locator('#mobileGoalSteps').fill([stepC,stepC,stepA,stepB].join('\n'));
  ensure(JSON.stringify(await reviewFlags()) === '[false,false,false,false]' &&
    (await nativeTools.locator('#mobileGoalApprovalSummary').innerText()).includes('ambiguous'),
    'Ambiguous duplicated descriptions must never inherit approval from an earlier unique step');
  await nativeTools.locator('#mobileGoalSteps').fill([stepC,stepA,stepB].join('\n'));
  ensure(JSON.stringify(await reviewFlags()) === '[false,false,false]',
    'Removing a duplicate cannot silently restore approvals lost to ambiguity');
  await nativeTools.locator('[data-goal-step-approval="0"]').check();
  await nativeTools.locator('[data-goal-step-approval="1"]').check();
  ensure(JSON.stringify(await reviewFlags()) === '[true,true,false]',
    'User can explicitly reapprove individual reviewed steps after edits or reorder');
  for (const width of [320,360,390]) {
    await nativeTools.setViewportSize({width,height:844});
    const rect = await nativeTools.evaluate(() => {
      const element = document.getElementById('mobileGoalDraftControls');
      const reviews = document.getElementById('mobileGoalApprovalReview');
      const retries = document.getElementById('mobileGoalRetryControls');
      return {width:innerWidth,scrollWidth:document.documentElement.scrollWidth,
        draftHidden:getComputedStyle(element).display === 'none',
        review:reviews.getBoundingClientRect().toJSON(),
        retry:retries.getBoundingClientRect().toJSON()};
    });
    ensure(rect.scrollWidth <= rect.width + 1 && rect.draftHidden &&
      rect.review.left >= -1 && rect.review.right <= rect.width + 1 &&
      rect.retry.left >= -1 && rect.retry.right <= rect.width + 1,
      `Manual Goal approval/retry controls overflow at Android ${width}px`);
  }
  ensure((await nativeTools.evaluate(() => window.__mcpTestCalls.filter(call => call.startsWith('goal:'))))
    .every(call => call === 'goal:status') &&
    (await nativeTools.evaluate(() => window.__goalTest == null)),
    'Reordering and manually checking draft steps must not create or begin the Goal');
  await nativeTools.locator('#mobileGoalCreate button[type="submit"]').click();
  await nativeTools.locator('#mobileGoalProgress').getByText('awaiting approval · 0/3').waitFor();
  const approvedSpec = await nativeTools.evaluate(() => window.__goalTest.spec);
  ensure(approvedSpec.title === 'Edited by user after manual review' &&
    approvedSpec.max_turns === 5 && approvedSpec.max_retries_per_step === 4 &&
    JSON.stringify(approvedSpec.steps.map(step => [step.description,step.requires_approval])) ===
      JSON.stringify([[stepC,true],[stepA,true],[stepB,false]]) &&
    await nativeTools.locator('#mobileGoalBegin').isDisabled() &&
    (await nativeTools.evaluate(() => window.__goalRunCalls.length)) === 0,
    'EXPLICIT Create must submit reordered exact true/false flags and retry budget, initially blocking an unapproved step');
  await nativeTools.locator('[data-goal-approval="true"]').click();
  await nativeTools.locator('#mobileGoalProgress').getByText('ready · 0/3').waitFor();
  ensure((await nativeTools.evaluate(() => window.__mcpTestCalls)).includes('goal:approval') &&
    await nativeTools.locator('#mobileGoalBegin').isEnabled(),
    'User must separately approve the first required step before manual Begin');
  await nativeTools.locator('#mobileGoalBegin').click();
  await nativeTools.locator('#mobileGoalEvidence').fill('User verified first approval-gated step');
  await nativeTools.locator('[data-goal-outcome="completed"]').click();
  await nativeTools.locator('#mobileGoalProgress').getByText('awaiting approval · 1/3').waitFor();
  ensure(await nativeTools.locator('#mobileGoalBegin').isDisabled(),
    'Completing one step must gate the NEXT approval-required step before another Begin');
  await nativeTools.locator('[data-goal-approval="true"]').click();
  await nativeTools.locator('#mobileGoalProgress').getByText('ready · 1/3').waitFor();
  await nativeTools.locator('#mobileGoalBegin').click();
  await nativeTools.locator('#mobileGoalEvidence').fill('User verified second gated step');
  await nativeTools.locator('[data-goal-outcome="completed"]').click();
  await nativeTools.locator('#mobileGoalProgress').getByText('ready · 2/3').waitFor();
  await nativeTools.locator('#mobileGoalBegin').click();
  await nativeTools.locator('#mobileGoalEvidence').fill('User verified last ungated step');
  await nativeTools.locator('[data-goal-outcome="completed"]').click();
  await nativeTools.locator('#mobileGoalProgress').getByText('completed · 3/3').waitFor();
  await nativeTools.evaluate(() => {
    const goal = window.__goalTest;
    const ticket = [0,1,2].map(i =>
      '1884eab' + i + '-4eee-4a04-8a11-00000000000' + (i+1));
    const events = [
      ...ticket.map((id,index) => ({
        type:'turn',kind:'user_attested_completed',stepIndex:index,
        attempt:1,turnNumber:index+1,ticketId:id,independentlyVerified:false,
      })),
      ...[0,1].map(stepIndex => ({
        type:'manual_approval',kind:'approved',stepIndex,
        attempt:null,turnNumber:null,ticketId:null,independentlyVerified:false,
      })),
    ];
    window.__goalReviewReceipt = {
      goalId:goal.id,
      provenance:{
        goalId:goal.id,revision:9,status:'completed',pausedFrom:null,
        currentStep:3,totalSteps:3,turnsUsed:3,turnBudget:goal.spec.max_turns,
        turnsRemaining:goal.spec.max_turns-3,retriesUsedOnStep:0,
        historyEventCount:3,approvalEventCount:2,independentlyVerified:false,
        crossStreamChronologyKnown:false,
        eventOrder:'turn_history_then_approval_history',events,
      },
    };
    window.__goalReviewMode = 'success';
  });
  const reviewCountBeforeGoal = await nativeTools.evaluate(() => window.__goalReviewQueries);
  ensure((await nativeTools.evaluate(() => window.__goalReviewQueries)) === reviewCountBeforeGoal,
    'Completing a Goal must not automatically request Review provenance');
  await nativeTools.locator('[data-work-tab="review"]').click();
  await nativeTools.locator('#reviewGoalProvenanceState')
    .getByText('Saved checkpoint metadata only', {exact:false}).waitFor();
  const goalReviewText = await nativeTools.locator('#reviewGoalProvenance').innerText();
  ensure(goalReviewText.includes('3 recorded turn events · 2 manual approval events') &&
    goalReviewText.includes('Current step 3/3') &&
    goalReviewText.includes('User-attested completed (not independently verified)') &&
    goalReviewText.includes('Manual approval · approved') &&
    goalReviewText.includes('not a cross-stream chronological timeline') &&
    (await nativeTools.locator('#reviewGoalProvenanceDetails .local-worker-job').count()) === 6,
    'Review must render the true bounded event kinds and distinguish manual, caller-attested and independent evidence');
  ensure((await nativeTools.evaluate(() => window.__goalReviewQueries)) === reviewCountBeforeGoal + 1 &&
    !(await nativeTools.evaluate(() => window.__goalRunCalls)).some(call => call.action === 'begin'),
    'Only explicitly opening Review may read Goal provenance; no AI run starts');
  for (const width of [320,360,390]) {
    await nativeTools.setViewportSize({width,height:844});
    const box = await nativeTools.evaluate(() => ({
      width:innerWidth,scrollWidth:document.documentElement.scrollWidth,
      bounds:document.getElementById('reviewGoalProvenance').getBoundingClientRect().toJSON(),
    }));
    ensure(box.scrollWidth <= box.width + 1 && box.bounds.left >= -1 &&
      box.bounds.right <= box.width + 1,
      `Goal Review provenance overflow at Android ${width}px`);
  }
  await nativeTools.evaluate(() => {
    window.__goalReviewReceipt.provenance.events[0].detail =
      '<img src=x onerror=alert("SECRET_GOAL_REVIEW_HTML")>';
    window.__goalReviewReceipt.provenance.events[0].userNote = 'SECRET_GOAL_REVIEW_NOTE';
  });
  await nativeTools.locator('[data-work-tab="review"]').click();
  await nativeTools.locator('#reviewGoalProvenanceState')
    .getByText('unavailable or stale', {exact:false}).waitFor();
  ensure(!(await nativeTools.locator('#reviewGoalProvenance').innerText()).includes('SECRET_GOAL_REVIEW') &&
    (await nativeTools.locator('#reviewGoalProvenanceDetails').innerText()) === '' &&
    await nativeTools.locator('#reviewGoalProvenance img').count() === 0,
    'Malformed extra secret fields and HTML must fail closed with no detail, note, image or partial evidence');
  await nativeTools.evaluate(() => {
    delete window.__goalReviewReceipt.provenance.events[0].detail;
    delete window.__goalReviewReceipt.provenance.events[0].userNote;
    window.__goalReviewReceipt.provenance.events[0].independentlyVerified = true;
  });
  await nativeTools.locator('[data-work-tab="review"]').click();
  await nativeTools.locator('#reviewGoalProvenanceState')
    .getByText('unavailable or stale', {exact:false}).waitFor();
  ensure((await nativeTools.locator('#reviewGoalProvenanceDetails').innerText()) === '',
    'Review cannot accept a false independent-verification claim');
  await nativeTools.evaluate(() => {
    window.__goalReviewReceipt.provenance.events[0].independentlyVerified = false;
    window.__goalReviewReceipt.provenance.events = [
      ...window.__goalReviewReceipt.provenance.events,
      ...Array(145).fill({type:'turn',kind:'needs_approval',stepIndex:0,
        attempt:1,turnNumber:1,ticketId:'1884eab0-4eee-4a04-8a11-000000000001',
        independentlyVerified:false}),
    ];
  });
  await nativeTools.locator('[data-work-tab="review"]').click();
  await nativeTools.locator('#reviewGoalProvenanceState')
    .getByText('unavailable or stale', {exact:false}).waitFor();
  ensure((await nativeTools.locator('#reviewGoalProvenanceDetails').innerText()) === '',
    'Goal Review must reject more than 144 events without rendering partial content');
  await nativeTools.evaluate(() => {
    window.__goalReviewReceipt.provenance.events.length = 5;
    window.__goalReviewReceipt.provenance.goalId =
      '1884eab0-4eee-4a04-8a11-000000000099';
  });
  await nativeTools.locator('[data-work-tab="review"]').click();
  await nativeTools.locator('#reviewGoalProvenanceState')
    .getByText('unavailable or stale', {exact:false}).waitFor();
  ensure((await nativeTools.locator('#reviewGoalProvenanceDetails').innerText()) === '',
    'Stale/mismatching Goal ID must not be shown as current checkpoint');
  await nativeTools.evaluate(() => {
    window.__goalReviewReceipt.provenance.goalId = window.__goalReviewReceipt.goalId;
    window.__goalReviewMode = 'pending';
  });
  await nativeTools.locator('[data-work-tab="review"]').click();
  await nativeTools.waitForFunction(() => typeof window.__goalReviewRelease === 'function');
  await nativeTools.locator('[data-work-tab="terminal"]').click();
  await nativeTools.waitForFunction(() => window.__ptySessionIds.length === 1);
  await nativeTools.locator('#androidPtyTabs button[aria-label="Close terminal sh 1"]').waitFor({state:'visible'});
  ensure((await nativeTools.evaluate(() => window.__androidNativeToolCalls))
    .filter(x => x === 'pty:create').length === 1 &&
    await nativeTools.evaluate(() => document.activeElement?.id !== 'androidPtyCommand' &&
      !document.activeElement?.classList.contains('xterm-helper-textarea')),
    'Opening the first sh 1 tab must never focus a keyboard input automatically');
  await nativeTools.evaluate(() => {
    window.__goalReviewMode = 'success';
    window.__goalReviewRelease();
  });
  await nativeTools.locator('[data-work-tab="review"]').click();
  await nativeTools.locator('#reviewGoalProvenanceState')
    .getByText('Saved checkpoint metadata only', {exact:false}).waitFor();
  ensure((await nativeTools.locator('#reviewGoalProvenanceDetails').innerText()).includes('3 recorded turn events') &&
    (await nativeTools.evaluate(() => window.__goalReviewQueries)) >= reviewCountBeforeGoal + 7,
    'An old in-flight Review receipt must not replace the explicitly reopened current Review');
  await nativeTools.locator('[data-work-tab="subagents"]').click();
  await nativeTools.locator('#mobileGoalProgress').getByText('completed · 3/3').waitFor();
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#mobileGoalArchive').click();
  await nativeTools.locator('#mobileGoalCreate').waitFor({state:'visible'});
  ensure((await nativeTools.evaluate(() => window.__goalRunCalls.length)) === 0,
    'Manual approval lifecycle must never invoke provider AI or auto-execute');
  await nativeTools.locator('#mobileGoalTitle').fill('');
  await nativeTools.locator('#mobileGoalSteps').fill('');
  await nativeTools.locator('#mobileGoalTurns').fill('12');
  await nativeTools.locator('#mobileGoalRetries').fill('2');
  await nativeTools.locator('#mobileGoalTitle').fill('Synthetic QA checkpoint');
  await nativeTools.locator('#mobileGoalSteps').fill('Inspect synthetic result');
  await nativeTools.locator('#mobileGoalTurns').fill('1');
  await nativeTools.locator('#mobileGoalCreate button[type="submit"]').click();
  await nativeTools.locator('#mobileGoalHeadline').getByText('Synthetic QA checkpoint').waitFor();
  ensure((await nativeTools.evaluate(() => window.__mcpTestCalls)).includes('goal:create'),
    'Checkpointed goal creation did not call native Rust route');
  await nativeTools.locator('#mobileGoalBegin').click();
  await nativeTools.locator('#mobileGoalOutcome').waitFor({state:'visible'});
  await nativeTools.locator('#mobileGoalEvidence').fill('User verified synthetic outcome');
  await nativeTools.locator('[data-goal-outcome="completed"]').click();
  await nativeTools.locator('#mobileGoalProgress').getByText('completed · 1/1').waitFor();
  ensure((await nativeTools.evaluate(() => window.__mcpTestCalls)).includes('goal:finish'),
    'Goal step outcome not recorded through native route');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#mobileGoalArchive').click();
  await nativeTools.locator('#mobileGoalCreate').waitFor({state:'visible'});
  ensure((await nativeTools.evaluate(() => window.__mcpTestCalls)).includes('goal:archive'),
    'Completed goal checkpoint was not archived by explicit user action');
  // Android only uses ChatGPT website. Native direct-model Goal runs,
  // delegated agents and model-proposed MCP tools must remain unavailable,
  // while manual Goal turns are still persisted by Rust.
  await nativeTools.locator('#mobileGoalTitle').fill('Manual Goal without AI provider');
  await nativeTools.locator('#mobileGoalSteps').fill('Inspect local synthetic evidence');
  await nativeTools.locator('#mobileGoalTurns').fill('3');
  await nativeTools.locator('#mobileGoalCreate button[type="submit"]').click();
  await nativeTools.locator('#mobileGoalHeadline').getByText('Manual Goal without AI provider').waitFor();
  ensure(await nativeTools.locator('#mobileGoalRunAi').isHidden() &&
    await nativeTools.locator('#mobileGoalStopAi').isHidden() &&
    await nativeTools.locator('#mobileGoalDelegateControls').isHidden() &&
    await nativeTools.locator('#mobileGoalMcpControls').isHidden() &&
    await nativeTools.locator('#mobileGoalMcpReview').isHidden() &&
    (await nativeTools.evaluate(() => window.__goalRunCalls.length)) === 0 &&
    (await nativeTools.evaluate(() => window.__delegateCalls.filter(call => call.action === 'run').length)) === 0,
    'Android manual Goals must not surface API-provider AI Run/Stop, delegate or MCP proposal controls');
  await nativeTools.locator('#mobileGoalBegin').click();
  await nativeTools.locator('#mobileGoalOutcome').waitFor({state:'visible'});
  await nativeTools.locator('#mobileGoalEvidence').fill('User independently checked local evidence');
  await nativeTools.locator('[data-goal-outcome="completed"]').click();
  await nativeTools.locator('#mobileGoalProgress').getByText('completed · 1/1').waitFor();
  ensure((await nativeTools.evaluate(() => window.__goalRunCalls.length)) === 0 &&
    (await nativeTools.evaluate(() => window.__goalMcpExecuted)) === 0,
    'Manual Goal checkpoint completion cannot launch any AI or MCP model proposal');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#mobileGoalArchive').click();
  await nativeTools.locator('#mobileGoalCreate').waitFor({state:'visible'});
  await nativeTools.locator('#toolViewClose').click();
  await nativeTools.locator('#toolPanelToggle').click();
  await nativeTools.locator('[data-tool-view="files"]').click();
  ensure(await nativeTools.locator('#filesPrivateEditToggle').isVisible(),
    'Private text editor unavailable only when Android native runtime supports explicit writes');
  await nativeTools.locator('#filesPrivateEditToggle').click();
  await nativeTools.locator('#filesPrivateWriteName').fill('synthetic.txt');
  await nativeTools.locator('#filesPrivateWriteContent').fill('Synthetic local text');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#filesPrivateCreate').click();
  await nativeTools.locator('#filesPrivateWriteStatus').getByText('Saved user-files/synthetic.txt').waitFor();
  ensure((await nativeTools.evaluate(() => window.__mcpTestCalls))
    .includes('private-write:create_text:user-files/synthetic.txt:Synthetic local text'),
    'Private text creation must invoke bounded Rust API under user-files/ with consent');
  ensure(await nativeTools.locator('#filesContentSearchForm').isVisible(),
    'Android-only private content search must be available with a real native API');
  await nativeTools.locator('#filesContentSearchQuery').fill('Synthetic');
  await nativeTools.locator('#filesContentSearchForm button[type="submit"]').click();
  await nativeTools.locator('#filesSearchStatus').getByText('1 verified text match(es)',{exact:false}).waitFor();
  ensure((await nativeTools.evaluate(() => window.__mcpTestCalls)).includes('private-search-text:Synthetic') &&
    (await nativeTools.locator('#filesList').innerText()).includes('user-files/synthetic.txt · line 1') &&
    (await nativeTools.locator('#filesList').innerText()).includes('Synthetic local text'),
    'Content search must use guarded Android route and show only verified app-owned text');
  await nativeTools.locator('#filesContentSearchQuery').fill('not-found');
  await nativeTools.locator('#filesContentSearchForm button[type="submit"]').click();
  await nativeTools.locator('#filesList').getByText('No matching user-owned text files.').waitFor();
  ensure((await nativeTools.locator('#filesSearchStatus').innerText()).includes('0 verified text match(es)'),
    'Content search empty state must not claim a file read');
  await nativeTools.locator('#filesContentSearchQuery').fill('tampered');
  await nativeTools.locator('#filesContentSearchForm button[type="submit"]').click();
  await nativeTools.locator('#filesSearchStatus').getByText('No content search verified:',{exact:false}).waitFor();
  ensure(await nativeTools.locator('#filesList .files-row').count() === 0,
    'Out-of-scope search results must never expose private app internals');
  await nativeTools.locator('#filesSourceSelected').click();
  ensure((await nativeTools.locator('#filesExternalStatus').innerText()).includes('No selected folder') &&
    (await nativeTools.evaluate(() => window.__treeCalls.length)) === 0,
    'SAF picker launched without an explicit user action');
  await nativeTools.locator('#filesChooseFolder').click();
  await nativeTools.locator('#filesExternalList').getByText('selected.txt').waitFor();
  ensure(JSON.stringify(await nativeTools.evaluate(() => window.__treeCalls)) === '["request","list"]',
    'SAF folder picker/list must follow explicit user selection only');
  ensure(await nativeTools.locator('#filesExternalCreateForm').isHidden(),
    'Read-only SAF selection must never expose file creation');
  ensure(await nativeTools.locator('#filesExternalCreateFolderForm').isHidden() &&
    await nativeTools.locator('#filesExternalRenameForm').isHidden() &&
    !(await nativeTools.evaluate(() => window.__treeCalls)).some(call => call.startsWith('create-folder:')),
    'Read-only SAF folder grant must not expose or auto-trigger folder creation or rename');
  await nativeTools.locator('#filesExternalList').getByText('selected.txt').click();
  await nativeTools.locator('#filesExternalPreview').getByText('Selected folder QA text').waitFor();
  ensure((await nativeTools.evaluate(() => window.__treeCalls)).includes('open'),
    'SAF selected document was not opened by the real native API route');
  ensure(await nativeTools.locator('#filesExternalPropertiesForm').isVisible(),
    'Scoped SAF properties should be available with READ-only permission');
  await nativeTools.locator('#filesExternalPropertiesEntry').selectOption('selected.txt');
  await nativeTools.locator('#filesExternalPropertiesForm button[type="submit"]').click();
  await nativeTools.locator('#filesExternalPropertiesResult').getByText('Provider supports rename: Yes', {exact:false}).waitFor();
  const metadata = await nativeTools.locator('#filesExternalPropertiesResult').innerText();
  ensure(metadata.includes('Size: 23 bytes') &&
    metadata.includes('Provider capability flags do not grant app permissions.') &&
    (await nativeTools.evaluate(() => window.__treeCalls)).includes('properties:selected.txt') &&
    await nativeTools.locator('#filesExternalCreateForm').isHidden(),
    'READ-only SAF properties must be real provider metadata and cannot silently enable WRITE');
  await nativeTools.evaluate(() => { window.__treeImageVisible = true; });
  await nativeTools.locator('#filesExternalReload').click();
  await nativeTools.locator('#filesExternalList').getByText('preview.png').waitFor();
  ensure(await nativeTools.locator('#filesExternalImagePreview').isHidden(),
    'Reading a selected folder must not auto-decode or display any image');
  await nativeTools.locator('#filesExternalList').getByText('preview.png').click();
  await nativeTools.locator('#filesExternalImagePreview img').waitFor({state:'visible'});
  ensure((await nativeTools.locator('#filesExternalImage').getAttribute('src')).startsWith('data:image/png;base64,') &&
    (await nativeTools.locator('#filesExternalImagePreview').innerText()).includes('no upload or AI attachment'),
    'Explicit selected SAF PNG must render in-app only with bounded matching receipt');
  await nativeTools.evaluate(() => { window.__treeImageMode = 'badBytes'; });
  await nativeTools.locator('#filesExternalList').getByText('preview.png').click();
  await nativeTools.locator('#filesExternalPreview').getByText('not a verified PNG/JPEG',{exact:false}).waitFor();
  ensure(await nativeTools.locator('#filesExternalImagePreview').isHidden() &&
    !(await nativeTools.locator('#filesExternalImage').getAttribute('src')),
    'SAF image MIME with false PNG bytes must never render or retain previous image data');
  await nativeTools.evaluate(() => { window.__treeImageMode = 'forged'; });
  await nativeTools.locator('#filesExternalList').getByText('preview.png').click();
  await nativeTools.locator('#filesExternalPreview').getByText('mismatched Android selected-document receipt',{exact:false}).waitFor();
  ensure(await nativeTools.locator('#filesExternalImagePreview').isHidden(),
    'SAF image from a different tree must be rejected before display');
  await nativeTools.evaluate(() => { window.__treeImageMode = 'deferred'; });
  await nativeTools.locator('#filesExternalList').getByText('preview.png').click();
  await nativeTools.waitForFunction(() => typeof window.__treeImageRelease === 'function');
  await nativeTools.locator('#filesExternalList').getByText('selected.txt').click();
  await nativeTools.locator('#filesExternalPreview').getByText('Selected folder QA text').waitFor();
  const beforeOldImageReply = await nativeTools.evaluate(() => window.__treeImageReadCompleted);
  await nativeTools.evaluate(() => window.__treeImageRelease());
  await nativeTools.waitForFunction(previous => window.__treeImageReadCompleted > previous,
    beforeOldImageReply);
  ensure(await nativeTools.locator('#filesExternalImagePreview').isHidden() &&
    (await nativeTools.locator('#filesExternalPreview').innerText()).includes('Selected folder QA text'),
    'A delayed previous PNG selection must never overwrite a newer selected text file');
  await nativeTools.evaluate(() => { window.__treeImageMode = 'success'; });
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#filesChooseWritableFolder').click();
  await nativeTools.locator('#filesExternalCreateForm').waitFor({state:'visible'});
  await nativeTools.locator('#filesExternalCreateFolderForm').waitFor({state:'visible'});
  await nativeTools.locator('#filesExternalRenameForm').waitFor({state:'visible'});
  await nativeTools.locator('#filesExternalCreateName').fill('created.txt');
  await nativeTools.locator('#filesExternalCreateContent').fill('New approved SAF text');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#filesExternalCreateForm button[type="submit"]').click();
  await nativeTools.locator('#filesExternalCreateStatus').getByText('Created created.txt').waitFor();
  ensure((await nativeTools.evaluate(() => window.__treeCalls)).includes('create:created.txt:New approved SAF text'),
    'SAF write must follow separate explicit permission and user confirmation');
  await nativeTools.locator('#filesExternalCreateFolderName').fill('../unsafe');
  await nativeTools.locator('#filesExternalCreateFolderForm button[type="submit"]').click();
  ensure((await nativeTools.locator('#filesExternalCreateFolderStatus').innerText()).includes('Unsafe folder name') &&
    !(await nativeTools.evaluate(() => window.__treeCalls)).some(call => call.startsWith('create-folder:')),
    'Traversal folder name must be rejected before invoking SAF');
  await nativeTools.locator('#filesExternalCreateFolderName').fill('New QA Folder');
  nativeTools.once('dialog', dialog => dialog.dismiss());
  await nativeTools.locator('#filesExternalCreateFolderForm button[type="submit"]').click();
  ensure(!(await nativeTools.evaluate(() => window.__treeCalls)).some(call => call.startsWith('create-folder:')),
    'Cancelled UI confirmation must not request folder creation');
  await nativeTools.evaluate(() => { window.__treeFolderMode = 'cancel'; });
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#filesExternalCreateFolderForm button[type="submit"]').click();
  await nativeTools.locator('#filesExternalCreateFolderStatus').getByText('Folder creation cancelled').waitFor();
  ensure((await nativeTools.evaluate(() => window.__treeFolders.length)) === 0,
    'Native cancelled folder dialog cannot claim creation');
  await nativeTools.evaluate(() => { window.__treeFolderMode = 'create'; });
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#filesExternalCreateFolderForm button[type="submit"]').click();
  await nativeTools.locator('#filesExternalCreateFolderStatus').getByText('Created folder New QA Folder').waitFor();
  await nativeTools.locator('#filesExternalList').getByText('New QA Folder').waitFor();
  ensure((await nativeTools.evaluate(() => window.__treeFolders)).join(',') === 'New QA Folder' &&
    (await nativeTools.evaluate(() => window.__treeCalls)).includes('create-folder:New QA Folder'),
    'Verified create-only folder receipt must refresh the selected SAF directory');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#filesExternalCreateFolderForm button[type="submit"]').click();
  await nativeTools.locator('#filesExternalCreateFolderStatus').getByText('No folder creation confirmed').waitFor();
  ensure((await nativeTools.evaluate(() => window.__treeFolders.length)) === 1,
    'Existing SAF folder must not be overwritten on duplicate create');
  await nativeTools.locator('#filesExternalRenameEntry').selectOption('New QA Folder');
  await nativeTools.locator('#filesExternalRenameName').fill('../unsafe');
  await nativeTools.locator('#filesExternalRenameForm button[type="submit"]').click();
  ensure((await nativeTools.locator('#filesExternalRenameStatus').innerText()).includes('Unsafe') &&
    !(await nativeTools.evaluate(() => window.__treeCalls)).some(call => call.startsWith('rename:')),
    'Unsafe rename must fail locally without invoking SAF');
  await nativeTools.locator('#filesExternalRenameName').fill('Renamed QA Folder');
  nativeTools.once('dialog', dialog => dialog.dismiss());
  await nativeTools.locator('#filesExternalRenameForm button[type="submit"]').click();
  ensure(!(await nativeTools.evaluate(() => window.__treeCalls)).some(call => call.startsWith('rename:')),
    'Declining UI rename confirmation must not issue native rename');
  await nativeTools.evaluate(() => { window.__treeRenameMode='cancel'; });
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#filesExternalRenameForm button[type="submit"]').click();
  await nativeTools.locator('#filesExternalRenameStatus').getByText('Native rename cancelled').waitFor();
  ensure((await nativeTools.evaluate(() => window.__treeFolders[0])) === 'New QA Folder',
    'Cancelling Android native rename must not claim mutation');
  await nativeTools.evaluate(() => {window.__treeRenameMode='uncertain';});
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#filesExternalRenameForm button[type="submit"]').click();
  await nativeTools.locator('#filesExternalRenameStatus').getByText('Rename outcome uncertain').waitFor();
  const uncertainCount = await nativeTools.evaluate(() => window.__treeCalls.filter(v => v.startsWith('rename:')).length);
  ensure((await nativeTools.evaluate(() => window.__treeFolders[0])) === 'New QA Folder',
    'Uncertain SAF rename must not claim verified success or automatic retry');
  await nativeTools.locator('#filesExternalReload').click();
  ensure((await nativeTools.evaluate(() => window.__treeCalls.filter(v => v.startsWith('rename:')).length)) === uncertainCount,
    'Manually refreshing after uncertain rename must never replay the write');
  await nativeTools.evaluate(() => {window.__treeRenameMode='success';});
  await nativeTools.locator('#filesExternalRenameEntry').selectOption('New QA Folder');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#filesExternalRenameForm button[type="submit"]').click();
  await nativeTools.locator('#filesExternalRenameStatus').getByText('Verified rename: New QA Folder').waitFor();
  await nativeTools.locator('#filesExternalList').getByText('Renamed QA Folder').waitFor();
  await nativeTools.locator('#filesExternalPropertiesEntry').selectOption('Renamed QA Folder');
  await nativeTools.locator('#filesExternalPropertiesForm button[type="submit"]').click();
  await nativeTools.locator('#filesExternalPropertiesResult').getByText('Type: Folder',{exact:false}).waitFor();
  ensure((await nativeTools.evaluate(() => window.__treeCalls)).includes('properties:Renamed QA Folder'),
    'Renamed SAF entry properties must reflect the newly verified sibling name');
  ensure((await nativeTools.evaluate(() => window.__treeFolders)).join(',') === 'Renamed QA Folder' &&
    (await nativeTools.evaluate(() => window.__treeCalls)).includes('rename:New QA Folder:Renamed QA Folder'),
    'Only verified same-tree/same-parent SAF rename receipt may update the selected folder list');
  for (const width of [320,360,390]) {
    await nativeTools.setViewportSize({width,height:844});
    const view = await nativeTools.evaluate(() => ({
      scrollWidth:document.documentElement.scrollWidth, width:innerWidth,
      form:document.getElementById('filesExternalCreateFolderForm').getBoundingClientRect().toJSON(),
      rename:document.getElementById('filesExternalRenameForm').getBoundingClientRect().toJSON(),
    }));
    ensure(view.scrollWidth <= view.width + 1 && view.form.left >= -1 &&
      view.form.right <= view.width + 1 && view.rename.left >= -1 &&
      view.rename.right <= view.width + 1, `Android create-folder/rename form overflow at ${width}px`);
  }
  await nativeTools.locator('#toolViewClose').click();
  await nativeTools.locator('#sidebarToggle').click();
  await nativeTools.locator('[data-sidebar-extra="skills"]').click();
  ensure(await nativeTools.locator('#mobileSkillImportForm').isVisible(),
    'Android Skills import panel should be available only in installed runtime');
  await nativeTools.locator('#mobileSkillImportPreviewButton').click();
  await nativeTools.locator('#mobileSkillImportPreview').getByText('Synthetic Imported Skill').waitFor();
  ensure(!(await nativeTools.evaluate(() => window.__mcpTestCalls)).some(call => call.startsWith('skill:add:')),
    'Previewing a selected SKILL.md must never auto-import or enable it');
  await nativeTools.evaluate(() => { window.__skillTestSource += '\nChanged between preview and approval'; });
  await nativeTools.locator('#mobileSkillImportApprove').click();
  await nativeTools.locator('#mobileSkillImportStatus').getByText('changed since preview').waitFor();
  ensure(!(await nativeTools.evaluate(() => window.__mcpTestCalls)).some(call => call.startsWith('skill:add:')),
    'Modified SAF skill content must invalidate the reviewed import');
  await nativeTools.evaluate(() => {
    window.__skillTestSource = '---\nname: Synthetic Imported Skill\ndescription: Explicit import fixture\n---\nReview the requested task carefully.';
  });
  await nativeTools.locator('#mobileSkillImportPreviewButton').click();
  await nativeTools.locator('#mobileSkillImportPreview').getByText('Synthetic Imported Skill').waitFor();
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#mobileSkillImportApprove').click();
  await nativeTools.locator('#mobileSkillImportStatus').getByText('Imported Synthetic Imported Skill as disabled').waitFor();
  ensure((await nativeTools.evaluate(() => window.__mcpTestCalls)).includes('skill:add:Synthetic Imported Skill') &&
    (await nativeTools.evaluate(() => window.__nativeImportedSkills)).every(skill => skill.enabled === false),
    'Reviewed SAF skill import must call the native disabled-by-default library');
  await nativeTools.locator('#extrasClose').click();
  await nativeTools.locator('#sidebarToggle').click();
  await nativeTools.locator('[data-sidebar-extra="pets"]').click();
  await nativeTools.locator('#petOverlayStatus').getByText('Overlay permission not granted', {exact:false}).waitFor();
  ensure(await nativeTools.locator('#petOverlayShow').isDisabled() &&
    !(await nativeTools.evaluate(() => window.__petOverlayCalls)).includes('show'),
    'Pocket Bot cannot show or request overlay permissions automatically');
  await nativeTools.evaluate(() => {window.__petOverlay.permissionGranted = true;});
  await nativeTools.locator('#petOverlayRefresh').click();
  await nativeTools.locator('#petOverlayStatus').getByText('Permission granted; overlay is off', {exact:false}).waitFor();
  ensure(await nativeTools.locator('#petOverlayShow').isEnabled() &&
    !(await nativeTools.evaluate(() => window.__petOverlayCalls)).includes('show'),
    'Permission grant alone must never activate an Android system overlay');
  nativeTools.once('dialog', dialog => dialog.dismiss());
  await nativeTools.locator('#petOverlayShow').click();
  ensure(!(await nativeTools.evaluate(() => window.__petOverlayCalls)).includes('show'),
    'Declined user confirmation must not invoke native overlay');
  await nativeTools.evaluate(() => {window.__petOverlayMode = 'cancel';});
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#petOverlayShow').click();
  await nativeTools.locator('#petOverlayStatus').getByText('Permission granted; overlay is off', {exact:false}).waitFor();
  ensure(!(await nativeTools.evaluate(() => window.__petOverlay.visible)),
    'Native overlay consent cancellation cannot report a shown pet');
  await nativeTools.evaluate(() => {window.__petOverlayMode = 'success';});
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#petOverlayShow').click();
  await nativeTools.locator('#petOverlayStatus').getByText('Pocket Bot is attached now', {exact:false}).waitFor();
  ensure(await nativeTools.locator('#petOverlayHide').isEnabled() &&
    (await nativeTools.evaluate(() => window.__petOverlayCalls.filter(x => x === 'show').length)) === 2,
    'Only explicit Pocket Bot actions can report a native attached view');
  await nativeTools.locator('#petOverlayHide').click();
  await nativeTools.locator('#petOverlayStatus').getByText('overlay is off', {exact:false}).waitFor();
  ensure(!(await nativeTools.evaluate(() => window.__petOverlay.visible)) &&
    (await nativeTools.evaluate(() => window.__petOverlayCalls)).includes('hide'),
    'User Hide must remove native overlay and return actual hidden receipt');
  await nativeTools.locator('#extrasClose').click();
  await nativeTools.locator('#toolPanelToggle').click();
  await nativeTools.locator('[data-tool-view="review"]').click();
  await nativeTools.locator('.review-export-button[aria-label*="Synthetic review export"]').click();
  await nativeTools.waitForFunction(() => document.getElementById('reviewExportPreview').value.includes('Synthetic review export'));
  ensure((await nativeTools.evaluate(() => window.__mcpTestCalls)).includes('export'),
    'Conversation Markdown export did not use the native Rust route');
  ensure(await nativeTools.locator('#reviewShareMarkdown').isVisible(),
    'Android Share Sheet should be available only after a real reviewed Markdown export');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#reviewShareMarkdown').click();
  await nativeTools.locator('#reviewShareStatus').getByText('share chooser opened').waitFor();
  ensure((await nativeTools.evaluate(() => window.__androidNativeToolCalls)).includes('review:share'),
    'Explicit reviewed Markdown share did not invoke Android chooser bridge');
  await nativeTools.locator('#toolViewClose').click();
  const ptyCountBeforeMenu = await nativeTools.evaluate(() =>
    window.__androidNativeToolCalls.filter(x => x === 'pty:create').length);
  await nativeTools.locator('#toolPanelToggle').click();
  ensure((await nativeTools.evaluate(() =>
    window.__androidNativeToolCalls.filter(x => x === 'pty:create').length)) === ptyCountBeforeMenu,
    'Opening the workspace menu must never start a shell in the background');
  await nativeTools.locator('[data-tool-view="terminal"]').click();
  await nativeTools.locator('#androidPtyNew').waitFor({state:'visible'});
  await nativeTools.waitForFunction(() => window.__ptySessionIds.length === 1);
  ensure(await nativeTools.locator('#androidPtyWorkspace').isVisible() &&
    await nativeTools.locator('#terminalForm').isHidden() &&
    (await nativeTools.evaluate(() => window.__androidNativeToolCalls.filter(x => x === 'pty:create').length)) === 1 &&
    await nativeTools.locator('#androidPtyTabs [role="tab"]').count() === 1,
    'Reopening the Terminal panel must reuse the automatically created shell tab');
  await nativeTools.locator('#androidPtyCommand').fill('pwd');
  await nativeTools.locator('#androidPtyMobileForm button').click();
  await nativeTools.waitForFunction(() =>
    window.__androidNativeToolCalls.some(x => x === 'pty:write:pwd\r'));
  ensure(await nativeTools.locator('#androidPtyTabs [role="tab"]').count() === 1 &&
    (await nativeTools.evaluate(() => window.__androidNativeToolCalls)).includes('pty:create'),
    'COS-style Android terminal must explicitly open a real PTY and provide its tab');
  await nativeTools.evaluate(() => window.__ptyHandler({
    event:'mcp-android-pty',payload:{id:window.__ptySessionId,kind:'output',
      data:btoa('synthetic terminal output'),bytes:25}
  }));
  await nativeTools.waitForFunction(() =>
    window.__androidNativeToolCalls.includes('pty:ack:25'));
  await nativeTools.locator('[data-pty-key="interrupt"]').click();
  await nativeTools.waitForFunction(() =>
    window.__androidNativeToolCalls.includes('pty:write:\x03'));
  await nativeTools.locator('[data-work-tab="review"]').click();
  await nativeTools.locator('[data-work-tab="terminal"]').click();
  ensure(await nativeTools.locator('#androidPtyTabs [role="tab"]').count() === 1 &&
    (await nativeTools.evaluate(() => window.__androidNativeToolCalls.filter(x => x === 'pty:create').length)) === 1,
    'Switching COS workspace tabs must retain the existing PTY without duplicating shells');
  await nativeTools.locator('#androidPtyNew').click();
  await nativeTools.waitForFunction(() => window.__ptySessionIds.length === 2);
  await nativeTools.waitForFunction(() =>
    document.querySelector('#androidPtyTabs button[aria-label="Close terminal sh 2"]')?.disabled === false);
  ensure(await nativeTools.locator('#androidPtyTabs [role="tab"]').count() === 2 &&
    (await nativeTools.evaluate(() => window.__androidNativeToolCalls.filter(x => x === 'pty:create').length)) === 2 &&
    await nativeTools.evaluate(() => document.activeElement?.id !== 'androidPtyCommand' &&
      !document.activeElement?.classList.contains('xterm-helper-textarea')),
    '+ New must create sh 2 without focusing the Android keyboard');
  await nativeTools.locator('#androidPtyTabs button[aria-label="Select terminal sh 1"]').click();
  ensure(await nativeTools.evaluate(() => document.activeElement?.id !== 'androidPtyCommand' &&
      !document.activeElement?.classList.contains('xterm-helper-textarea')) &&
    await nativeTools.locator('#androidPtyTabs [role="tab"][aria-selected="true"]').count() === 1,
    'Selecting an existing sh 1 tab must not automatically raise the keyboard');
  await nativeTools.locator('#androidPtyCommand').click();
  ensure(await nativeTools.evaluate(() => document.activeElement?.id === 'androidPtyCommand'),
    'Only tapping Type command should focus the keyboard input');
  await nativeTools.locator('#androidPtyTabs button[aria-label="Select terminal sh 2"]').click();
  ensure(await nativeTools.evaluate(() => document.activeElement?.id !== 'androidPtyCommand' &&
      !document.activeElement?.classList.contains('xterm-helper-textarea')) &&
    (await nativeTools.locator('#androidPtyTabs [role="tab"][aria-selected="true"] button').first().innerText()).includes('sh 2'),
    'Selecting sh 2 must dismiss previous command focus without opening the IME');
  await nativeTools.locator('#androidPtyCommand').click();
  ensure(await nativeTools.evaluate(() => document.activeElement?.id === 'androidPtyCommand'),
    'Tapping the input in sh 2 must still enable manual keyboard entry');
  await nativeTools.locator('#androidPtyCommand').fill('echo second');
  await nativeTools.locator('#androidPtyMobileForm button').click();
  await nativeTools.waitForFunction(() =>
    window.__ptyWrites.some(x => x.id === window.__ptySessionIds[1] && x.data === 'echo second\r'));
  ensure(await nativeTools.evaluate(() => window.__ptyWrites.some(x =>
    x.id === window.__ptySessionIds[1] && x.data === 'echo second\r')),
    'Typing on the focused sh 2 tab must write only to its own session');
  await nativeTools.locator('#androidPtyNew').click();
  await nativeTools.waitForFunction(() => window.__ptySessionIds.length === 3);
  await nativeTools.waitForFunction(() =>
    document.querySelector('#androidPtyTabs button[aria-label="Close terminal sh 3"]')?.disabled === false);
  ensure(await nativeTools.evaluate(() => document.activeElement?.id !== 'androidPtyCommand' &&
    !document.activeElement?.classList.contains('xterm-helper-textarea')),
    '+ New must not reopen the keyboard after typing in sh 2');
  await nativeTools.locator('#androidPtyTabs button[aria-label="Select terminal sh 1"]').click();
  await nativeTools.locator('#androidPtyTabs button[aria-label="Select terminal sh 3"]').click();
  ensure(await nativeTools.evaluate(() => document.activeElement?.id !== 'androidPtyCommand' &&
      !document.activeElement?.classList.contains('xterm-helper-textarea')) &&
    (await nativeTools.locator('#androidPtyTabs [role="tab"][aria-selected="true"] button').first().innerText()).includes('sh 3'),
    'Selecting sh 3 must not automatically focus keyboard input');
  await nativeTools.locator('#androidPtyCommand').click();
  ensure(await nativeTools.evaluate(() => document.activeElement?.id === 'androidPtyCommand'),
    'Manual tap on sh 3 input must continue to allow typing');
  await nativeTools.locator('#androidPtyCommand').fill('echo third');
  await nativeTools.locator('#androidPtyMobileForm button').click();
  await nativeTools.waitForFunction(() =>
    window.__ptyWrites.some(x => x.id === window.__ptySessionIds[2] && x.data === 'echo third\r'));
  ensure(await nativeTools.evaluate(() =>
    window.__ptyWrites.filter(x => x.data === 'echo third\r')
      .every(x => x.id === window.__ptySessionIds[2])),
    'Switching to sh 3 must not route input into sh 1 or sh 2');
  const beforeLeavingTerminal = await nativeTools.evaluate(() => ({
    ids: [...window.__ptySessionIds],
    creates: window.__androidNativeToolCalls.filter(x => x === 'pty:create').length,
    saved: JSON.parse(localStorage.getItem('mcp-agent-ai-android-terminal-tab-layout-v1')),
  }));
  await nativeTools.locator('#toolViewClose').click();
  await nativeTools.locator('#toolPanelToggle').click();
  await nativeTools.locator('[data-tool-view="terminal"]').click();
  await nativeTools.waitForFunction(() => document.getElementById('androidPtyTabs')
    .querySelectorAll('[role="tab"]').length === 3);
  ensure(await nativeTools.evaluate(expected =>
      window.__ptySessionIds.join(',') === expected.ids.join(',') &&
      window.__androidNativeToolCalls.filter(x => x === 'pty:create').length === expected.creates &&
      document.querySelector('#androidPtyTabs [aria-selected="true"] button')?.textContent.includes('sh 3'),
    beforeLeavingTerminal) &&
    beforeLeavingTerminal.saved?.tabs?.length === 3 &&
    beforeLeavingTerminal.saved?.selected === 2 &&
    await nativeTools.evaluate(() => document.activeElement?.id !== 'androidPtyCommand'),
    'Leaving and reopening Terminal must retain all three tabs and the selected running PTY, without opening keyboard or shells');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidPtyTabs button[aria-label="Close terminal sh 3"]').click();
  await nativeTools.waitForFunction(() => window.__ptySessionIds.length === 2);
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidPtyTabs button[aria-label^="Close terminal"]').nth(1).click();
  await nativeTools.waitForFunction(() => window.__ptySessionIds.length === 1);
  ensure(await nativeTools.locator('#androidPtyTabs [role="tab"]').count() === 1 &&
    (await nativeTools.evaluate(() => window.__androidNativeToolCalls.filter(x => x === 'pty:create').length)) === 3,
    'Closing one of multiple tabs must keep the remaining tab, not create a duplicate');
  await nativeTools.screenshot({
    path:path.join(screenshotFolder,'native-android-pty-terminal.png')
  });
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidPtyTabs button[aria-label^="Close terminal"]').click();
  await nativeTools.waitForFunction(() =>
    window.__androidNativeToolCalls.filter(x => x === 'pty:create').length === 4 &&
    window.__ptySessionIds.length === 1);
  await nativeTools.waitForFunction(() =>
    document.querySelector('#androidPtyTabs button[aria-label="Close terminal sh 1"]')?.disabled === false);
  ensure(await nativeTools.locator('#androidPtyTabs [role="tab"]').count() === 1 &&
    (await nativeTools.evaluate(() => window.__androidNativeToolCalls.filter(x => x === 'pty:close').length)) === 3 &&
    await nativeTools.evaluate(() => document.activeElement?.id !== 'androidPtyCommand' &&
      !document.activeElement?.classList.contains('xterm-helper-textarea')),
    'Replacing the last closed tab must not reopen the Android keyboard');
  const createsBeforeBackground = await nativeTools.evaluate(() =>
    window.__androidNativeToolCalls.filter(x => x === 'pty:create').length);
  await nativeTools.evaluate(() => {
    Object.defineProperty(document,'visibilityState',{configurable:true,get:()=> 'hidden'});
    Object.defineProperty(document,'hidden',{configurable:true,get:()=>true});
    document.dispatchEvent(new Event('visibilitychange'));
    window.__ptySessionIds = [];
    window.__ptySessionId = null;
  });
  ensure(await nativeTools.locator('#androidPtyTabs [role="tab"]').count() === 1,
    'Losing Android foreground must not remove the visible terminal tab');
  await nativeTools.evaluate(() => {
    Object.defineProperty(document,'visibilityState',{configurable:true,get:()=> 'visible'});
    Object.defineProperty(document,'hidden',{configurable:true,get:()=>false});
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await nativeTools.waitForFunction(() =>
    document.querySelector('#androidPtyRestart')?.hidden === false);
  ensure(await nativeTools.locator('#androidPtyTabs [role="tab"]').count() === 1 &&
    await nativeTools.locator('#androidPtyTabs [role="tab"]').getAttribute('data-terminal-state') === 'stopped' &&
    (await nativeTools.locator('#androidPtyStatus').innerText()).includes('stopped') &&
    (await nativeTools.evaluate(() =>
      window.__androidNativeToolCalls.filter(x => x === 'pty:create').length)) === createsBeforeBackground,
    'After Android stops background PTYs, preserve the old tab as stopped rather than silently deleting/restarting it');
  await nativeTools.locator('[data-work-tab="review"]').click();
  await nativeTools.locator('[data-work-tab="terminal"]').click();
  ensure(await nativeTools.locator('#androidPtyTabs [role="tab"]').count() === 1 &&
    await nativeTools.locator('#androidPtyRestart').isVisible(),
    'Re-entering Terminal after a background stop must retain the stopped tab with an explicit Restart action');
  await nativeTools.locator('#androidPtyRestart').click();
  await nativeTools.waitForFunction(expected =>
    window.__androidNativeToolCalls.filter(x => x === 'pty:create').length === expected + 1,
    createsBeforeBackground);
  await nativeTools.waitForFunction(() =>
    document.querySelector('#androidPtyTabs button[aria-label="Close terminal sh 1"]')?.disabled === false);
  ensure(await nativeTools.locator('#androidPtyTabs [role="tab"]').count() === 1 &&
    await nativeTools.locator('#androidPtyTabs button[aria-label="Close terminal sh 1"]').isEnabled() &&
    await nativeTools.locator('#androidPtyRestart').isHidden() &&
    await nativeTools.evaluate(() => document.activeElement?.id !== 'androidPtyCommand'),
    'Explicit Restart must reconnect the same tab layout with a new local shell and no auto keyboard');
  await nativeTools.locator('#androidPtyNew').click();
  await nativeTools.waitForFunction(() => window.__ptySessionIds.length === 2);
  await nativeTools.locator('#androidPtyNew').click();
  await nativeTools.waitForFunction(() => window.__ptySessionIds.length === 3);
  await nativeTools.evaluate(() => {
    window.__ptySessionIds = [];
    window.__ptySessionId = null;
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await nativeTools.waitForFunction(() => [...document.querySelectorAll('#androidPtyTabs [role="tab"]')]
    .every(tab => tab.getAttribute('data-terminal-state') === 'stopped'));
  const stoppedTabLayout = await nativeTools.evaluate(() => {
    const bar = document.getElementById('androidPtyTabs');
    return {
      labels: [...bar.querySelectorAll('[role="tab"] > button:first-child')].map(button => button.textContent),
      visibleWidth:bar.clientWidth, contentWidth:bar.scrollWidth,
      restartOutsideTabs:!!document.querySelector('.android-pty-status-row > #androidPtyRestart:not([hidden])'),
    };
  });
  ensure(stoppedTabLayout.labels.join(',') === 'sh 1,sh 2,sh 3' &&
    stoppedTabLayout.contentWidth <= stoppedTabLayout.visibleWidth + 2 &&
    stoppedTabLayout.restartOutsideTabs,
    'Three stopped terminal tabs must remain compact, visible and separately selectable; Restart must not crowd the tabs');
  await nativeTools.screenshot({
    path:path.join(screenshotFolder,'native-android-pty-stopped-tabs.png')
  });
  await nativeTools.locator('#androidTermuxDetails summary').click();
  await nativeTools.locator('#androidTermuxStatus').getByText('not installed', {exact:false}).waitFor();
  ensure(await nativeTools.locator('#androidTermuxInstall').isVisible() &&
    await nativeTools.locator('#terminalForm').isHidden(),
    'External Termux must remain optional and distinct from the in-app PTY');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidTermuxInstall').click();
  ensure((await nativeTools.evaluate(() => window.__androidNativeToolCalls)).includes('termux:install') &&
    !(await nativeTools.evaluate(() => window.__androidNativeToolCalls)).some(v => v.startsWith('terminal:')),
    'Termux install action must open external listing, not execute a mock shell command');
  await nativeTools.evaluate(() => { window.__termuxInstalled = true; });
  await nativeTools.locator('#androidTermuxRefresh').click();
  await nativeTools.locator('#androidTermuxOpen').waitFor({state:'visible'});
  await nativeTools.locator('#androidTermuxOpen').click();
  ensure((await nativeTools.evaluate(() => window.__androidNativeToolCalls)).includes('termux:open') &&
    await nativeTools.locator('#terminalForm').isHidden(),
    'External Termux should open only on explicit action; the PTY must remain separate');
  await nativeTools.locator('#toolViewClose').click();
  await nativeTools.locator('#sidebarToggle').click();
  await nativeTools.locator('#sidebarCustomize').click();
  await nativeTools.locator('[data-android-settings-category="setup"]').click();
  ensure(await nativeTools.locator('#screenCaptureOne').count() === 0 &&
    await nativeTools.locator('#screenCapturePreview').count() === 0 &&
    !(await nativeTools.locator('#storageAccessSection').innerText()).includes('One-frame screen capture'),
    'Native Android must not expose deleted One-frame MediaProjection capture controls');
  ensure(await nativeTools.locator('[data-permission-row]').count() === 5 &&
    await nativeTools.locator('#accessibilityManage').isDisabled() &&
    await nativeTools.locator('#accessibilityPermissionState').innerText() === 'Connected' &&
    await nativeTools.locator('#accessibilityFeedback,#storageAccessFeedback,#androidCosAllowNext,#refreshAndroidPermissions').count() === 0 &&
    await nativeTools.locator('#accessibilityControls').count() === 0 &&
    await nativeTools.locator('#accessibilityListWindows').count() === 0 &&
    await nativeTools.locator('#accessibilitySnapshot').count() === 0 &&
    await nativeTools.locator('#accessibilityActionForm').count() === 0 &&
    (await nativeTools.locator('#storageAccessSection').innerText()).includes('Accessibility service') &&
    !(await nativeTools.locator('#storageAccessSection').innerText()).includes('Statuses come from Android'),
    'Accessibility permission retained, lower helper text/buttons and selected-window controls removed');
  ensure((await nativeTools.evaluate(() => window.__androidNativeToolCalls)).filter(v => v.startsWith('accessibility:')).length === 0,
    'Settings status checks must not read or interact with another app window');
  await nativeTools.locator('#storageAccessSection').scrollIntoViewIfNeeded();
  await nativeTools.screenshot({ path: path.join(screenshotFolder, 'native-android-permissions-automation.png'), fullPage: true });
  await closeNativeAndroidSettings(nativeTools);
  ensure(await nativeTools.locator('#provider').inputValue() === 'chatgpt-browser' &&
    JSON.stringify(await nativeTools.locator('#provider option').evaluateAll(options =>
      options.map(option => option.value))) === '["chatgpt-browser"]' &&
    (await nativeTools.evaluate(() => window.__mcpTestCalls.filter(call => call.startsWith('chat:')).length)) === 0,
    'Android must never issue a direct AI provider chat or streaming request after manual Goal and file operations');
  await nativeTools.locator('#sidebarQuickSearch').click();
  ensure(await nativeTools.locator('[data-view-action="browser"]').isEnabled(),
    'Android COS View menu did not expose the real native browser');
  ensure(await nativeTools.locator('[data-view-action="browser-tools"]').isEnabled(),
    'Advanced native browser controls must remain accessible separately');
  await nativeTools.locator('[data-view-action="browser-tools"]').click();
  ensure(await nativeTools.locator('#androidBrowserPanel').isVisible(),
    'Android View browser did not open the real browser controls');
  await nativeTools.locator('#androidBrowserUrl').fill('https://synthetic.example/');
  await nativeTools.locator('#androidBrowserOpenForm button[type="submit"]').click();
  await nativeTools.locator('#androidBrowserTabs').getByText('Synthetic HTTPS page').waitFor();
  ensure((await nativeTools.evaluate(() => window.__browserCalls)).includes('in_app_browser_open'),
    'In-app browser did not invoke native open from explicit user action');
  ensure(await nativeTools.locator('#androidBrowserPreviewOnce').isEnabled() &&
    (await nativeTools.evaluate(() => window.__browserPreviewPrepareCount)) === 0,
    'Browser preview must be opt-in and never capture during page opening');
  nativeTools.once('dialog', dialog => dialog.dismiss());
  await nativeTools.locator('#androidBrowserPreviewOnce').click();
  ensure((await nativeTools.evaluate(() => window.__browserPreviewPrepareCount)) === 0,
    'Cancelling browser preview confirmation must not prepare a capture');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidBrowserPreviewOnce').click();
  const previewImage = nativeTools.locator('#androidBrowserPreviewOutput img');
  await previewImage.waitFor({state:'visible'});
  ensure((await previewImage.getAttribute('src')).startsWith('data:image/png;base64,') &&
    (await nativeTools.evaluate(() => window.__browserPreviewPrepareCount)) === 1 &&
    (await nativeTools.evaluate(() => window.__browserPreviewCaptureCount)) === 1,
    'One explicit tab preview must prepare then capture a bounded in-memory PNG exactly once');
  await nativeTools.locator('#androidBrowserRefresh').click();
  ensure((await nativeTools.evaluate(() => window.__browserPreviewCaptureCount)) === 1,
    'Refreshing tab status must never capture another browser frame');
  for (const width of [320,360,390]) {
    await nativeTools.setViewportSize({width,height:844});
    const view = await nativeTools.evaluate(() => ({
      width:innerWidth,scrollWidth:document.documentElement.scrollWidth,
      image:document.querySelector('#androidBrowserPreviewOutput img').getBoundingClientRect().toJSON(),
    }));
    ensure(view.scrollWidth <= view.width + 1 && view.image.left >= -1 &&
      view.image.right <= view.width + 1, `One-time browser preview overflow at ${width}px`);
  }
  await nativeTools.evaluate(() => {window.__browserPreviewMode='cancel';});
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidBrowserPreviewOnce').click();
  await nativeTools.locator('#androidBrowserStatus').getByText('preview cancelled', {exact:false}).waitFor();
  ensure(await nativeTools.locator('#androidBrowserPreviewOutput img').count() === 0,
    'Denied Android capture must not retain the prior image');
  await nativeTools.evaluate(() => {window.__browserPreviewMode='success';});
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidBrowserPreviewOnce').click();
  await previewImage.waitFor({state:'visible'});
  await nativeTools.evaluate(() => {
    window.__browserTest.tabs.push({tabId:2,title:'Synthetic second tab',
      url:'https://second.synthetic.example/',active:false});
  });
  await nativeTools.locator('#androidBrowserRefresh').click();
  await nativeTools.locator('#androidBrowserTabs').getByText('Synthetic second tab').click();
  ensure(await nativeTools.locator('#androidBrowserPreviewOutput img').count() === 0 &&
    (await nativeTools.evaluate(() => window.__browserTest.activeTabId)) === 2,
    'Changing selected tab must clear previous in-memory preview immediately');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidBrowserPreviewOnce').click();
  await previewImage.waitFor({state:'visible'});
  await nativeTools.locator('[aria-label="Close in-app browser tab 2"]').click();
  ensure(await nativeTools.locator('#androidBrowserPreviewOutput img').count() === 0 &&
    (await nativeTools.evaluate(() => window.__browserTest.activeTabId)) === 1,
    'Closing selected browser tab must clear the one-time image');
  await nativeTools.locator('#androidBrowserConsent').click();
  await nativeTools.locator('#androidBrowserSnapshot').waitFor({state:'visible'});
  await nativeTools.locator('#androidBrowserSnapshot').click();
  await nativeTools.locator('#androidBrowserSnapshotOutput').getByText('Synthetic HTML from owned tab').waitFor();
  await nativeTools.locator('#androidBrowserAction').selectOption('inspect');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidBrowserActionForm button[type="submit"]').click();
  await nativeTools.locator('#androidBrowserSnapshotOutput .android-browser-node').last()
    .getByText('type: button', {exact:false}).waitFor();
  ensure((await nativeTools.evaluate(() => window.__browserCalls)).includes('in_app_browser_inspect') &&
    (await nativeTools.locator('#androidBrowserStatus').innerText()).includes('Snapshot retained'),
    'Read-only indexed inspect must use exact owned snapshot and retain it for another action');
  await nativeTools.locator('#androidBrowserAction').selectOption('hover');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidBrowserActionForm button[type="submit"]').click();
  await nativeTools.locator('#androidBrowserStatus').getByText('Synthetic DOM hover events dispatched', {exact:false}).waitFor();
  ensure((await nativeTools.evaluate(() => window.__browserCalls)).includes('in_app_browser_hover') &&
    (await nativeTools.locator('#androidBrowserSnapshotOutput').innerText()) === '',
    'Hover must be synthetic only, never physical pointer, and consume the DOM snapshot');
  await nativeTools.locator('#androidBrowserSnapshot').click();
  await nativeTools.locator('#androidBrowserSnapshotOutput').getByText('Synthetic HTML from owned tab').waitFor();
  await nativeTools.locator('#androidBrowserAction').selectOption('click');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidBrowserActionForm button[type="submit"]').click();
  ensure((await nativeTools.evaluate(() => window.__browserCalls)).includes('in_app_browser_click'),
    'Indexed browser action did not reach native bridge after explicit confirmation');
  await nativeTools.evaluate(() => {window.__browserPreviewMode='pending';});
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidBrowserPreviewOnce').click();
  await nativeTools.waitForFunction(() => window.__browserPreviewCaptureCount >= 5);
  ensure(await nativeTools.locator('#androidBrowserPreviewOnce').isDisabled() &&
    await nativeTools.locator('#androidBrowserPreviewOutput img').count() === 0,
    'In-flight preview should disable duplicate clicks and not expose partial images');
  await nativeTools.evaluate(() => {
    window.__browserTest.tabs.push({tabId:3,title:'Synthetic third tab',
      url:'https://third.synthetic.example/',active:false});
  });
  await nativeTools.locator('#androidBrowserRefresh').click();
  await nativeTools.locator('#androidBrowserTabs').getByText('Synthetic third tab').click();
  await nativeTools.evaluate(() => window.__browserPreviewRelease());
  ensure(await nativeTools.locator('#androidBrowserPreviewOutput img').count() === 0,
    'Late capture for an old tab must never repaint an image after tab switch');
  await nativeTools.screenshot({ path: path.join(screenshotFolder, 'native-android-browser-controls.png'), fullPage: true });
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#androidBrowserStop').click();
  await nativeTools.waitForFunction(() =>
    document.getElementById('androidBrowserStatus').textContent.includes('closed'));
  ensure((await nativeTools.evaluate(() => window.__browserTest.opened)) === false,
    'Native browser close all did not stop the owned tabs');
  await nativeTools.locator('#androidBrowserClose').click();
  ensure(await nativeTools.locator('#androidBrowserPanel').isHidden(),
    'Closing advanced browser controls must fully dismiss the separate inspector');
  // The new header shortcut opens the native Chrome-like drawer directly,
  // without displaying another HTML browser panel above the chat.
  const beforeDrawerOpens = await nativeTools.evaluate(() =>
    window.__browserCalls.filter(name => name === 'in_app_browser_open').length);
  await nativeTools.locator('#androidBrowserDrawerToggle').click();
  await nativeTools.waitForFunction(() => window.__browserTest.opened);
  ensure(await nativeTools.locator('#androidBrowserPanel').isHidden() &&
    await nativeTools.locator('#sidebar').evaluate(el => !el.classList.contains('open')) &&
    (await nativeTools.evaluate(() => window.__browserCalls.filter(name =>
      name === 'in_app_browser_open').length)) === beforeDrawerOpens + 1,
    'Explicit browser shortcut must open only one native drawer without extra inspector');
  await nativeTools.evaluate(() => {
    window.__browserTest.opened = false;
    window.__browserTest.tabs = [];
    window.__browserTest.activeTabId = 0;
  });
  await nativeTools.locator('#sidebarToggle').click();
  ensure(await nativeTools.locator('#sidebar #projectBackendControls').count() === 0 &&
    await nativeTools.locator('#projectNativeEnable').isHidden(),
    'Native storage entry point must no longer clutter Projects sidebar');
  await nativeTools.locator('#sidebarCustomize').click();
  await nativeTools.locator('[data-android-settings-category="workspace"]').click();
  ensure(await nativeTools.locator('#projectNativeEnable').isHidden() &&
    await nativeTools.locator('#projectStorageSettings').isHidden() &&
    await nativeTools.locator('#androidWorkspacePermissionsSlot #androidCosAccessPanel').isVisible() &&
    await nativeTools.locator('#androidWorkspaceFoldersSlot #androidCosFolderPanel').isVisible(),
    'Native Workspace must show shared Setup controls and keep retired Project Storage migration hidden');
  // The compatibility project bridge is still covered, but is no longer a
  // visible Workspace control. Drive that internal binding only to preserve
  // preexisting project API regression coverage.
  await nativeTools.locator('#projectNativeEnable').evaluate(button => button.click());
  await nativeTools.waitForFunction(() =>
    localStorage.getItem('mcp-agent-native-projects-active-v1') === 'on');
  ensure((await nativeTools.evaluate(() => localStorage.getItem('mcp-agent-native-projects-active-v1'))) === 'on',
    'Explicit native project migration did not persist its selected storage mode');
  await closeNativeAndroidSettings(nativeTools);
  await nativeTools.locator('#sidebarToggle').click();
  await nativeTools.locator('#projectAdd').click();
  ensure(await nativeTools.locator('#projectChooseServerSsh').isDisabled(),
    'Android server SSH option must not impersonate a working remote workspace transport');
  await nativeTools.locator('#projectAddGroup').click();
  const existingFilesTabGrant = await nativeTools.evaluate(() => window.__treeTestGranted);
  await nativeTools.locator('#projectName').fill('Native synthetic group');
  await nativeTools.locator('#projectForm button[type="submit"]').click();
  await nativeTools.locator('#projectsList').getByText('Native synthetic group').waitFor();
  const createdProjectId = 'f2a19e15-9921-4fc7-87a5-a02c3f38cb4f';
  ensure((await nativeTools.evaluate(() => window.__projectFolderCalls.length)) === 0,
    'Creating a Project must not silently request Android folder access');
  await nativeTools.locator('#projectAdd').click();
  await nativeTools.evaluate(() => {window.__projectFolderMode = 'cancel';});
  await nativeTools.locator('#projectChoosePhoneFolder').click();
  await nativeTools.waitForFunction(() => window.__projectFolderCalls.some(x => x.action === 'request'));
  ensure((await nativeTools.evaluate(id => !!window.__projectFolderBindings[id],createdProjectId)) === false,
    'Cancelling Project folder picker must never persist a grant');
  await nativeTools.locator('#projectAdd').click();
  await nativeTools.evaluate(() => {window.__projectFolderMode = 'invalid';});
  await nativeTools.locator('#projectChoosePhoneFolder').click();
  await nativeTools.locator('#projectPhoneFolderStatus').getByText('Folder not linked:',{exact:false}).waitFor({state:'attached'});
  ensure((await nativeTools.evaluate(id => !!window.__projectFolderBindings[id],createdProjectId)) === false,
    'Unverified Project SAF receipt must not be treated as a persistent read grant');
  await nativeTools.locator('#projectAdd').click();
  await nativeTools.evaluate(() => {window.__projectFolderMode = 'success';});
  await nativeTools.locator('#projectChoosePhoneFolder').click();
  await nativeTools.locator('#projectSelectedFolderStatus').getByText('Read access verified',{exact:false}).waitFor();
  ensure(await nativeTools.locator('#projectUnlinkPhoneFolder').isVisible() &&
    (await nativeTools.evaluate(id => !!window.__projectFolderBindings[id],createdProjectId)) === true &&
    (await nativeTools.evaluate(() => window.__treeTestGranted)) === existingFilesTabGrant,
    'Project SAF should use independent, verified read-only binding without granting Files-tab access');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#projectRemove').click();
  ensure(await nativeTools.locator('#projectsList').getByText('Native synthetic group').isVisible() &&
    (await nativeTools.evaluate(id => !!window.__projectFolderBindings[id],createdProjectId)) === true &&
    !(await nativeTools.evaluate(() => window.__mcpTestCalls)).includes('project:remove'),
    'Project deletion must refuse orphaning a linked persisted SAF permission');
  nativeTools.once('dialog', dialog => dialog.dismiss());
  await nativeTools.locator('#projectUnlinkPhoneFolder').click();
  ensure((await nativeTools.evaluate(id => !!window.__projectFolderBindings[id],createdProjectId)) === true,
    'Declining Project grant removal must preserve the existing binding');
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#projectUnlinkPhoneFolder').click();
  await nativeTools.locator('#projectSelectedFolderStatus').getByText('No phone folder linked',{exact:false}).waitFor();
  const projectFolderCalls = await nativeTools.evaluate(() => window.__projectFolderCalls);
  ensure(!await nativeTools.locator('#projectUnlinkPhoneFolder').isVisible() &&
    !(await nativeTools.evaluate(id => !!window.__projectFolderBindings[id],createdProjectId)) &&
    projectFolderCalls.filter(x => x.action === 'request').length === 3 &&
    projectFolderCalls.filter(x => x.action === 'release').length === 1 &&
    projectFolderCalls.filter(x => x.action === 'request' || x.action === 'release').every(x => x.userInitiated) &&
    !projectFolderCalls.some(x => x.id !== createdProjectId),
    'Independent Project SAF picker and explicit unlink must preserve the right project, files and consent');
  await nativeTools.locator('#projectRename').click();
  await nativeTools.locator('#projectName').fill('Renamed synthetic group');
  await nativeTools.locator('#projectForm button[type="submit"]').click();
  await nativeTools.locator('#projectsList').getByText('Renamed synthetic group').waitFor();
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#projectRemove').click();
  await nativeTools.locator('#projectsList').getByText('No projects yet').waitFor();
  const nativeProjectCalls = await nativeTools.evaluate(() => window.__mcpTestCalls);
  ensure(['project:status','project:create','project:rename','project:remove']
    .every(action => nativeProjectCalls.includes(action)),
    'Project lifecycle must use real Rust catalog commands rather than local-only edits');
  ensure(await nativeTools.evaluate(() => localStorage.getItem('mcp-agent-mobile-projects-v1')) === null,
    'Native Rust project operations modified the original legacy project labels');
  await nativeTools.locator('#sidebarCustomize').click();
  await nativeTools.locator('[data-android-settings-category="workspace"]').click();
  nativeTools.once('dialog', dialog => dialog.accept());
  await nativeTools.locator('#projectNativeDisable').evaluate(button => button.click());
  await nativeTools.waitForFunction(() =>
    localStorage.getItem('mcp-agent-native-projects-active-v1') === null);
  await nativeTools.evaluate(() => localStorage.setItem('mcp-agent-mobile-projects-v1',
    JSON.stringify({projects:[{id:'project-qa',name:'Reviewed local label'}],assignments:{}})));
  await nativeTools.locator('#projectNativeEnable').evaluate(button => button.click());
  await nativeTools.waitForFunction(() =>
    !document.getElementById('projectImportPreview').classList.contains('hidden'));
  await nativeTools.evaluate(() => localStorage.setItem('mcp-agent-mobile-projects-v1',
    JSON.stringify({projects:[{id:'project-qa',name:'Changed after review'}],assignments:{}})));
  await nativeTools.locator('#projectImportApprove').evaluate(button => button.click());
  ensure((await nativeTools.locator('#projectNativeStatus').innerText()).includes('changed during review') &&
    (await nativeTools.evaluate(() => localStorage.getItem('mcp-agent-native-projects-active-v1'))) === null &&
    !(await nativeTools.evaluate(() => window.__mcpTestCalls)).includes('project:approve-import'),
    'Stale localStorage review must refuse Rust import and preserve original mode');
  await nativeTools.locator('#androidSettingsBack').click();
  await nativeTools.locator('[data-android-settings-category="setup"]').click();
  ensure(await nativeTools.locator('#connectionSection').isHidden() &&
    await nativeTools.locator('#backgroundChatgptSettings').isHidden() &&
    await nativeTools.locator('#backgroundChatgptOpen').isHidden() &&
    await nativeTools.locator('#provider').isHidden() &&
    !(await nativeTools.locator('#androidSettingsContent').innerText()).includes('ChatGPT built-in browser') &&
    !(await nativeTools.evaluate(() => window.__ownedChatgptCalls)).includes('open') &&
    (await nativeTools.evaluate(() => window.__ownedChatgptSends)) === 0,
    'Neither removed Setup card may reappear or start an owned ChatGPT browser in installed Android mode');
  await nativeTools.screenshot({path:path.join(screenshotFolder,'native-android-setup-without-legacy-connection-cards.png'),fullPage:true});
  await nativeTools.close();
  ensure(errors.length === 0, `mobile JavaScript errors: ${errors.join('; ')}`);
  console.log(`UI QA PASS: ${checks} assertions. Screenshots: ${screenshotFolder}`);
} finally {
  if (browser) await browser.close();
  await app.close();
  await rm(directory, { recursive: true, force: true });
}
