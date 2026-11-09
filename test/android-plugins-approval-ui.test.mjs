import test from 'node:test';
import assert from 'node:assert/strict';
import {chromium} from 'playwright';
import {mkdtemp, mkdir, rm} from 'node:fs/promises';
import {existsSync, readdirSync} from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {startServer} from '../src/server.mjs';

test('Android Plugins no longer render or poll the removed per-tool approval UI',
  {timeout:90000},async () => {
    const temp = await mkdtemp(path.join(os.tmpdir(),'android-plugin-no-approval-ui-'));
    await mkdir(path.join(temp,'workspace'));
    const server = await startServer({home:path.join(temp,'home'),
      workspaceRoot:path.join(temp,'workspace'),openBrowser:false});
    let browser;
    try {
      const root = path.join(os.homedir(),'AppData','Local','ms-playwright');
      const installed = existsSync(root) ? readdirSync(root)
        .filter(name=>/^chromium-\d+$/.test(name))
        .map(name=>path.join(root,name,'chrome-win64','chrome.exe')).find(existsSync) : undefined;
      browser = await chromium.launch({headless:true,
        ...(installed ? {executablePath:installed} : {})});
      const page = await browser.newPage({viewport:{width:360,height:760},
        isMobile:true,hasTouch:true,
        userAgent:'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36'});
      await page.addInitScript(() => {
        window.isTauri = true;
        window.__pluginCommands = [];
        window.__TAURI_INTERNALS__ = {invoke:async(command,args={}) => {
          if (command === 'api_request') {
            if (args.route === '/api/state') {
              const response = await fetch('/api/state');
              return {...await response.json(),platform:'android',
                providers:['chatgpt-browser','openai'],hasKeys:[]};
            }
            return {};
          }
          if (command.startsWith('android_plugins_'))
            window.__pluginCommands.push(command);
          if (command.startsWith('android_plugins_approval'))
            throw Error('Removed Android Plugins approval endpoint was invoked');
          if (command === 'android_plugins_status') return {
            state:'disconnected',pollActive:false,toolCallsVerified:0,
            desiredConnected:false,plugins:[],availableTools:0,
            tunnelId:null,lastError:null
          };
          if (command === 'android_plugins_catalog') return {plugins:[]};
          return {};
        }};
      });
      await page.goto(server.url);
      assert.equal(await page.locator('#androidPluginsApprovalDialog').count(),0);
      assert.equal(await page.locator('#androidPluginsApprovalApprove').count(),0);
      assert.equal(await page.locator('#androidPluginsApprovalReject').count(),0);
      assert.equal(await page.locator('#androidPluginsApprovalAlways').count(),0);

      await page.locator('#sidebarToggle').click();
      await page.locator('[data-sidebar-extra="plugins"]').click();
      await page.locator('#androidPluginsSettings.android-settings-active').waitFor();
      assert.equal(await page.locator('#androidPluginsAllowedRules').count(),0,
        'Obsolete Always allow rules section must be absent');
      assert.equal(await page.locator('#androidPluginsOpenChatGPT').count(),1,
        'ChatGPT Core connector navigation remains');
      assert.equal(await page.locator('#androidPluginsChatName').inputValue(),
        'Mcp Agent Ai Core','No separate Plugins connector name may be presented');
      assert.match(await page.locator('#androidPluginsSettings').textContent(),
        /No second Plugins connector/);
      assert.equal(await page.locator('#androidPluginsSaveConnect').count(),0);
      assert.equal(await page.locator('#androidPluginsTunnelId').count(),0);
      assert.equal(await page.locator('#androidPluginsDisconnect').count(),0);
      assert.equal(await page.locator('#androidPluginsOpenTunnels').count(),0);
      assert.equal(await page.locator('#androidPluginsCoreStatus').count(),1,
        'Plugins show the shared Core Tunnel status instead of a second connection');
      assert.equal(await page.locator('#androidPluginsModules').count(),1);
      await page.waitForTimeout(1900);
      await page.evaluate(() => {
        document.dispatchEvent(new Event('visibilitychange'));
        window.dispatchEvent(new Event('focus'));
      });
      await page.waitForTimeout(800);
      const commands = await page.evaluate(() => window.__pluginCommands);
      assert.ok(commands.includes('android_plugins_status'),
        'Normal Plugins status still refreshes');
      assert.equal(commands.some(command => command.startsWith('android_plugins_approval')),false,
        'Approval polling, decisions, and revoke endpoints must never be called');
      assert.equal(JSON.stringify(await page.evaluate(() => localStorage))
        .includes('androidPluginsApproval'),false,
        'No approval state is written to browser storage');
      await page.close();
    } finally {
      if (browser) await browser.close();
      await server.close();
      await rm(temp,{recursive:true,force:true});
    }
  });
