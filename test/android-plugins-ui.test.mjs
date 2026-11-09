import test from 'node:test';
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { mkdtemp, mkdir, rm } from 'node:fs/promises';
import { existsSync, readdirSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { startServer } from '../src/server.mjs';

test('Android Plugins share the Core Tunnel and keep independent module controls',
  {timeout:90000},async () => {
    const temp = await mkdtemp(path.join(os.tmpdir(),'mcp-android-plugins-ui-'));
    await mkdir(path.join(temp,'workspace'));
    const server = await startServer({home:path.join(temp,'data'),
      workspaceRoot:path.join(temp,'workspace'),openBrowser:false});
    let browser;
    try {
      const chromeRoot = path.join(os.homedir(),'AppData','Local','ms-playwright');
      const installed = existsSync(chromeRoot) ? readdirSync(chromeRoot)
        .filter(name => /^chromium-\d+$/.test(name))
        .map(name => path.join(chromeRoot,name,'chrome-win64','chrome.exe'))
        .find(existsSync) : undefined;
      browser = await chromium.launch({headless:true,
        ...(installed ? {executablePath:installed} : {})});
      const page = await browser.newPage({viewport:{width:360,height:800},
        isMobile:true,hasTouch:true,
        userAgent:'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36'});
      await page.addInitScript(() => {
        window.isTauri = true;
        window.__pluginCalls = [];
        window.__coreStatus = {
          state:'disconnected',pollActive:false,toolCallsVerified:0,
          desiredConnected:false,corePluginLinked:false
        };
        window.__pluginStatus = {
          state:'disconnected',pollActive:false,toolCallsVerified:0,
          desiredConnected:false,availableTools:0,tunnelId:null,lastError:null,
          plugins:[
            {id:'web_fetch',name:'Web Fetch',description:'Fetch approved web pages',
              available:true,enabled:false,ready:false,tools:0},
            {id:'browser',name:'Browser',description:'App owned browser',
              available:false,enabled:false,ready:false,tools:0},
            {id:'app_control',name:'App control',description:'Accessibility tools',
              available:false,enabled:false,ready:false,tools:0},
            {id:'security_audit',name:'Security Audit',
              description:'Scan bounded text for security risks without revealing secrets',
              available:true,enabled:false,ready:false,tools:0,category:'security',
              installed:true,installation:'bundled_with_app',updateSupported:false,
              risk:'read_only',version:'0.1.0',sourceLabel:'Bundled Android',
              toolNames:['security_audit_analyze'],permissions:[]},
            {id:'reverse_engineering',name:'DEX Header Inspector',
              description:'Read bounded Android DEX header metadata, without executing an APK',
              available:true,enabled:false,ready:false,tools:0,category:'development',
              risk:'read_only',version:'0.1.0',sourceLabel:'Bundled Android',
              toolNames:['reverse_engineering_dex_header'],permissions:[]}
          ]
        };
        window.__pluginCatalog = {plugins:[
          {id:'web_fetch',name:'Web Fetch',description:'Fetch approved web pages',
            available:true,enabled:false,ready:false,tools:0},
          {id:'browser',name:'Browser',description:'App owned browser',
            available:false,enabled:false,ready:false,tools:0},
          {id:'app_control',name:'App control',description:'Accessibility tools',
            available:false,enabled:false,ready:false,tools:0},
          {id:'memory',name:'Memory',description:'Optional local memory',
            available:false,enabled:false,ready:false,tools:0},
          {id:'security_audit',name:'Security Audit',
            description:'Scan bounded text for security risks without revealing secrets',
            available:true,enabled:false,ready:false,tools:0,category:'security',
            installed:true,installation:'bundled_with_app',updateSupported:false,
            risk:'read_only',version:'0.1.0',sourceLabel:'Bundled Android',
            toolNames:['security_audit_analyze'],permissions:[]},
          {id:'reverse_engineering',name:'DEX Header Inspector',
            description:'Read bounded Android DEX header metadata, without executing an APK',
            available:true,enabled:false,ready:false,tools:0,category:'development',
            risk:'read_only',version:'0.1.0',sourceLabel:'Bundled Android',
            toolNames:['reverse_engineering_dex_header'],permissions:[]}
        ]};
        window.__pluginStatusFail = false;
        window.__pluginOpenFail = false;
        window.__TAURI_INTERNALS__ = {invoke:async(command,args={}) => {
          if (command === 'api_request') {
            if (args.route === '/api/state') {
              const response = await fetch('/api/state');
              return {...await response.json(),platform:'android',
                providers:['chatgpt-browser','openai'],hasKeys:[]};
            }
            return {};
          }
          if (command.startsWith('android_plugins_') || command === 'android_openai_tunnel_status')
            window.__pluginCalls.push({command,args:structuredClone(args)});
          if (command === 'android_openai_tunnel_status')
            return structuredClone(window.__coreStatus);
          if (command === 'android_plugins_status') {
            if (window.__pluginStatusFail) throw Error('unreachable');
            return structuredClone(window.__pluginStatus);
          }
          if (command === 'android_plugins_catalog')
            return structuredClone(window.__pluginCatalog);
          if (command === 'android_plugins_set_enabled') {
            if (args.userInitiated !== true ||
                !['web_fetch','security_audit','reverse_engineering'].includes(args.pluginId))
              throw Error('Unexpected module toggle');
            const entry=window.__pluginStatus.plugins.find(p=>p.id===args.pluginId);
            if (!entry?.available)throw Error('Unavailable plugin cannot be enabled');
            entry.enabled=args.enabled;
            entry.tools=args.enabled ? (entry.id==='web_fetch'?2:1):0;
            window.__pluginStatus.availableTools=window.__pluginStatus.plugins
              .reduce((sum,p)=>sum+p.tools,0);
            return structuredClone(window.__pluginStatus);
          }
          if (command === 'android_plugins_uninstall_bundled' ||
              command === 'android_plugins_install_bundled') {
            if (args.userInitiated !== true || args.pluginId !== 'security_audit')
              throw Error('Unexpected install request');
            const install = command === 'android_plugins_install_bundled';
            const entry=window.__pluginStatus.plugins.find(p=>p.id===args.pluginId);
            if (entry.installed === install) throw Error('Already in requested state');
            entry.installed=install;
            entry.enabled=false;
            entry.ready=false;
            entry.tools=0;
            window.__pluginStatus.availableTools=window.__pluginStatus.plugins.reduce(
              (sum,p)=>sum+p.tools,0);
            return structuredClone(window.__pluginStatus);
          }
          if (command === 'android_plugins_check_update') {
            if(args.pluginId!=='security_audit'||args.userInitiated!==true)
              throw Error('Unexpected plugin update check');
            return {pluginId:args.pluginId,checkSucceeded:false,updateAvailable:false,
              version:null,url:null,reason:'no_published_release',
              automaticInstallSupported:false,signingCompatibilityVerified:false};
          }
          if (command === 'android_tunnel_open_platform_page') {
            window.__pluginCalls.push({command,args:structuredClone(args)});
            if (window.__pluginOpenFail) throw Error('Unable to open browser');
            return {opened:true,kind:args.kind};
          }
          return {};
        }};
      });
      await page.goto(server.url);
      // Unlike the ChatGPT-only legacy QA fixture, this Android state exposes
      // a real API provider separately from ChatGPT website chat.
      await page.locator('#provider option[value="openai"]').waitFor({state:'attached'});
      assert.deepEqual(await page.locator('#provider option').evaluateAll(options =>
        options.map(option => option.value).filter(value =>
          ['chatgpt-browser','openai'].includes(value)).sort()),
        ['chatgpt-browser','openai']);
      await page.locator('#sidebarToggle').click();
      await page.locator('[data-sidebar-extra="plugins"]').click();
      await page.locator('#androidPluginsSettings.android-settings-active').waitFor();
      assert.equal(await page.locator('#settingsPanel').getAttribute('data-settings-view'),'plugins');
      const panelWidth = await page.locator('#androidPluginsSettings').evaluate(node =>
        ({client:node.clientWidth,scroll:node.scrollWidth}));
      assert.ok(panelWidth.scroll <= panelWidth.client + 2,
        `Plugins cards must fit a 360px Android viewport (${JSON.stringify(panelWidth)})`);
      assert.equal(await page.locator('#androidPluginsState').textContent(),'Not connected');
      assert.equal(await page.locator('#androidPluginsRefresh').count(),0,
        'Connection state is checked automatically; no Refresh button');
      assert.equal(await page.locator('#androidPluginsToolCount').textContent(),'0 available tools');
      assert.match(await page.locator('#androidPluginsChatStatus').textContent(),/Core connector not verified/);
      assert.equal(await page.locator('#androidPluginsChatName').inputValue(),'Mcp Agent Ai Core');
      assert.match(await page.locator('#androidPluginsChatDescription').inputValue(),/same Core Tunnel and connector/);
      assert.match(await page.locator('#androidPluginsSettings').textContent(),
        /Use the existing Mcp Agent Ai Core connector/);
      assert.match(await page.locator('#androidPluginsSettings').textContent(),
        /do not add or install a second Plugins connector/);
      assert.equal(await page.getByText('02 · Add to ChatGPT',{exact:true}).count(),0);
      assert.equal(await page.getByText('02 · ChatGPT Core connection',{exact:true}).count(),1);
      assert.match(await page.locator('#androidPluginsOpenChatGPT').textContent(),/Open ChatGPT connectors/);
      assert.match(await page.locator('#androidPluginsCoreStatus').textContent(),
        /Core Tunnel disconnected/,
        'Plugins must report the authoritative Core Tunnel connection');
      for (const id of ['androidPluginsTunnelId','androidPluginsSaveConnect',
        'androidPluginsDisconnect','androidPluginsOpenTunnels']) {
        assert.equal(await page.locator(`#${id}`).count(),0,
          `Obsolete duplicate Plugins connection control ${id} must be absent`);
      }
      assert.match(await page.locator('#androidPluginsSettings').textContent(),
        /same OpenAI Tunnel/);
      assert.equal(await page.locator('#androidPluginsSettings h2').first().textContent(),
        'Plugins Marketplace','Plugins must provide a marketplace, not only automation.');
      assert.match(await page.locator('#androidPluginsMarketplaceSummary').textContent(),
        /0 enabled · 3 available/);
      assert.equal(await page.locator('#androidPluginsModules [data-plugin-id]').count(),6);
      await page.locator('#androidPluginsCategory').selectOption('security');
      assert.equal(await page.locator('#androidPluginsModules [data-plugin-id]').count(),1);
      assert.equal(await page.locator('#androidPluginsModules [data-plugin-id=security_audit]').count(),1);
      await page.locator('#androidPluginsModules [data-plugin-id=security_audit] details').evaluate(
        node=>node.open=true);
      assert.match(await page.locator('#androidPluginsModules [data-plugin-id=security_audit]')
        .textContent(),/Read-only tools/);
      assert.match(await page.locator('#androidPluginsModules [data-plugin-id=security_audit]')
        .textContent(),/security_audit_analyze/);
      await page.locator('#androidPluginsSearch').fill('dex');
      assert.match(await page.locator('#androidPluginsModules').textContent(),
        /No plugins match/,'Search and category filters combine without fake installs');
      await page.locator('#androidPluginsCategory').selectOption('development');
      assert.equal(await page.locator('#androidPluginsModules [data-plugin-id=reverse_engineering]').count(),1);
      await page.locator('#androidPluginsSearch').fill('');
      await page.locator('#androidPluginsCategory').selectOption('all');
      await page.getByRole('button',{name:'Enable Security Audit'}).click();
      await page.getByRole('button',{name:'Disable Security Audit'}).waitFor();
      assert.equal((await page.locator('#androidPluginsMarketplaceSummary').textContent()),
        '1 enabled · 3 available');
      await page.getByRole('button',{name:'Disable Security Audit'}).click();
      await page.getByRole('button',{name:'Enable Security Audit'}).waitFor();
      page.on('dialog',dialog=>dialog.accept());
      assert.equal(await page.getByRole('button',{name:'Uninstall bundled Security Audit'}).count(),1);
      await page.getByRole('button',{name:'Uninstall bundled Security Audit'}).click();
      await page.getByRole('button',{name:'Install bundled Security Audit'}).waitFor();
      assert.equal(await page.getByRole('button',{name:'Enable Security Audit'}).count(),0,
        'Uninstalled module may not expose enable control');
      await page.getByRole('button',{name:'Install bundled Security Audit'}).click();
      await page.getByRole('button',{name:'Uninstall bundled Security Audit'}).waitFor();
      assert.equal(await page.getByRole('button',{name:'Enable Security Audit'}).isDisabled(),false,
        'Reinstalled bundled module remains disabled but may be separately enabled');
      assert.deepEqual(await page.evaluate(()=>window.__pluginCalls.filter(
        entry=>['android_plugins_uninstall_bundled','android_plugins_install_bundled']
          .includes(entry.command)).map(entry=>entry.command)),[
        'android_plugins_uninstall_bundled','android_plugins_install_bundled'
      ]);
      await page.getByRole('button',{name:'Check official update for Security Audit'}).click();
      await page.locator('#androidPluginsModulesFeedback').getByText(
        'official update check unavailable',{exact:false}).waitFor();
      assert.equal(await page.evaluate(()=>window.__pluginCalls.filter(
        call=>call.command==='android_plugins_check_update').length),1,
        'Bundled update check must invoke the native HTTPS metadata reader exactly once');
      assert.equal(await page.locator('#androidPluginsMarketplaceSummary').textContent(),
        '0 enabled · 3 available');
      assert.equal(await page.locator('#androidPluginsModules').getByText('Memory',{exact:true}).isVisible(),true);
      assert.equal(await page.locator('#androidPluginsModules').getByText(
        'ChatGPT can open, read and interact with web pages. No Chrome cookies.').isVisible(),true);
      assert.equal(await page.locator('#androidPluginsModules').getByText(
        'ChatGPT can open a permitted app and stop its session. Not full app control.').isVisible(),true);
      await page.locator('#androidPluginsOpenChatGPT').click();
      assert.match(await page.locator('#androidPluginsChatStatus').textContent(),/Core connector not verified/,
        'Opening ChatGPT connectors must not fabricate a linked Core connector');
      assert.deepEqual(await page.evaluate(() => window.__pluginCalls.filter(
        call=>call.command==='android_tunnel_open_platform_page')), [{
        command:'android_tunnel_open_platform_page',
        args:{kind:'chatgpt_plugins',userInitiated:true}
      }]);
      assert.equal(await page.getByRole('button',{name:'Enable Browser'}).isDisabled(),true);
      await page.getByRole('button',{name:'Enable Web Fetch'}).click();
      await page.getByRole('button',{name:'Disable Web Fetch'}).waitFor();
      assert.equal(await page.locator('#androidPluginsToolCount').textContent(),'2 available tools');
      assert.equal(await page.locator('#androidPluginsModules').getByText(/Waiting for ChatGPT · 2 tools/).isVisible(),true);
      await page.evaluate(() => {
        Object.assign(window.__coreStatus,{state:'polling',pollActive:true,
          desiredConnected:true,toolCallsVerified:0});
        Object.assign(window.__pluginStatus,{state:'polling',pollActive:true,
          desiredConnected:true,toolCallsVerified:0});
      });
      await page.locator('#androidPluginsState').getByText('Core connected').waitFor({timeout:10000});
      assert.match(await page.locator('#androidPluginsCoreStatus').textContent(),/Core Tunnel online/);
      assert.match(await page.locator('#androidPluginsChatStatus').textContent(),/Optional Plugins tools await a verified call/,
        'Core polling is not proof of an optional Plugins tool call');
      await page.evaluate(() => {
        Object.assign(window.__coreStatus,{state:'tool_verified',pollActive:true,
          desiredConnected:true,toolCallsVerified:1,corePluginLinked:true});
      });
      await page.locator('#androidPluginsCoreStatus').getByText(/Core Tunnel verified/).waitFor({timeout:10000});
      assert.equal(await page.locator('#androidPluginsState').textContent(),'Core connected',
        'A verified Core call alone is not proof that an optional Plugins tool was called');
      await page.evaluate(() => {
        Object.assign(window.__pluginStatus,{state:'tool_verified',
          pollActive:true,toolCallsVerified:1});
        window.__pluginStatus.plugins[0].ready = true;
      });
      const beforeAutoCheck = await page.evaluate(() => window.__pluginCalls.filter(
        call=>call.command==='android_plugins_status').length);
      await page.locator('#androidPluginsState').getByText('Tools working').waitFor({timeout:10000});
      assert.match(await page.locator('#androidPluginsChatStatus').textContent(),
        /Core connector active · Optional Plugins tool call verified through Core/,
        'Only a backend verified optional tool receipt confirms Plugins readiness through Core');
      const afterAutoCheck = await page.evaluate(() => window.__pluginCalls.filter(
        call=>call.command==='android_plugins_status').length);
      assert.ok(afterAutoCheck>beforeAutoCheck,'Status must recheck the native backend automatically');
      assert.equal(await page.locator('#androidPluginsModules').getByText('Ready · 2 tools').isVisible(),true);
      await page.evaluate(() => {
        Object.assign(window.__coreStatus,{state:'disconnected',pollActive:false,
          desiredConnected:false,toolCallsVerified:0,corePluginLinked:false});
      });
      await page.locator('#androidPluginsState').getByText('Not connected').waitFor();
      assert.match(await page.locator('#androidPluginsChatStatus').textContent(),/Core connector not verified/);
      await page.evaluate(() => {window.__pluginStatusFail = true;});
      await page.locator('#androidPluginsToolCount').getByText('0 available tools').waitFor({timeout:10000});
      assert.equal(await page.locator('#androidPluginsModules').getByText(/Ready/).count(),0);
      const commands = await page.evaluate(() => window.__pluginCalls.map(call => call.command));
      assert.ok(commands.includes('android_openai_tunnel_status'),
        'Plugins read the existing Core Tunnel status');
      assert.ok(commands.includes('android_plugins_set_enabled'));
      assert.equal(commands.some(name => [
        'android_plugins_save_tunnel_id','android_plugins_connect',
        'android_plugins_disconnect','android_plugins_approval_decide',
        'android_plugins_approvals_status'].includes(name)),false,
      'No separate Plugins tunnel or duplicate approval endpoint may be invoked');
      assert.deepEqual(await page.evaluate(() => window.__pluginCalls
        .filter(call => call.command === 'android_tunnel_open_platform_page')
        .map(call => call.args.kind)),['chatgpt_plugins'],
      'Plugins must not display a duplicate Open Tunnels shortcut');
      await page.close();
    } finally {
      if (browser) await browser.close();
      await server.close();
      await rm(temp,{recursive:true,force:true});
    }
  });
