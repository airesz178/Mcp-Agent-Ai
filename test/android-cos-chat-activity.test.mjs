import test from 'node:test';
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { mkdtemp, mkdir, rm } from 'node:fs/promises';
import { existsSync, readdirSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';
import { startServer } from '../src/server.mjs';

const HISTORY_ID = '0ee30dd7-44f8-4bdd-968c-5f349d8d4d13';
const LOCAL_ID = '00d81359-5ec6-4b5e-84d8-2bf588fa64d1';
const SECRET_ARGS = 'SYNTHETIC_UNVERIFIED_PRIVATE_ARGUMENT';
const SECRET_RESULT = 'SYNTHETIC_UNVERIFIED_PRIVATE_RESULT';
const LONG_UNVERIFIED = 'UNVERIFIED_LONG_RESULT_'.repeat(500);
const MARKDOWN_ID = '64dcc1d4-b8ce-4d59-895b-00600f8ced70';
const RAW_UNTRUSTED_HTML = '<img src=x onerror=window.__cosHtmlExecuted=true>';

function testChromePath() {
  const root = path.join(os.homedir(),'AppData','Local','ms-playwright');
  if (!existsSync(root)) return undefined;
  return readdirSync(root).filter(name => /^chromium-\d+$/.test(name))
    .map(name => path.join(root,name,'chrome-win64','chrome.exe')).find(existsSync);
}

function historicalSession() {
  return {
    id:HISTORY_ID, provider:'local-automation', model:'native-agent',
    messages:[
      {role:'user',text:'Explain a simple idea without using tools'},
      {role:'assistant',text:'This answer came from saved conversation data.'}
    ],
    // A persisted but unapproved proposal is NOT evidence of execution.
    actions:[{type:'mcp_tool_call',name:'unapproved_fake_tool',approvedByUser:false,
      resultIsError:false,resultStored:false}]
  };
}

test('COS Android chat activity tracks real pending time, confirmed tools and safe reload', {
  timeout:90000
},async () => {
  const temp = await mkdtemp(path.join(os.tmpdir(),'mcp-chat-activity-'));
  await mkdir(path.join(temp,'workspace'));
  const server = await startServer({home:path.join(temp,'data'),
    workspaceRoot:path.join(temp,'workspace'),openBrowser:false});
  let browser;
  let nativeSessions = [historicalSession()];
  try {
    // Build into memory so the test exercises the current app.ts under active
    // development without replacing the production-owned ui/app.js artifact.
    const bundle = await build({
      entryPoints:[fileURLToPath(new URL('../ui/app.ts',import.meta.url))],
      bundle:true,platform:'browser',format:'iife',
      target:['chrome110','safari16'],sourcemap:false,minify:false,
      write:false,logLevel:'silent'
    });
    assert.equal(bundle.outputFiles.length,1);
    browser = await chromium.launch({headless:true,
      ...(testChromePath() ? {executablePath:testChromePath()} : {})});
    const page = await browser.newPage({viewport:{width:360,height:780},
      isMobile:true,hasTouch:true,
      userAgent:'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36'});
    await page.route('**/app.js',route => route.fulfill({
      status:200,contentType:'application/javascript',
      body:bundle.outputFiles[0].text
    }));
    await page.exposeFunction('__activityReadNativeSessions',
      () => structuredClone(nativeSessions));
    await page.exposeFunction('__activityPersistNativeSession',session => {
      assert.equal(session.id,LOCAL_ID,'Mocked native bridge must save exact session');
      nativeSessions = [structuredClone(session),
        ...nativeSessions.filter(item => item.id !== session.id)];
      return true;
    });
    await page.addInitScript(({localId,argumentSecret,resultSecret,longUnverified}) => {
      window.isTauri = true;
      window.__activityNativeCalls = [];
      window.__activityReleaseNative = null;
      window.__TAURI_INTERNALS__ = {invoke:async (command,args={}) => {
        if (command !== 'api_request') return {};
        if (args.route === '/api/state') {
          const response = await fetch('/api/state');
          return {...await response.json(),platform:'android',
            providers:['chatgpt-browser','openai','anthropic','openrouter'],
            hasKeys:[],sessions:await window.__activityReadNativeSessions()};
        }
        if (args.route === '/api/chat-local-action') {
          window.__activityNativeCalls.push({
            route:args.route, action:args.body?.action,
            requestId:args.body?.requestId, userInitiated:args.body?.userInitiated
          });
          if (args.body?.prompt !== 'buka YouTube' ||
              args.body?.action !== 'android_app_start' ||
              args.body?.arguments?.query !== 'YouTube' ||
              args.body?.userInitiated !== true)
            throw Error('Unexpected native action input');
          return await new Promise(resolve => {
            window.__activityReleaseNative = async () => {
              // Metadata-only action mirrors native local-agent receipts.
              // Unexpected args/result fields deliberately carry hostile data:
              // the activity renderer must not treat them as proven output.
              const action = {
                type:'android_local_direct_action',name:'android_app_start',
                userIntentAuthorized:true,nativeConfirmed:true,
                resultIsError:false,resultStored:false,
                arguments:{token:argumentSecret},
                result:{privateValue:resultSecret,details:longUnverified}
              };
              const answer = 'Android reported the requested app launch as confirmed.';
              await window.__activityPersistNativeSession({
                id:localId,provider:'local-automation',model:'native-agent',
                messages:[{role:'user',text:args.body.prompt},{role:'assistant',text:answer}],
                actions:[action]
              });
              resolve({sessionId:localId,answer,actions:[action]});
            };
          });
        }
        // Other native status methods may be unavailable in the browser QA
        // fixture; they must never fabricate chat tool activity.
        return {};
      }};
    },{localId:LOCAL_ID,argumentSecret:SECRET_ARGS,resultSecret:SECRET_RESULT,
      longUnverified:LONG_UNVERIFIED});

    await page.goto(server.url);
    await page.locator('#sidebarToggle').click();
    await page.locator('#threads').getByText('Explain a simple idea without using tools').click();
    const historicalAssistant = page.locator('#messages .message.assistant');
    assert.match(await historicalAssistant.innerText(),/saved conversation data/);
    assert.equal(await page.locator('#messages .android-chat-activity').count(),0,
      'No activity may be fabricated for unapproved or missing native receipts');
    assert.equal((await page.locator('#messages').innerText()).includes('unapproved_fake_tool'),false);
    const previousStyle = await historicalAssistant.evaluate(node => {
      const surface = node.querySelector('.bubble') || node;
      const style = getComputedStyle(surface);
      return {background:style.backgroundColor,
        top:style.borderTopWidth,bottom:style.borderBottomWidth};
    });
    assert.equal(previousStyle.top,'0px',
      'Android assistant prose must not be enclosed in a bordered chat card');
    assert.equal(previousStyle.bottom,'0px');
    assert.ok(['transparent','rgba(0, 0, 0, 0)'].includes(previousStyle.background),
      'Android assistant prose must have a transparent background');

    await page.locator('#sidebarToggle').click();
    await page.locator('#newChat').click();
    await page.locator('#prompt').fill('buka YouTube');
    await page.locator('#send').click();
    await page.waitForFunction(() => typeof window.__activityReleaseNative === 'function');
    const elapsed = page.locator('#messages .android-chat-worked');
    await elapsed.getByText(/^Working for \d+s$/).waitFor();
    assert.equal(await page.locator('#messages .android-chat-running').isVisible(),true,
      'A separate running step should be visible while the native request is in flight');
    assert.equal(await page.locator('#messages .android-chat-activity').count(),0,
      'A pending request without a tool receipt must not show completed tools');
    const firstSeconds = Number((await elapsed.innerText()).match(/Working for (\d+)s/)?.[1]);
    assert.ok(Number.isInteger(firstSeconds));
    await page.waitForFunction(previous => {
      const text = document.querySelector('#messages .android-chat-worked')?.textContent || '';
      const seconds = Number(text.match(/Working for (\d+)s/)?.[1]);
      return Number.isInteger(seconds) && seconds > previous;
    },firstSeconds,{timeout:7000});
    await page.evaluate(() => window.__activityReleaseNative());
    await page.locator('#messages .message.assistant .body')
      .getByText('Android reported the requested app launch as confirmed.').waitFor();
    await elapsed.getByText(/^Worked for \d+s$/).waitFor();
    assert.equal(await page.locator('#messages .android-chat-running').count(),0,
      'The running step must leave once this request has a confirmed saved result');
    assert.equal(await page.locator('#messages .android-chat-activity').count(),1,
      'Exactly one confirmed native action group belongs to this chat');
    const group = page.locator('#messages details.android-chat-tool-group').first();
    assert.equal(await group.count(),1);
    assert.equal(await group.evaluate(node => node.open),false,
      'Tool activity groups should start collapsed');
    assert.equal(await group.locator('details.android-chat-tool').isVisible(),false,
      'Nested tool details should not be visible while group is collapsed');
    await group.locator('summary').first().click();
    const tool = group.locator('details.android-chat-tool').first();
    assert.equal(await tool.count(),1,'Only backend-confirmed native tool evidence is expandable');
    assert.match(await group.innerText(),/Opened Android app/,
      'Collapsed nested tool rows should show the safe action label');
    assert.equal(await tool.evaluate(node => node.open),false);
    await tool.locator('summary').click();
    assert.equal(await tool.evaluate(node => node.open),true);
    const expanded = await group.innerText();
    assert.match(expanded,/Tool: android_app_start/,
      'Expanded verified native action should identify the bounded tool name');
    assert.equal(expanded.includes(SECRET_ARGS),false,
      'Action metadata cannot disclose arbitrary unverified arguments');
    assert.equal(expanded.includes(SECRET_RESULT),false,
      'Action metadata cannot disclose arbitrary unverified result bodies');
    assert.equal(expanded.includes('UNVERIFIED_LONG_RESULT_'),false,
      'Unexpected long result properties must not be rendered as confirmed tool output');
    assert.ok(expanded.length < 3000,
      'Expandable activity details must remain bounded even with hostile long metadata');
    for (const width of [320,360,390]) {
      await page.setViewportSize({width,height:780});
      const geometry = await page.locator('#messages').evaluate(node => {
        const activity = node.querySelector('.android-chat-activity')?.getBoundingClientRect();
        return {visible:node.clientWidth,content:node.scrollWidth,
          activityLeft:activity?.left,activityRight:activity?.right,
          viewport:window.innerWidth};
      });
      assert.ok(geometry.content <= geometry.visible + 2 &&
        (geometry.activityLeft ?? -1) >= -1 &&
        (geometry.activityRight ?? width + 2) <= width + 1,
      `Conversation activity must fit Android at ${width}px: ${JSON.stringify(geometry)}`);
    }
    assert.deepEqual(await page.evaluate(() => window.__activityNativeCalls.map(x => x.action)),
      ['android_app_start'],'Exactly one user initiated native action was submitted');

    // Reload uses the same synthetic native persisted /api/state snapshot,
    // never a fabricated localStorage receipt or a repeated tool invocation.
    await page.reload();
    await page.locator('#sidebarToggle').click();
    await page.locator('#threads')
      .getByText('buka YouTube',{exact:false}).first().click();
    assert.equal(await page.locator('#messages .android-chat-activity').count(),1,
      'Persisted confirmed tool metadata should be visible after restart');
    assert.equal(await page.locator('#messages details.android-chat-tool-group')
      .first().evaluate(node => node.open),false,
    'Reload should not reopen old tool details automatically');
    assert.equal(await page.locator('#messages').evaluate(node =>
      node.scrollWidth <= node.clientWidth + 2),true);
    assert.equal((await page.locator('#messages').innerText()).includes(SECRET_ARGS),false);
    assert.equal((await page.locator('#messages').innerText()).includes(SECRET_RESULT),false);
    await page.close();
  } finally {
    if (browser) await browser.close();
    await server.close();
    await rm(temp,{recursive:true,force:true});
  }
});

test('saved Android assistant Markdown stays formatted, inert and hides tool secrets', {
  timeout:90000
},async () => {
  const temp = await mkdtemp(path.join(os.tmpdir(),'mcp-chat-markdown-'));
  await mkdir(path.join(temp,'workspace'));
  const server = await startServer({home:path.join(temp,'data'),
    workspaceRoot:path.join(temp,'workspace'),openBrowser:false});
  let browser;
  try {
    // Compile into memory, not the shared generated ui/app.js. This verifies
    // the current Android Markdown renderer without editing production files.
    const bundle = await build({
      entryPoints:[fileURLToPath(new URL('../ui/app.ts',import.meta.url))],
      bundle:true,platform:'browser',format:'iife',target:['chrome110','safari16'],
      write:false,sourcemap:false,minify:false,logLevel:'silent'
    });
    assert.equal(bundle.outputFiles.length,1);
    browser = await chromium.launch({headless:true,
      ...(testChromePath() ? {executablePath:testChromePath()} : {})});
    const page = await browser.newPage({viewport:{width:360,height:780},
      isMobile:true,hasTouch:true,
      userAgent:'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36'});
    await page.route('**/app.js',route => route.fulfill({
      status:200,contentType:'application/javascript',body:bundle.outputFiles[0].text
    }));
    await page.addInitScript(({sessionId,rawHtml,secretArgs,secretResult}) => {
      window.isTauri = true;
      window.__cosHtmlExecuted = false;
      const answer = [
        'Use **Mcp Agent Ai Plugins** for the separate tunnel.',
        '',
        'Check `android_plugins_status` before claiming a tool is ready.',
        '',
        '- First: keep verified receipts',
        '- Second: show **confirmed** only after a native response',
        '',
        '```text',
        'native receipt: confirmed',
        '```',
        '',
        'This raw input must remain text: ' + rawHtml
      ].join('\n');
      const saved = {
        id:sessionId, provider:'local-automation',model:'native-agent',
        messages:[
          {role:'user',text:'Show my saved Markdown conversation safely'},
          {role:'assistant',text:answer}
        ],
        // The native backend only persists the bounded metadata fields.
        // Unexpected sensitive fields are injected here to ensure the chat
        // renderer does not treat them as verified tool output.
        actions:[{
          type:'android_local_direct_action',name:'android_app_start',
          approvedByUser:false,userIntentAuthorized:true,nativeConfirmed:true,
          resultIsError:false,resultStored:false,
          arguments:{apiKey:secretArgs},result:{output:secretResult}
        }]
      };
      window.__TAURI_INTERNALS__={invoke:async(command,args={}) => {
        if (command==='api_request' && args.route==='/api/state') {
          const response=await fetch('/api/state');
          return {...await response.json(),platform:'android',
            providers:['chatgpt-browser','openai'],hasKeys:[],sessions:[saved]};
        }
        return {};
      }};
    },{sessionId:MARKDOWN_ID,rawHtml:RAW_UNTRUSTED_HTML,
      secretArgs:SECRET_ARGS,secretResult:SECRET_RESULT});
    await page.goto(server.url);
    await page.locator('#sidebarToggle').click();
    await page.locator('#threads')
      .getByText('Show my saved Markdown conversation safely').click();
    const prose = page.locator('#messages .message.assistant .android-chat-prose');
    await prose.locator('strong').getByText('Mcp Agent Ai Plugins',{exact:true}).waitFor();
    assert.equal(await prose.locator('strong').first().textContent(),'Mcp Agent Ai Plugins');
    const rendered = await prose.innerText();
    assert.ok(rendered.includes('Mcp Agent Ai Plugins'));
    assert.equal(rendered.includes('**Mcp Agent Ai Plugins**'),false,
      'Markdown emphasis should render as strong without literal asterisks');
    assert.equal(await prose.locator('code').getByText('android_plugins_status',{exact:true}).count(),1,
      'Inline backtick content must be a code element');
    assert.equal(await prose.locator('ul.android-chat-list > li').count(),2,
      'Saved bullet list should contain exactly the two supplied items');
    assert.match(await prose.locator('pre.android-chat-code code').innerText(),/native receipt: confirmed/);
    assert.equal((await prose.textContent()).includes(RAW_UNTRUSTED_HTML),true,
      'Untrusted HTML must remain literal visible assistant text');
    assert.equal(await prose.locator('img').count(),0,
      'Raw HTML must never create a real image node in assistant prose');
    assert.equal(await page.evaluate(() => window.__cosHtmlExecuted),false,
      'No untrusted HTML event handler may execute');

    const activity = page.locator('#messages .android-chat-activity');
    assert.equal(await activity.count(),1,
      'Only the explicit confirmed native action receipt should create activity');
    const group=activity.locator('details.android-chat-tool-group');
    await group.locator('summary').first().click();
    await group.locator('details.android-chat-tool summary').click();
    const allTranscriptText=await page.locator('#messages').textContent();
    assert.equal(allTranscriptText.includes(SECRET_ARGS),false,
      'Unverified tool arguments must remain invisible after expansion');
    assert.equal(allTranscriptText.includes(SECRET_RESULT),false,
      'Unverified tool results must remain invisible after expansion');

    // The same saved state after reload must stay inert and never expose
    // unexpected tool properties, even without any live tool operation.
    await page.reload();
    await page.locator('#sidebarToggle').click();
    await page.locator('#threads')
      .getByText('Show my saved Markdown conversation safely').click();
    assert.equal(await prose.locator('strong').first().textContent(),'Mcp Agent Ai Plugins');
    assert.equal(await prose.locator('img').count(),0);
    assert.equal((await prose.textContent()).includes(RAW_UNTRUSTED_HTML),true);
    assert.equal(await page.evaluate(() => window.__cosHtmlExecuted),false);
    assert.equal((await page.locator('#messages').textContent()).includes(SECRET_ARGS),false);
    assert.equal((await page.locator('#messages').textContent()).includes(SECRET_RESULT),false);
    await page.close();
  } finally {
    if (browser) await browser.close();
    await server.close();
    await rm(temp,{recursive:true,force:true});
  }
});
