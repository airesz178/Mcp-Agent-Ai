import test from 'node:test';
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { mkdtemp, mkdir, rm } from 'node:fs/promises';
import { existsSync, readdirSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { startServer } from '../src/server.mjs';

test('Android chat directly runs a named app or HTTPS link, and API provider offers tools without manual controls',
  {timeout:90000}, async () => {
    const home = await mkdtemp(path.join(os.tmpdir(),'mcp-chat-first-'));
    await mkdir(path.join(home,'workspace'));
    const app = await startServer({home:path.join(home,'data'),
      workspaceRoot:path.join(home,'workspace'),openBrowser:false});
    let browser;
    try {
      const root = path.join(os.homedir(),'AppData','Local','ms-playwright');
      const installed = existsSync(root) ? readdirSync(root)
        .filter(name => /^chromium-\d+$/.test(name))
        .map(name => path.join(root,name,'chrome-win64','chrome.exe'))
        .find(existsSync) : undefined;
      browser = await chromium.launch({headless:true,
        ...(installed ? {executablePath:installed} : {})});
      const page = await browser.newPage({viewport:{width:390,height:844},
        isMobile:true,hasTouch:true,
        userAgent:'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36'});
      await page.addInitScript(() => {
        window.isTauri = true;
        window.__qaModelKey = false;
        window.__qaCalls = [];
        window.__qaSessions = [];
        window.__qaOAuthCalls = 0;
        const localSessionId = 'd0d36908-9d36-46e8-a200-40db352ddf01';
        const modelSessionId = 'f28638d5-f64a-48da-9c08-44e40f9673ee';
        window.__TAURI_INTERNALS__ = {invoke:async(command,args={}) => {
          if (command === 'chatgpt_oauth_chat') { window.__qaOAuthCalls++; return {}; }
          if (command !== 'api_request') return {};
          if (args.route === '/api/state') {
            const response = await fetch('/api/state');
            return {...await response.json(),platform:'android',
              providers:['chatgpt-browser','openai','anthropic','openrouter'],
              hasKeys:window.__qaModelKey ? ['openai'] : [],
              sessions:structuredClone(window.__qaSessions)};
          }
          if (args.route === '/api/key') {
            window.__qaModelKey = args.body.provider === 'openai' && !!args.body.key;
            return {configured:window.__qaModelKey};
          }
          if (args.route === '/api/chat-local-action') {
            window.__qaCalls.push({route:args.route,body:structuredClone(args.body)});
            const {prompt,action,arguments:values} = args.body;
            if (action === 'android_app_start' &&
                (prompt !== 'buka YouTube' || values.query !== 'YouTube'))
              return {error:'Wrong app authorization'};
            if (action === 'android_browser_start' &&
                (prompt !== 'https://example.org/' || values.url !== prompt))
              return {error:'Wrong browser authorization'};
            const old = window.__qaSessions.find(s => s.id === localSessionId);
            const answer = action === 'android_app_start'
              ? 'Android confirmed YouTube launch; blue indicator is OS controlled.'
              : 'Android confirmed offscreen HTTPS browser started.';
            const messages = [...(old?.messages || []),{role:'user',text:prompt},{role:'assistant',text:answer}];
            const actions = [...(old?.actions || []),{type:'android_local_direct_action',name:action,userIntentAuthorized:true}];
            window.__qaSessions = [{id:localSessionId,provider:'local-automation',model:'native-agent',
              messages,actions},...window.__qaSessions.filter(s => s.id !== localSessionId)];
            return {sessionId:localSessionId,answer,actions:[actions.at(-1)]};
          }
          if (args.route === '/api/chat') {
            window.__qaCalls.push({route:args.route,body:structuredClone(args.body)});
            if (!window.__qaModelKey || args.body.provider !== 'openai' ||
                args.body.allowLocalTools !== true || args.body.allowMcpTools !== false)
              return {error:'Model tool access missing'};
            const answer = 'AI used a native status tool and completed its answer.';
            const actions = [{type:'android_local_tool_call',name:'android_browser_status',nativeConfirmed:true}];
            window.__qaSessions = [{id:modelSessionId,provider:'openai',model:'gpt-4.1-mini',
              messages:[{role:'user',text:args.body.prompt},{role:'assistant',text:answer}],actions},
              ...window.__qaSessions.filter(s => s.id !== modelSessionId)];
            return {sessionId:modelSessionId,answer,actions};
          }
          if (args.route === '/api/chat-tool-review')
            throw Error('Chat-first native tools must not use a manual approval panel');
          return {};
        }};
      });
      await page.goto(app.url);
      assert.equal(await page.locator('#androidAutomationEngines,#androidLocalToolsOptIn').count(),0);
      await page.locator('#prompt').fill('buka YouTube');
      await page.locator('#send').click();
      await page.getByText('Android confirmed YouTube launch; blue indicator is OS controlled.').waitFor();
      await page.locator('#prompt').fill('https://example.org/');
      await page.locator('#send').click();
      await page.getByText('Android confirmed offscreen HTTPS browser started.').waitFor();
      const direct = await page.evaluate(() => ({calls:window.__qaCalls,oauth:window.__qaOAuthCalls}));
      assert.deepEqual(direct.calls.map(x => x.route),['/api/chat-local-action','/api/chat-local-action']);
      assert.equal(direct.oauth,0,'Direct user commands must not claim ChatGPT OAuth tool calls');
      assert.equal(await page.locator('#chatToolApprovalPanel').isVisible(),false);

      await page.locator('#sidebarToggle').click();
      await page.locator('#sidebarCustomize').click();
      await page.locator('[data-android-settings-category="setup"]').click();
      await page.locator('[data-cos-setup-step="key"]').click();
      assert.equal(await page.locator('#androidAgentProvider').isVisible(),true);
      await page.locator('#androidAgentProvider').selectOption('openai');
      await page.locator('#androidAgentModel').fill('gpt-4.1-mini');
      await page.locator('#androidAgentApiKey').fill('sk-synthetic-test-key');
      await page.locator('#androidAgentSaveKey').click();
      await page.waitForFunction(() => window.__qaModelKey === true);
      await page.locator('#androidSettingsBack').click();
      await page.locator('#settingsClose').click();
      await page.locator('#prompt').fill('Check native background browser status and explain it');
      await page.locator('#send').click();
      await page.getByText('AI used a native status tool and completed its answer.').waitFor();
      const calls = await page.evaluate(() => window.__qaCalls);
      assert.deepEqual(calls.map(x => x.route),
        ['/api/chat-local-action','/api/chat-local-action','/api/chat']);
      assert.equal(calls[2].body.allowLocalTools,true);
      assert.equal(await page.locator('#chatToolApprovalPanel').isVisible(),false);
      await page.close();
    } finally {
      if (browser) await browser.close();
      await app.close();
      await rm(home,{recursive:true,force:true});
    }
  });
