import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, existsSync } from 'node:fs';

const text = file => readFileSync(new URL('../' + file, import.meta.url), 'utf8');
const source = text('scripts/patch-tauri-android.mjs');
const kotlin = text('android/StorageAccessPlugin.kt');
const rust = text('src-tauri/src/storage_access.rs');
const manifestUrl = new URL('../src-tauri/gen/android/app/src/main/AndroidManifest.xml', import.meta.url);

test('optional camera, microphone and approximate location are declared without mandatory hardware', () => {
  const names = ['CAMERA', 'RECORD_AUDIO', 'ACCESS_COARSE_LOCATION'];
  for (const name of names)
    assert.match(source, new RegExp('android\\.permission\\.' + name + '(?![A-Z_])'));
  for (const hardware of ['camera', 'camera.autofocus', 'microphone', 'location', 'location.network'])
    assert.ok(source.includes('android.hardware.' + hardware + '" android:required="false"'),
      hardware + ' must not become an install requirement');
  const app = text('ui/app.ts');
  // The optional rows can be shown in Step1, but must never be included in
  // the existing four required OS permissions or in five-of-five progress.
  assert.match(app, /const androidPermissionKeys: AndroidPermission\[\] = \['files', 'overlay', 'notifications', 'battery'\]/);
  assert.doesNotMatch(app, /androidPermissionKeys[^\n]*camera|androidPermissionKeys[^\n]*microphone|androidPermissionKeys[^\n]*location/);
});

test('optional permission status reads actual OS grant separately from four legacy required statuses', () => {
  const allStatuses = kotlin.split('private fun allStatuses()')[1]?.split('@Command')[0] || '';
  for (const legacy of ['files','overlay','notifications','battery'])
    assert.match(allStatuses, new RegExp('result\\.put\\("' + legacy + '"'));
  for (const optional of ['camera','microphone','location'])
    assert.doesNotMatch(allStatuses, new RegExp('result\\.put\\("' + optional + '"'));
  const status = kotlin.split('fun optionalPermissionsStatus(invoke: Invoke)')[1]?.split('@Command')[0] || '';
  assert.match(status, /arrayOf\("camera", "microphone", "location"\)/);
  assert.match(status, /optionalEntry\(name\)/);
  assert.match(kotlin, /activity\.checkSelfPermission\(permission!!\) == PackageManager\.PERMISSION_GRANTED/);
  assert.match(kotlin, /"camera" -> Manifest\.permission\.CAMERA/);
  assert.match(kotlin, /"microphone" -> Manifest\.permission\.RECORD_AUDIO/);
  assert.match(kotlin, /"location" -> Manifest\.permission\.ACCESS_COARSE_LOCATION/);
  assert.match(rust, /fn validate_optional_status\(value: Value\)/);
  assert.match(rust, /"optionalPermissionsStatus"/);
  assert.match(rust, /OPTIONAL_PERMISSION_KEYS: \[&str; 3\] = \["camera", "microphone", "location"\]/);
});

test('optional Allow prompts only Android runtime dialog from explicit action without App Info fallback', () => {
  const request = kotlin.split('fun optionalPermissionsRequest(invoke: Invoke)')[1]?.split('\n  @Command')[0] || '';
  assert.match(request, /if \(!args\.userInitiated \|\| permission == null\)/);
  assert.match(request, /if \(!optionalSupported\(args\.permission\)\)/);
  assert.match(request, /activity\.runOnUiThread/);
  assert.match(request, /activity\.requestPermissions\(arrayOf\(permission\), requestCode\)/);
  assert.match(request, /activity\.checkSelfPermission\(permission\) ==/);
  assert.match(request, /receipt\.put\("opened", !alreadyGranted\)/);
  assert.doesNotMatch(request, /ACTION_APPLICATION_DETAILS_SETTINGS|ACTION_MANAGE|startActivity|grantRuntimePermission|pm grant|Settings\./);
  assert.doesNotMatch(request, /put\("granted"|invoke\.resolve\(allStatuses\(\)\)/);
  assert.match(rust, /if !user_initiated \{/);
  assert.match(rust, /if !valid_optional_permission\(&permission\)/);
  assert.match(rust, /"optionalPermissionsRequest"/);
  assert.match(rust, /"userInitiated":user_initiated/);
  assert.match(rust, /fn validate_optional_request\(value: Value\)/);
});

test('optional OS permission declarations are staged exactly once per generated manifest', () => {
  if (!existsSync(manifestUrl)) return;
  const xml = readFileSync(manifestUrl, 'utf8');
  for (const name of ['CAMERA', 'RECORD_AUDIO', 'ACCESS_COARSE_LOCATION']) {
    const escaped = 'android.permission.' + name;
    assert.equal(xml.split('android:name="' + escaped + '"').length - 1, 1,
      'Android app permission must be declared once: ' + name);
  }
  for (const feature of ['camera', 'microphone', 'location'])
    assert.match(xml, new RegExp('android:required="false"[^>]*'), feature + ' hardware optional');
});
