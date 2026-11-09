// Android-only GeckoView extension manager. No personal Chrome access, and no
// browser/extension is automatically launched, installed or connected.
import { invoke, isTauri } from '@tauri-apps/api/core';

const android = /Android/i.test(navigator.userAgent);
const item = (id: string) => document.getElementById(id)!;
const showStatus = (id: string, message: string) => { item(id).textContent = message; };

type BrowserStatus = { supported:boolean; running:boolean; visible:boolean; headless?:boolean;
  engine:string; message?:string };
type GeckoExtension = { id:string; name:string; version?:string; enabled:boolean;
  builtIn?:boolean };
let busy = false;

function disableWhileBusy(state:boolean) {
  busy = state;
  for (const id of ['customGeckoOpen','customGeckoShow','customGeckoClose',
    'customGeckoInstallProbe','customGeckoRefresh'])
    (item(id) as HTMLButtonElement).disabled = state || !isTauri();
}
async function status() {
  if (!android) return;
  const target = 'customGeckoStatus';
  if (!isTauri()) {
    showStatus(target,'Custom browser needs the native Mcp Agent Ai app; unavailable in web preview.');
    return;
  }
  try {
    const state = await invoke<BrowserStatus>('custom_gecko_status');
    const summary = state.running ? state.visible ? 'visible' : 'hidden' : 'stopped';
    showStatus(target,(state.engine || 'Browser') + ': ' + summary + '. ' + (state.message || ''));
  } catch(error) { showStatus(target,String(error)); }
}

async function refreshGeckoExtensions() {
  const target = item('customGeckoExtensions');
  target.replaceChildren();
  if (!android || !isTauri()) return;
  try {
    const response = await invoke<{extensions:GeckoExtension[]}>('custom_gecko_list_extensions');
    if (!Array.isArray(response.extensions) || response.extensions.length > 40)
      throw Error('Invalid GeckoView extension list');
    if (!response.extensions.length) {
      const message = document.createElement('p');
      message.textContent = 'No GeckoView extensions installed.';
      target.append(message);
      return;
    }
    for (const ext of response.extensions) {
      if (typeof ext.id !== 'string' || ext.id.length > 200 ||
          typeof ext.enabled !== 'boolean') throw Error('Invalid GeckoView extension item');
      const card = document.createElement('div');
      card.className = 'custom-gecko-extension';
      const title = document.createElement('strong');
      title.textContent = ext.name || ext.id;
      const note = document.createElement('small');
      note.textContent = ext.id+' · '+(ext.enabled ? 'Enabled' : 'Disabled')+
        ' · '+(ext.version || 'unknown version');
      const actions = document.createElement('div');
      actions.className = 'custom-browser-actions';
      for (const [action,label] of [
        [ext.enabled ? 'disable' : 'enable', ext.enabled ? 'Disable' : 'Enable'],
        ['remove','Remove']
      ] as const) {
        const button = document.createElement('button');
        button.type = 'button';
        button.className = 'outline';
        button.textContent = label;
        button.addEventListener('click',async () => {
          if (busy || !isTauri()) return;
          if (!window.confirm(label+' extension '+(ext.name || ext.id)+'? Android will ask again.'))
            return;
          disableWhileBusy(true);
          try {
            await invoke('custom_gecko_manage',{id:ext.id,action,userInitiated:true});
            await refreshGeckoExtensions();
          } catch(error) {showStatus('customGeckoStatus',String(error));}
          finally {disableWhileBusy(false);}
        });
        actions.append(button);
      }
      card.append(title,note,actions);
      target.append(card);
    }
  } catch(error) {showStatus('customGeckoStatus',String(error));}
}

function geckoAction(id:string, command:string, args:Record<string,unknown> = {}) {
  item(id).addEventListener('click',async () => {
    if (!android || !isTauri() || busy) return;
    if (id === 'customGeckoClose' &&
        !window.confirm('Close the app-owned GeckoView tab? Its extension data will not be deleted.'))
      return;
    if (id === 'customGeckoInstallProbe' &&
        !window.confirm('Review an original bundled test extension with no website access?'))
      return;
    disableWhileBusy(true);
    try {
      // No extra URL field in Settings. GeckoView's own address bar handles
      // navigation after the user explicitly opens its safe HTTPS starter page.
      const payload = id === 'customGeckoOpen'
        ? {url:'https://example.org/',userInitiated:true}
        : args;
      await invoke(command,payload);
      await status();
      if (id === 'customGeckoOpen' || id === 'customGeckoInstallProbe')
        await refreshGeckoExtensions();
    } catch(error) {showStatus('customGeckoStatus',String(error));}
    finally {disableWhileBusy(false);}
  });
}
geckoAction('customGeckoOpen','custom_gecko_open');
geckoAction('customGeckoShow','custom_gecko_show');
geckoAction('customGeckoClose','custom_gecko_close');
geckoAction('customGeckoInstallProbe','custom_gecko_install_probe',{userInitiated:true});
item('customGeckoRefresh').addEventListener('click',() => {
  void status();
  void refreshGeckoExtensions();
});
disableWhileBusy(false);
// Read-only status at initialization. No browser, profile or extension starts.
void status();
