import test from 'node:test';
import assert from 'node:assert/strict';
import { parse } from 'smol-toml';
import { chromium } from 'playwright';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { renewalDate, selectOrganization, updateConfig, subscriptionDetails, readyForLookup, openBrowser, closeBrowser } from './claude-renewal.mjs';

test('lookup waits for the Claude app, not verification or login pages', () => {
  const tab = { type: 'page', title: 'Claude', url: 'https://claude.ai/settings/billing' };
  assert.equal(readyForLookup(tab), true);
  assert.equal(readyForLookup({ ...tab, url: 'https://claude.ai/new' }), true);
  for (const change of [
    { title: 'Just a moment...' }, { title: 'Verify you are human' }, { title: '' },
    { url: 'https://claude.ai/login' }, { url: 'https://example.com/settings/billing' },
    { type: 'service_worker' },
  ]) assert.equal(readyForLookup({ ...tab, ...change }), false);
});

test('regular Chrome launches independently and allows a later Playwright connection', async () => {
  const profile = await mkdtemp(join(tmpdir(), 'usage-widget-browser-test-'));
  let chrome;
  let browser;
  try {
    chrome = await openBrowser(profile, 'chrome', 'about:blank');
    let tabs = [];
    for (let attempt = 0; attempt < 30 && !tabs.length; attempt++) {
      tabs = await chrome.tabs();
      if (!tabs.length) await sleep(200);
    }
    assert.ok(tabs.some(tab => tab.url === 'about:blank'));
    browser = await chromium.connectOverCDP(chrome.endpoint);
    const page = browser.contexts()[0].pages()[0];
    assert.equal(await page.evaluate(() => navigator.webdriver), false);
    await closeBrowser(browser, chrome);
    for (let attempt = 0; attempt < 20 && chrome.child.exitCode === null && chrome.child.signalCode === null; attempt++) await sleep(50);
    assert.ok(chrome.child.exitCode !== null || chrome.child.signalCode !== null);
  } finally {
    await browser?.close().catch(() => {});
    if (chrome?.child.exitCode === null) chrome.child.kill();
    // Only remove this test's newly created temporary profile.
    await rm(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  }
});

test('renewal dates accept calendar dates and timestamps and reject missing or cancelled renewals', () => {
  assert.equal(renewalDate({ next_charge_date: '2028-02-29' }), '2028-02-29');
  const stamp = new Date(2026, 9, 12, 12).getTime();
  for (const value of [stamp, stamp / 1000, new Date(stamp).toISOString()]) {
    assert.equal(renewalDate({ next_charge_at: value }), '2026-10-12');
  }
  for (const value of ['2026-02-30', 'tomorrow', '', null, undefined]) {
    assert.throws(() => renewalDate({ next_charge_date: value }));
  }
  assert.throws(() => renewalDate({ plan_ending_at: '2026-10-12', next_charge_date: '2026-10-12' }), /ending/);
});

test('workspace selection follows Claude Code and never guesses between accounts', () => {
  const orgs = [{ uuid: 'personal' }, { uuid: 'work' }];
  assert.equal(selectOrganization(orgs, 'personal', 'work'), 'personal');
  assert.equal(selectOrganization(orgs, undefined, 'work'), 'work');
  assert.equal(selectOrganization([orgs[0]]), 'personal');
  assert.throws(() => selectOrganization(orgs, 'another-account', 'work'), /same Claude account/);
  assert.throws(() => selectOrganization(orgs), /Select your personal workspace/);
});

test('config update retains settings and replaces only the renewal value', () => {
  const original = 'opacity = 100\n[copilot]\nenabled = false\n[claude]\nestimate = true\nrenewal_date = "2026-09-12"\n[codex]\nestimate = true\n';
  const expected = parse(original);
  expected.claude.renewal_date = '2026-10-12';
  assert.deepEqual(parse(updateConfig(original, '2026-10-12')), expected);
  assert.equal(parse(updateConfig('', '2026-10-12')).claude.renewal_date, '2026-10-12');
  assert.throws(() => updateConfig('opacity = [broken', '2026-10-12'));
});

test('browser lookup uses the signed-in same-origin session and handles rejection', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true, chromiumSandbox: true });
  try {
    const context = await browser.newContext();
    await context.addCookies([{ name: 'sessionKey', value: 'test-session', domain: 'claude.ai', path: '/', httpOnly: true, secure: true }]);
    let status = 200;
    let observedCookie;
    await context.route('https://claude.ai/**', async route => {
      if (route.request().url().endsWith('/subscription_details')) {
        observedCookie = await route.request().headerValue('cookie');
        await route.fulfill({ status, json: { next_charge_date: '2026-10-12' } });
      } else {
        await route.fulfill({ contentType: 'text/html', body: '<html>Test billing page</html>' });
      }
    });
    const page = await context.newPage();
    await page.goto('https://claude.ai/settings/billing');
    assert.equal(renewalDate(await subscriptionDetails(page, 'personal')), '2026-10-12');
    assert.match(observedCookie, /sessionKey=test-session/);
    status = 403;
    await assert.rejects(subscriptionDetails(page, 'personal'), /HTTP 403/);
  } finally {
    await browser.close();
  }
});

