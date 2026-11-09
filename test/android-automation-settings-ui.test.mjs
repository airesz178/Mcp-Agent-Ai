import test from 'node:test';
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { mkdtemp, mkdir, rm } from 'node:fs/promises';
import { existsSync, readdirSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { startServer } from '../src/server.mjs';

test('Setup Access Fine-tune owns browser/RealTime Screen dropdowns and keeps native policy separate from browser launches',
  {timeout:90000},async () => {
    const dir=await mkdtemp(path.join(os.tmpdir(),'mcp-android-automation-ui-'));
    await mkdir(path.join(dir,'workspace'));
    const server=await startServer({home:path.join(dir,'data'),
      workspaceRoot:path.join(dir,'workspace'),openBrowser:false});
    let browser;
    try {
      const root=path.join(os.homedir(),'AppData','Local','ms-playwright');
      const binary=existsSync(root)?readdirSync(root)
        .filter(name=>/^chromium-\d+$/.test(name))
        .map(name=>path.join(root,name,'chrome-win64','chrome.exe')).find(existsSync):undefined;
      browser=await chromium.launch({headless:true,
        ...(binary?{executablePath:binary}:{})});
      const page=await browser.newPage({viewport:{width:360,height:800},
        isMobile:true,hasTouch:true,
        userAgent:'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36'});
      await page.addInitScript(() => {
        window.isTauri=true;
        window.__automationCalls=[];
        window.__automationPrefs={version:1,androidMode:'realtime',browserMode:'headless',saved:true};
        window.__automationBlockSaves=false;
        window.__automationCapabilitiesReady=false;
        window.__automationHeadless={supported:true,running:false,ready:false,
          profileIsolated:false,offscreen:false,url:'',title:''};
        window.__automationInApp={opened:false,activeTabId:0,
          profileIsolationSupported:true,isolatedProfile:false,tabs:[]};
        window.__automationChrome={supported:true,available:true,canLaunch:true,
          mode:'external_chrome',launchOnly:true,domControl:false};
        window.__mcpConfigureCalls=[];
        window.__mcpCommandRulesCalls=[];
        window.__mcpCommandRulesFailSave=false;
        window.__mcpCommandRulesFailStatus=false;
        window.__mcpWriteRoutesAvailable=false;
        window.__mcpCommandRulesState={
          enabled:false,mode:'allowlist',rules:['pwd','ls','stat *','cat *'],
          available:true,supportedCommands:['pwd','ls','cat','stat']
        };
        const capabilities=['browseFolders','searchFiles','readFiles','fileMetadata',
          'createFiles','editFiles','moveFiles','deleteFiles','browserControl','terminal'];
        const readOnly=['browseFolders','searchFiles','readFiles','fileMetadata'];
        window.__mcpPolicy={preset:'read_only',
          requested:Object.fromEntries(capabilities.map(key=>[key,readOnly.includes(key)])),
          effective:Object.fromEntries(capabilities.map(key=>[key,false])),
          available:Object.fromEntries(capabilities.map(key=>[key,false]))};
        window.__TAURI_INTERNALS__={invoke:async(command,args={}) => {
          if(command==='api_request') {
            if(args.route==='/api/state') {
              const data=await (await fetch('/api/state')).json();
              return {...data,platform:'android',providers:['chatgpt-browser','openai'],hasKeys:[]};
            }
            return {};
          }
          if(command==='android_automation_preferences_status')
            return structuredClone(window.__automationPrefs);
          if(command==='android_mcp_permissions_status')
            return structuredClone(window.__mcpPolicy);
          if(command==='android_mcp_command_rules_status') {
            window.__mcpCommandRulesCalls.push({command,args:structuredClone(args)});
            if(window.__mcpCommandRulesFailStatus) throw Error('status unavailable');
            return structuredClone(window.__mcpCommandRulesState);
          }
          if(command==='android_mcp_command_rules_set') {
            window.__mcpCommandRulesCalls.push({command,args:structuredClone(args)});
            if(window.__mcpCommandRulesFailSave || args.userInitiated !== true)
              throw Error('native policy save denied');
            window.__mcpCommandRulesState={...window.__mcpCommandRulesState,
              enabled:args.enabled,mode:args.mode,rules:structuredClone(args.rules)};
            return structuredClone(window.__mcpCommandRulesState);
          }
          if(command==='android_mcp_permissions_configure') {
            if(args.userInitiated !== true) throw Error('native access change requires a user gesture');
            window.__mcpConfigureCalls.push(structuredClone(args));
            window.__mcpPolicy={preset:args.preset,requested:structuredClone(args.requested),
              effective:Object.fromEntries(capabilities.map(key=>[key,
                window.__mcpWriteRoutesAvailable && ['createFiles','editFiles','moveFiles','deleteFiles'].includes(key) &&
                args.requested[key]===true])),
              available:Object.fromEntries(capabilities.map(key=>[key,
                window.__mcpWriteRoutesAvailable && ['createFiles','editFiles','moveFiles','deleteFiles'].includes(key)]))};
            return structuredClone(window.__mcpPolicy);
          }
          if(command==='android_automation_preferences_set') {
            window.__automationCalls.push({command,args:structuredClone(args)});
            if(window.__automationBlockSaves || !args.userInitiated) throw Error('save rejected');
            window.__automationPrefs={version:1,androidMode:args.androidMode,
              browserMode:args.browserMode,saved:true};
            return structuredClone(window.__automationPrefs);
          }
          if(command==='android_background_browser_status')
            return structuredClone(window.__automationHeadless);
          if(command==='in_app_browser_status')
            return structuredClone(window.__automationInApp);
          if(command==='android_external_chrome_status')
            return structuredClone(window.__automationChrome);
          if(command==='accessibility_automation_mode_status') {
            window.__automationCalls.push({command,args:structuredClone(args)});
            if(window.__automationCapabilitiesReady) return {
              selectedMode:args.selectedMode,effectiveMode:'unavailable',
              fallbackUsed:false,reason:'target_session_required',
              supported:true,enabled:true,connected:true,needsReconnect:false,
              semanticActionAvailable:true,screenCaptureAvailable:true,tapAvailable:true,
              realtimeAvailable:true,screenshotClickAvailable:true};
            return {selectedMode:args.selectedMode,effectiveMode:'unavailable',
              fallbackUsed:false,reason:'semantic_action_and_screenshot_tap_unavailable',
              supported:true,enabled:true,connected:true,needsReconnect:false,
              semanticActionAvailable:false,screenCaptureAvailable:false,tapAvailable:false,
              realtimeAvailable:false,screenshotClickAvailable:false};
          }
          if(command==='android_background_browser_start') {
            window.__automationCalls.push({command,args:structuredClone(args)});
            window.__automationHeadless={supported:true,running:true,ready:true,
              profileIsolated:true,offscreen:true,url:args.url,title:'Example Domain'};
            return structuredClone(window.__automationHeadless);
          }
          if(command==='android_background_browser_stop') {
            window.__automationCalls.push({command,args:structuredClone(args)});
            window.__automationHeadless={supported:true,running:false,ready:false,
              profileIsolated:false,offscreen:false,url:'',title:''};
            return structuredClone(window.__automationHeadless);
          }
          if(command==='in_app_browser_open') {
            window.__automationCalls.push({command,args:structuredClone(args)});
            window.__automationInApp={opened:true,activeTabId:1,
              profileIsolationSupported:true,isolatedProfile:true,
              tabs:[{tabId:1,url:args.url,active:true}]};
            return {opened:true};
          }
          if(command==='in_app_browser_close_all') {
            window.__automationInApp={opened:false,activeTabId:0,
              profileIsolationSupported:true,isolatedProfile:false,tabs:[]};
            return {opened:false};
          }
          if(command==='android_external_chrome_open') {
            window.__automationCalls.push({command,args:structuredClone(args)});
            return {dispatched:true,mode:'external_chrome'};
          }
          return {};
        }};
      });
      await page.goto(server.url);
      await page.locator('#sidebarToggle').click();
      await page.locator('#sidebarCustomize').click();
      await page.locator('[data-android-settings-category="agents"]').click();
      assert.equal(await page.locator('[data-setting-category="agents"] #androidAutomationBrowserMode, [data-setting-category="agents"] #androidAutomationAppMode').count(),0,
        'Global Agents Settings may not duplicate the Automation modes controls');
      await page.locator('#androidAutomationOpenFineTune').click();
      await page.locator('#androidCosSetupChecking').waitFor({state:'hidden'});
      await page.locator('[data-cos-setup-step="tools"][aria-current="step"]').waitFor();
      await page.locator('#androidCosAccessPanel').waitFor({state:'visible'});
      assert.equal(await page.locator('#androidMcpAccessPresets .android-mcp-fine-tune').getAttribute('open'),'');
      assert.deepEqual((await page.locator('#androidMcpAccessPresets .android-mcp-fine-tune .android-mcp-permission-group > summary').allTextContents())
        .map(text=>text.trim().replace(/\s+/g,' ')).map(text=>text.replace(/^(Look at files|Change files|Run programs|Sub-agents|Automation modes).*$/,'$1')),
        ['Look at files','Change files','Automation modes','Run programs','Sub-agents'],
        'Fine-tune permissions must show the five COS-style groups in order');
      assert.equal(await page.locator('#androidMcpAllowLook').isChecked(),true,
        'Existing native file inspection toggles must reflect saved Read-only policy');
      assert.equal(await page.locator('#androidMcpAllowTerminal').isChecked(),false,
        'No terminal permission is invented by an automation dropdown');
      const subagent=page.locator('#androidMcpAccessPresets .android-mcp-permission-group')
        .filter({has:page.getByText('Sub-agents',{exact:true})});
      assert.equal(await subagent.count(),1);
      await subagent.locator(':scope > summary').click();
      assert.match(await subagent.innerText(),/separate|independent|not.*permission|does not grant|no.*grant|requires.*approval/i,
        'Sub-agents must be described truthfully as separate from an automatic device grant');
      assert.equal(await page.locator('#androidAutomationModesGroup').getAttribute('open'),'');
      for(const id of ['androidAutomationBrowserMode','androidAutomationAppMode',
        'androidAutomationBrowserStatus','androidAutomationAppStatus']) {
        assert.equal(await page.locator(`#${id}`).count(),1,`${id} must exist once`);
        assert.equal(await page.locator(`#androidAutomationModesGroup #${id}`).count(),1,
          `${id} belongs to the Setup Access Automation modes group`);
      }
      await page.locator('#androidAutomationBrowserBadge').getByText('Available').waitFor();
      assert.equal(await page.locator('#androidAutomationBrowserMode').inputValue(),'headless');
      assert.equal(await page.locator('#androidAutomationAppMode').inputValue(),'realtime');
      assert.equal((await page.locator('#androidAutomationAppMode option[value="realtime"]')
        .textContent()).trim(),'RealTime Screen');
      assert.equal(await page.locator('#androidAutomationBrowserUrl,#androidAutomationBrowserOpen,#androidAutomationBrowserStop').count(),0,
        'Fine-tune Browser mode is a preference only: no URL field or launch/stop controls');
      assert.equal(await page.locator('#androidAutomationModesGroup .android-automation-readonly-note, #androidAutomationModesGroup .android-automation-links').count(),0,
        'Automation modes must not contain the Web Fetch note or shortcut strip');
      assert.equal(await page.locator('#androidSettingsOpenAgents,#androidSettingsOpenSkills,#androidSettingsOpenPlugins').count(),0,
        'Remove only the obsolete Agents shortcut buttons');
      assert.equal(await page.locator('[data-android-settings-category="plugins"]').count(),1,
        'The real MCP Plugins category must remain available outside Agents');
      assert.match(await page.locator('#androidAutomationAppStatus').textContent(),/unavailable/i);
      const size=await page.locator('#androidAutomationModesGroup').evaluate(el=>
        ({client:el.clientWidth,scroll:el.scrollWidth}));
      assert.ok(size.scroll<=size.client+2,JSON.stringify(size));
      await page.locator('#androidAutomationAppMode').selectOption('screenshot_click');
      await page.waitForFunction(()=>window.__automationPrefs.androidMode==='screenshot_click');
      assert.equal(await page.locator('#androidAutomationAppMode').inputValue(),'screenshot_click');
      assert.equal(await page.locator('#androidAutomationAppBadge').textContent(),'Unavailable');
      assert.match(await page.locator('#androidAutomationAppStatus').textContent(),/unavailable/i);
      const lastProbe=await page.evaluate(()=>window.__automationCalls.filter(
        x=>x.command==='accessibility_automation_mode_status').at(-1));
      assert.equal(lastProbe.args.selectedMode,'screenshot_click');
      assert.equal(await page.locator('#androidAutomationAppMode').inputValue(),'screenshot_click',
        'Native capability failure must never silently change the selected dropdown');
      await page.locator('#androidAutomationBrowserMode').selectOption('right_sidebar');
      await page.waitForFunction(()=>window.__automationPrefs.browserMode==='right_sidebar');
      assert.equal(await page.locator('#androidAutomationBrowserMode').inputValue(),'right_sidebar');
      assert.match(await page.locator('#androidAutomationBrowserDescription').textContent(),/full-screen/);
      await page.locator('#androidAutomationBrowserMode').selectOption('open_browser_app');
      await page.waitForFunction(()=>window.__automationPrefs.browserMode==='open_browser_app');
      assert.equal(await page.locator('#androidAutomationBrowserBadge').textContent(),'Launch only');
      assert.match(await page.locator('#androidAutomationBrowserDescription').textContent(),
        /cannot be read or controlled/);
      await page.locator('#androidAutomationBrowserMode').selectOption('headless');
      await page.waitForFunction(()=>window.__automationPrefs.browserMode==='headless');
      const browserLaunchCalls=await page.evaluate(()=>window.__automationCalls.filter(x=>
        ['in_app_browser_open','android_external_chrome_open',
          'android_background_browser_start','android_background_browser_stop',
          'in_app_browser_close_all'].includes(x.command)));
      assert.deepEqual(browserLaunchCalls,[],
        'Choosing any browser dropdown option cannot open/stop a browser or require a URL');
      assert.deepEqual(await page.evaluate(()=>window.__automationCalls.filter(
        x=>x.command==='android_automation_preferences_set').map(x=>x.args.browserMode).slice(-3)),
        ['right_sidebar','open_browser_app','headless'],
        'Each browser dropdown selection must reach native preference storage without a URL');
      assert.equal(await page.evaluate(()=>window.__automationPrefs.androidMode),'screenshot_click');
      // Fine-tune native file and terminal controls keep their old backend
      // routes, and never overwrite the independent automation preference.
      await page.locator('#androidMcpAccessPresets .android-mcp-permission-group > summary')
        .filter({hasText:'Run programs'}).click();
      await page.locator('#androidMcpCommandRulesStatus')
        .getByText(/Command rule limiting is disabled/).waitFor();
      assert.equal(await page.locator('#androidMcpCommandRulesEnabled').isChecked(),false);
      assert.equal(await page.locator('#androidMcpCommandRulesInput').inputValue(),
        'pwd\nls\nstat *\ncat *');
      assert.equal(await page.locator('input[name="androidMcpCommandRulesMode"]:checked').inputValue(),
        'allowlist');
      assert.equal(await page.locator('#androidMcpCommandRulesSave').isDisabled(),true);
      assert.equal(await page.locator('#androidMcpCommandRulesEnabled').isDisabled(),false);
      await page.locator('#androidMcpCommandRulesEnabled').check();
      await page.locator('#androidMcpCommandRulesInput').fill('git status');
      assert.equal(await page.locator('#androidMcpCommandRulesSave').isDisabled(),true,
        'An unsupported shell command can never be saved or invoked');
      assert.match(await page.locator('#androidMcpCommandRulesStatus').innerText(),/Invalid rules/);
      assert.equal(await page.evaluate(() => window.__mcpCommandRulesCalls
        .filter(call=>call.command==='android_mcp_command_rules_set').length),0,
        'Textarea edits must never directly invoke the native command-rule setter');
      await page.locator('#androidMcpCommandRulesInput').fill('pwd\nls\nstat *\ncat *');
      await page.locator('#androidMcpCommandRulesSave').click();
      await page.locator('#androidMcpCommandRulesStatus')
        .getByText(/Rules saved|Native rules verified/).waitFor({timeout:10000});
      assert.equal(await page.locator('#androidMcpCommandRulesEnabled').isChecked(),true);
      assert.deepEqual(await page.evaluate(() => window.__mcpCommandRulesCalls
        .filter(call=>call.command==='android_mcp_command_rules_set').at(-1).args),
      {enabled:true,mode:'allowlist',rules:['pwd','ls','stat *','cat *'],userInitiated:true},
      'Only explicit Save may update the native command-rule policy');
      await page.locator('#androidMcpCommandRulesModes label')
        .filter({hasText:'Denylist'}).click();
      assert.equal(await page.locator('input[name="androidMcpCommandRulesMode"]:checked')
        .inputValue(),'denylist');
      await page.locator('#androidMcpCommandRulesInput').fill('pwd');
      await page.locator('#androidMcpCommandRulesSave').click();
      await page.locator('#androidMcpCommandRulesSave').waitFor({state:'visible'});
      await page.waitForFunction(() => window.__mcpCommandRulesState.mode==='denylist' &&
        window.__mcpCommandRulesState.rules.join(',')==='pwd');
      assert.deepEqual(await page.evaluate(() => window.__mcpCommandRulesCalls
        .filter(call=>call.command==='android_mcp_command_rules_set').at(-1).args),
      {enabled:true,mode:'denylist',rules:['pwd'],userInitiated:true});
      await page.evaluate(() => {window.__mcpCommandRulesFailSave=true;});
      await page.locator('#androidMcpCommandRulesInput').fill('ls');
      await page.locator('#androidMcpCommandRulesSave').click();
      await page.locator('#androidMcpCommandRulesStatus')
        .getByText(/Save was not verified by native Android/).waitFor();
      assert.equal(await page.locator('#androidMcpCommandRulesSave').isDisabled(),true,
        'Failed native Save must fail closed and invalidate stale UI authority');
      await page.evaluate(() => {window.__mcpCommandRulesFailSave=false;});
      await page.locator('#androidMcpRunProgramsGroup > summary').click();
      await page.locator('#androidMcpRunProgramsGroup > summary').click();
      await page.locator('#androidMcpCommandRulesStatus')
        .getByText(/Rules saved|Native rules verified/).waitFor();
      await page.evaluate(() => {window.__mcpCommandRulesFailStatus=true;});
      await page.locator('#androidMcpRunProgramsGroup > summary').click();
      await page.locator('#androidMcpRunProgramsGroup > summary').click();
      await page.locator('#androidMcpCommandRulesStatus')
        .getByText(/Native command rules could not be verified/).waitFor();
      assert.equal(await page.locator('#androidMcpCommandRulesEnabled').isDisabled(),true);
      assert.equal(await page.locator('#androidMcpCommandRulesSave').isDisabled(),true);
      await page.evaluate(() => {window.__mcpCommandRulesFailStatus=false;});
      await page.locator('#androidMcpRunProgramsGroup > summary').click();
      await page.locator('#androidMcpRunProgramsGroup > summary').click();
      await page.locator('#androidMcpCommandRulesStatus')
        .getByText(/Rules saved|Native rules verified/).waitFor();
      await page.evaluate(() => {window.__mcpCommandRulesState.available=false;});
      await page.locator('#androidMcpRunProgramsGroup > summary').click();
      await page.locator('#androidMcpRunProgramsGroup > summary').click();
      await page.locator('#androidMcpCommandRulesStatus')
        .getByText(/Native command policy is unavailable/).waitFor();
      assert.equal(await page.locator('#androidMcpCommandRulesEnabled').isDisabled(),true,
        'Native unavailable state must disable terminal policy editing');
      assert.equal(await page.locator('#androidMcpCommandRulesSave').isDisabled(),true);
      await page.evaluate(() => {window.__mcpCommandRulesState.available=true;});
      await page.locator('#androidMcpRunProgramsGroup > summary').click();
      await page.locator('#androidMcpRunProgramsGroup > summary').click();
      await page.locator('#androidMcpCommandRulesStatus')
        .getByText(/Rules saved|Native rules verified/).waitFor();
      await page.locator('#androidMcpAllowTerminal').check();
      await page.waitForFunction(()=>window.__mcpPolicy.requested.terminal===true);
      assert.equal(await page.locator('#androidMcpAllowTerminal').isChecked(),true);
      await page.locator('#androidMcpAllowTerminal').uncheck();
      await page.waitForFunction(()=>window.__mcpPolicy.requested.terminal===false);
      await page.locator('#androidMcpAllowLook').uncheck();
      await page.waitForFunction(()=>window.__mcpPolicy.requested.readFiles===false);
      await page.locator('#androidMcpAllowLook').check();
      await page.waitForFunction(()=>window.__mcpPolicy.requested.readFiles===true);
      assert.equal(await page.locator('#androidMcpAllowLook').isChecked(),true);
      assert.ok((await page.evaluate(()=>window.__mcpConfigureCalls.length))>=4);
      assert.deepEqual(await page.evaluate(()=>window.__automationPrefs),
        {version:1,androidMode:'screenshot_click',browserMode:'headless',saved:true},
        'Toggling native file/terminal permissions must not change automation mode preferences');
      assert.equal(await page.evaluate(()=>Object.values(window.__mcpPolicy.effective).some(Boolean)),false,
        'Unsupported native file and terminal tools must never become effective merely from a checkbox');
      await page.evaluate(()=>window.__automationBlockSaves=true);
      await page.locator('#androidAutomationAppMode').selectOption('realtime');
      await page.getByText('Mode was not saved. Previous selection retained.').waitFor();
      assert.equal(await page.locator('#androidAutomationAppMode').inputValue(),'screenshot_click');
      assert.deepEqual(await page.evaluate(()=>window.__automationPrefs),
        {version:1,androidMode:'screenshot_click',browserMode:'headless',saved:true});
      await page.evaluate(()=>{window.__automationCapabilitiesReady=true;window.__automationBlockSaves=false;});
      await page.locator('#androidAutomationAppBadge').getByText('Needs app session')
        .waitFor({timeout:10000});
      assert.match(await page.locator('#androidAutomationAppStatus').textContent(),
        /Start a third-party app session first/,
        'OS capabilities without an active target must not be mistaken for active automation');
      assert.equal(await page.locator('#androidAutomationAppMode').inputValue(),
        'screenshot_click','Unavailable target must preserve selected dropdown');
      // Exact MCP file routes are announced only when native reports
      // corresponding permissions available and effective.
      assert.equal(await page.locator('[data-file-tool="files/apply_patch"] small')
        .textContent(),'Unavailable');
      await page.evaluate(() => {window.__mcpWriteRoutesAvailable=true;});
      await page.locator('#androidMcpAccessPresets .android-mcp-permission-group > summary')
        .filter({hasText:'Change files'}).click();
      await page.locator('#androidMcpAllowChange').check();
      await page.waitForFunction(() => window.__mcpPolicy.effective.editFiles===true);
      for (const name of ['files/create','files/edit','files/apply_patch','files/move','files/delete'])
        assert.equal(await page.locator(`[data-file-tool="${name}"] small`).textContent(),
          'Available',`${name} should appear available only with native effective policy`);
      await page.locator('#androidMcpAllowChange').uncheck();
      await page.waitForFunction(() => window.__mcpPolicy.effective.editFiles===false);
      assert.equal(await page.locator('[data-file-tool="files/apply_patch"] small')
        .textContent(),'Not enabled',
        'Revoked write permission must immediately revoke apply_patch badge');
      await page.close();
    } finally {
      if(browser)await browser.close();
      await server.close();
      await rm(dir,{recursive:true,force:true});
    }
  });
