import test from 'node:test';
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { mkdtemp, mkdir, rm } from 'node:fs/promises';
import { existsSync, readdirSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { startServer } from '../src/server.mjs';

test('Android Prime right drawer shows only real workers, expandable history and active spinner',
  {timeout:90000},async()=>{
    const temp=await mkdtemp(path.join(os.tmpdir(),'mcp-android-prime-drawer-'));
    await mkdir(path.join(temp,'workspace'));
    const server=await startServer({home:path.join(temp,'data'),
      workspaceRoot:path.join(temp,'workspace'),openBrowser:false});
    let browser;
    try{
      const chromeRoot=path.join(os.homedir(),'AppData','Local','ms-playwright');
      const installed=existsSync(chromeRoot)?readdirSync(chromeRoot)
        .filter(name=>/^chromium-\d+$/.test(name))
        .map(name=>path.join(chromeRoot,name,'chrome-win64','chrome.exe'))
        .find(existsSync):undefined;
      browser=await chromium.launch({headless:true,
        ...(installed?{executablePath:installed}:{})});
      const page=await browser.newPage({viewport:{width:360,height:740},
        isMobile:true,hasTouch:true,
        userAgent:'Mozilla/5.0 (Linux; Android 11; RMX3171) AppleWebKit/537.36 Mobile Safari/537.36'});
      await page.addInitScript(()=>{
        window.isTauri=true;
        window.__primeCalls=[];
        window.__primeSnapshot={
          maxConcurrentWorkers:2,
          task:{
            id:'aa813f83-cc65-4ecd-b7e7-e1d3104978c8',
            goal:'Review the Android browser and Prime',
            status:'active',summary:null,workers:[{
              id:'f663c230-bb10-4d45-98ef-a21b330d7420',
              label:'code',assignment:'Check real Android UI layout',
              provider:'openai',model:'gpt-test-verified-model',
              status:'running',reports:[],error:null
            }]
          },
          history:[{
            id:'08bea38a-0a9a-47f9-93ee-39882aa18f93',
            goal:'Previous verified Android review',
            status:'finished',summary:'Checked manually',
            summaryUserWritten:true,summaryTruncated:false,autoVerified:false,
            workers:[{
              id:'f454e90e-7445-4e1c-9a5a-8c10219db480',
              label:'review',assignment:'Check a bounded report',
              provider:'openai',model:'gpt-test-verified-model',
              status:'finished',reportCount:1,verifiedReportCount:1,
              providerVerified:true,error:null
            }]
          }]
        };
        window.__TAURI_INTERNALS__={invoke:async(command,args={})=>{
          if(command==='api_request'){
            if(args.route==='/api/state'){
              const response=await fetch('/api/state');
              return {...await response.json(),platform:'android',
                providers:['chatgpt-browser','openai'],hasKeys:[],
                toolPanels:{review:true,terminal:true,files:true,subagents:true}};
            }
            if(args.route==='/api/mobile-prime'){
              window.__primeCalls.push(structuredClone(args.body));
              if(args.body.action!=='status')
                throw Error('A status view cannot mutate Prime or dispatch provider calls');
              return structuredClone(window.__primeSnapshot);
            }
            return {};
          }
          if(command==='android_openai_tunnel_status')
            return {state:'disconnected',pollActive:false,toolCallsVerified:0,
              desiredConnected:false};
          return {};
        }};
      });
      await page.goto(server.url);
      await page.locator('#androidPrimeChatActivity').waitFor({state:'visible',timeout:15000});
      assert.match(await page.locator('#androidPrimeChatActivity').textContent(),
        /Prime · Worker running/);
      assert.equal(await page.locator('#androidPrimeChatActivity .android-prime-running-spinner').count(),1);
      assert.equal(await page.locator('#androidSubagentsToggle').getAttribute('data-running'),'true');
      await page.locator('#androidPrimeChatActivity summary').click();
      assert.match(await page.locator('#androidPrimeChatActivity').textContent(),
        /code · Running/);
      await page.locator('#androidSubagentsToggle').click();
      await page.locator('#toolViewPanel[data-workspace-panel="subagents"]').waitFor({state:'visible'});
      assert.equal(await page.locator('#androidPrimeActiveCount').textContent(),'Active · 1 / 2');
      assert.equal(await page.locator('#androidPrimeActiveRows [data-prime-task-id]').count(),1);
      assert.equal(await page.locator('#androidPrimeHistoryRows [data-prime-task-id]').count(),1);
      assert.equal(await page.locator('#androidPrimeHistoryCount').textContent(),'1');
      assert.equal(await page.locator('#androidPrimeAdvanced').evaluate(node=>node.open),false,
        'Advanced forms must not overwhelm the right sidebar');
      await page.locator('#androidPrimeActiveRows .android-prime-worker-row summary').click();
      assert.match(await page.locator('#androidPrimeActiveRows').textContent(),/Provider: openai/);
      assert.equal(await page.locator('#androidPrimeActiveRows').getByRole('button',{name:'Finish'}).isDisabled(),true,
        'A provider request still running cannot be falsely marked finished');
      assert.equal(await page.locator('#androidPrimeActiveRows').getByRole('button',{name:'Cancel'}).isEnabled(),true,
        'A running worker retains its bounded cancellation control');
      await page.locator('#androidPrimeHistoryRows .android-prime-worker-row summary').first().click();
      assert.match(await page.locator('#androidPrimeHistoryRows').textContent(),
        /1 verified provider reply\/replies/);
      const screenshots=path.join(process.cwd(),'work','ui-qa');
      await mkdir(screenshots,{recursive:true});
      await page.screenshot({path:path.join(screenshots,'native-android-prime-right-drawer-360.png'),fullPage:true});
      const firstRect=await page.locator('#toolViewPanel').boundingBox();
      assert.ok(firstRect.width<=361&&firstRect.x>=0,'Phone must use a full-width right drawer');
      assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+2),
        '360px phone must not gain horizontal overflow');
      await page.setViewportSize({width:320,height:700});
      assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+2),
        '320px Android must not overflow with the extra Sub-agents header shortcut');
      assert.ok(await page.locator('#androidSubagentsToggle').isVisible());
      await page.locator('#androidPrimeOptionsToggle').click();
      assert.equal(await page.locator('#androidPrimeOptionsToggle').getAttribute('aria-expanded'),'true');
      await page.locator('[data-prime-option="refresh"]').click();
      assert.equal(await page.locator('#androidPrimeOptionsToggle').getAttribute('aria-expanded'),'false');
      await page.locator('#androidPrimeOptionsToggle').click();
      await page.locator('[data-prime-option="manage"]').click();
      assert.equal(await page.locator('#androidPrimeAdvanced').evaluate(node=>node.open),true,
        'Three-dot controls must genuinely reveal the existing Prime management form');
      const commands=await page.evaluate(()=>window.__primeCalls);
      assert.ok(commands.length>=2&&commands.every(call=>call.action==='status'),
        'Drawer and refresh must be strictly read-only; no model starts without user action');
      await page.setViewportSize({width:900,height:740});
      const tabletRect=await page.locator('#toolViewPanel').boundingBox();
      assert.ok(tabletRect.width<=413&&tabletRect.x>=450,
        'Tablet must retain chat beside a right-anchored narrow drawer');
      await page.screenshot({path:path.join(screenshots,'native-android-prime-right-drawer-tablet.png'),fullPage:true});
      await page.locator('#androidSubagentsToggle').click();
      assert.equal(await page.locator('#toolViewPanel').isHidden(),true);
      assert.equal(await page.locator('#androidSubagentsToggle').getAttribute('aria-expanded'),'false');
      await page.close();
    }finally{
      if(browser)await browser.close();
      await server.close();
      await rm(temp,{recursive:true,force:true});
    }
  });