test('config edits preserve comments, CRLF and the first backup', async () => {
  const { saveConfig } = await import('./claude-renewal.mjs');
  const { readFile, writeFile } = await import('node:fs/promises');
  const original = '# settings\r\nopacity = 100 # solid\r\n[claude] # account\r\n# keep this\r\nrenewal_date = "2026-09-12" # billing\r\nestimate = true\r\n';
  assert.equal(updateConfig(original, '2026-10-12'), original.replace('2026-09-12', '2026-10-12'));
  const folder = await mkdtemp(join(tmpdir(), 'usage-widget-config-test-'));
  const path = join(folder, 'config.toml');
  try {
    await writeFile(path, original);
    await saveConfig(path, '2026-10-12');
    await saveConfig(path, '2026-11-12');
    assert.equal(await readFile(`${path}.before-renewal`, 'utf8'), original);
    assert.equal(parse(await readFile(path, 'utf8')).claude.renewal_date, '2026-11-12');
  } finally { await rm(folder, { recursive: true, force: true }); }
});

test('tab selection tolerates navigation after sign-in', async () => {
  const { selectPage } = await import('./claude-renewal.mjs');
  const changed = { url: () => 'https://claude.ai/new', title: async () => 'Claude', isClosed: () => false };
  const closed = { url: () => 'https://claude.ai/settings/billing', isClosed: () => true };
  assert.equal(await selectPage({ pages: () => [closed, changed] }, { url: 'https://claude.ai/settings/billing' }), changed);
});

test('first lookup preserves the actual widget config template', async () => {
  const { readFile } = await import('node:fs/promises');
  const source = await readFile(new URL('../src/config.rs', import.meta.url), 'utf8');
  const template = source.match(/const TEMPLATE: &str = r#"([\s\S]*?)"#;/)?.[1];
  assert.ok(template, 'Rust config template must be found');
  const updated = updateConfig(template, '2026-10-12');
  const expected = parse(template);
  expected.claude.renewal_date = '2026-10-12';
  assert.deepEqual(parse(updated), expected);
  for (const line of template.split('\n').filter(line => line.trimStart().startsWith('#'))) assert.ok(updated.includes(line));
});

test('additional app routes are eligible but authentication pages are not', () => {
  for (const route of ['/chats', '/recents', '/projects', '/project/123']) {
    assert.equal(readyForLookup({ type: 'page', title: 'Claude', url: `https://claude.ai${route}` }), true);
  }
  for (const route of ['/login', '/login/callback', '/signup', '/oauth/authorize']) {
    assert.equal(readyForLookup({ type: 'page', title: 'Claude', url: `https://claude.ai${route}` }), false);
  }
});

test('closing the parent pipe closes the dedicated browser without CDP attachment', { timeout: 15000 }, async () => {
  const { spawn } = await import('node:child_process');
  const { once } = await import('node:events');
  const profile = await mkdtemp(join(tmpdir(), 'usage-widget-parent-test-'));
  const moduleUrl = new URL('./claude-renewal.mjs', import.meta.url).href;
  const script = `import {openBrowser, closeBrowser, watchParent} from ${JSON.stringify(moduleUrl)};
    const chrome = await openBrowser(${JSON.stringify(profile)}, 'chrome', 'about:blank');
    watchParent(process.stdin, () => closeBrowser(undefined, chrome).then(() => process.exit(0)));
    for (let i=0; i<50; i++) { if ((await chrome.tabs()).length) break; await new Promise(r=>setTimeout(r,100)); }
    process.stdout.write('ready');`;
  const child = spawn(process.execPath, ['--input-type=module', '-e', script], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
  let timer;
  try {
    const exit = once(child, 'exit');
    await Promise.race([
      once(child.stdout, 'data'),
      exit.then(() => { throw new Error('Helper exited before browser readiness'); }),
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('Browser did not start')), 10000); }),
    ]);
    clearTimeout(timer);
    child.stdin.end();
    assert.equal((await exit)[0], 0);
  } finally {
    clearTimeout(timer);
    child.stdin.end();
    if (child.exitCode === null) child.kill();
    await rm(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  }
});
