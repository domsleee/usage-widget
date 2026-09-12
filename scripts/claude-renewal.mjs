import { chromium } from 'playwright';
import { parse } from 'smol-toml';
import { readFile, mkdir, writeFile, rename, access, rm } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { setTimeout as sleep } from 'node:timers/promises';
import { homedir } from 'node:os';
import { dirname, join, resolve, delimiter } from 'node:path';
import { pathToFileURL } from 'node:url';
import { parseArgs, isDeepStrictEqual } from 'node:util';

export function renewalDate(details) {
  if (!details || typeof details !== 'object') throw new Error('Claude returned no subscription details.');
  if (details.plan_ending_at) {
    throw new Error('Claude reports that this subscription is ending, not renewing. Config was not changed.');
  }
  const value = details.next_charge_at ?? details.next_charge_date;
  if (typeof value === 'string' && /^\d{4}-\d{2}-\d{2}$/.test(value)) {
    const parsed = new Date(`${value}T00:00:00Z`);
    if (Number.isFinite(parsed.getTime()) && parsed.toISOString().slice(0, 10) === value) return value;
  } else if ((typeof value === 'string' && /^\d{4}-\d{2}-\d{2}T.*(?:Z|[+-]\d{2}:\d{2})$/.test(value)) || typeof value === 'number') {
    const date = new Date(typeof value === 'number' && value < 100_000_000_000 ? value * 1000 : value);
    if (Number.isFinite(date.getTime())) {
      return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`;
    }
  }
  throw new Error('Claude returned no valid next charge date. Config was not changed.');
}

export async function subscriptionDetails(page, org) {
  const result = await page.evaluate(async org => {
    const response = await fetch(`/api/organizations/${encodeURIComponent(org)}/subscription_details`, {
      signal: AbortSignal.timeout(25_000),
    });
    return { status: response.status, details: response.ok ? await response.json() : null };
  }, org);
  if (result.status !== 200) throw new Error(`Claude subscription lookup returned HTTP ${result.status}. Config was not changed.`);
  return result.details;
}

export function selectOrganization(organizations, expected, active) {
  if (!Array.isArray(organizations)) throw new Error('Claude returned an unexpected organization list.');
  const wanted = expected || active;
  if (wanted) {
    if (organizations.some(org => org.uuid === wanted)) return wanted;
    throw new Error('Sign in to the same Claude account used by Claude Code. Config was not changed.');
  }
  if (organizations.length === 1 && organizations[0].uuid) return organizations[0].uuid;
  throw new Error('Select your personal workspace in Claude, then run the helper again.');
}

export function updateConfig(text, date) {
  const config = parse(text);
  if (config.claude !== undefined && (typeof config.claude !== 'object' || Array.isArray(config.claude))) {
    throw new Error('The claude config must be a TOML table.');
  }
  config.claude ??= {};
  config.claude.renewal_date = date;
  const newline = text.includes('\r\n') ? '\r\n' : '\n';
  const value = JSON.stringify(date);
  const candidates = [];
  // Validate every textual edit semantically, including possible multiline-string decoys.
  for (const match of text.matchAll(/^[ \t]*(?:renewal_date|"renewal_date"|'renewal_date')[ \t]*=[^\r\n]*/gm)) {
    const comment = match[0].match(/[ \t]+#.*$/)?.[0] || '';
    candidates.push(text.slice(0, match.index) + `renewal_date = ${value}${comment}` + text.slice(match.index + match[0].length));
  }
  for (const match of text.matchAll(/^[ \t]*\[(?:claude|"claude"|'claude')\][ \t]*(?:#[^\r\n]*)?(?:\r?\n|$)/gm)) {
    const end = match.index + match[0].length;
    candidates.push(text.slice(0, end) + (match[0].endsWith('\n') ? '' : newline) + `renewal_date = ${value}${newline}` + text.slice(end));
  }
  candidates.push(text + `${newline}[claude]${newline}renewal_date = ${value}${newline}`);
  for (const candidate of candidates) {
    try { if (isDeepStrictEqual(parse(candidate), config)) return candidate; } catch { /* Try another safe edit. */ }
  }
  throw new Error('Cannot safely edit this Claude TOML layout. Add a [claude] table with renewal_date and retry. Config was not changed.');
}

function configDirectory() {
  if (process.platform === 'win32') return join(process.env.APPDATA || join(homedir(), 'AppData', 'Roaming'), 'usage-widget');
  if (process.platform === 'darwin') return join(homedir(), 'Library', 'Application Support', 'usage-widget');
  return join(process.env.XDG_CONFIG_HOME || join(homedir(), '.config'), 'usage-widget');
}

async function optionalText(path) {
  try { return await readFile(path, 'utf8'); }
  catch (error) { if (error.code === 'ENOENT') return ''; throw error; }
}

export function readyForLookup(tab) {
  if (tab.type !== 'page' || !tab.title || /just a moment|verif.*human|security verification/i.test(tab.title)) return false;
  try {
    const url = new URL(tab.url);
    return url.origin === 'https://claude.ai' && /^\/(settings|new|chat|chats|recents|projects|project)(\/|$)/.test(url.pathname);
  } catch { return false; }
}

export async function selectPage(context, ready) {
  const pages = context.pages().filter(page => !page.isClosed());
  const exact = pages.find(page => page.url() === ready.url);
  if (exact) return exact;
  for (const page of pages) {
    const title = await page.title().catch(() => '');
    if (readyForLookup({ type: 'page', title, url: page.url() })) return page;
  }
  return undefined;
}

export async function saveConfig(configPath, date) {
  const original = await optionalText(configPath);
  const updated = updateConfig(original, date);
  await mkdir(dirname(configPath), { recursive: true });
  if (original) await writeFile(`${configPath}.before-renewal`, original, { flag: 'wx' }).catch(error => {
    if (error.code !== 'EEXIST') throw error;
  });
  const temporary = `${configPath}.${process.pid}.tmp`;
  try {
    await writeFile(temporary, updated);
    await rename(temporary, configPath);
  } finally { await rm(temporary, { force: true }); }
}

async function browserExecutable(channel) {
  const edge = channel === 'msedge';
  let candidates;
  if (process.platform === 'win32') {
    candidates = [process.env.PROGRAMFILES, process.env['PROGRAMFILES(X86)'], process.env.LOCALAPPDATA]
      .filter(Boolean).map(root => join(root, edge ? 'Microsoft/Edge/Application/msedge.exe' : 'Google/Chrome/Application/chrome.exe'));
  } else if (process.platform === 'darwin') {
    const app = edge ? 'Microsoft Edge' : 'Google Chrome';
    candidates = [join('/Applications', `${app}.app`, 'Contents/MacOS', app)];
  } else {
    candidates = (process.env.PATH || '').split(delimiter).map(root => join(root, edge ? 'microsoft-edge' : 'google-chrome'));
  }
  for (const path of candidates) {
    try { await access(path); return path; } catch { /* Try the next installation. */ }
  }
  throw new Error(`${edge ? 'Edge' : 'Chrome'} is not installed.`);
}

export async function openBrowser(profile, channel, initialUrl = 'https://claude.ai/settings/billing') {
  const executable = await browserExecutable(channel);
  // A nonzero port keeps Chrome in its normal interactive mode. The dedicated
  // profile is required by Chrome and keeps debugging separate from daily browsing.
  const server = createServer();
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  const child = spawn(executable, [
    `--user-data-dir=${profile}`, `--remote-debugging-port=${port}`,
    '--remote-debugging-address=127.0.0.1', '--no-first-run', '--no-default-browser-check', initialUrl,
  ], { stdio: 'ignore' });
  let launchError;
  child.once('error', error => { launchError = error; });
  const endpoint = `http://127.0.0.1:${port}`;
  const tabs = async () => {
    if (launchError) throw launchError;
    if (child.exitCode !== null || child.signalCode !== null) throw new Error('The helper browser closed. Close any previous helper window and retry.');
    try {
      const response = await fetch(`${endpoint}/json/list`, { signal: AbortSignal.timeout(1500) });
      return response.ok ? await response.json() : [];
    } catch { return []; }
  };
  return { child, endpoint, tabs };
}

export async function closeBrowser(browser, chrome) {
  let timer;
  try {
    await Promise.race([
      (async () => {
        if (!browser) return;
        const session = await browser.newBrowserCDPSession().catch(() => null);
        await session?.send('Browser.close').catch(() => {});
        await browser.close().catch(() => {});
      })(),
      new Promise(resolve => { timer = setTimeout(resolve, 3000); }),
    ]);
  } finally {
    clearTimeout(timer);
    if (chrome.child.exitCode === null && chrome.child.signalCode === null) {
      if (process.platform === 'win32') {
        await new Promise(resolve => {
          const killer = spawn('taskkill.exe', ['/PID', String(chrome.child.pid), '/T', '/F'], { windowsHide: true, stdio: 'ignore' });
          killer.once('error', resolve);
          killer.once('exit', resolve);
        });
      } else { chrome.child.kill(); }
    }
  }
}

export function watchParent(input, onClose) {
  input.once('end', onClose);
  input.resume();
  return () => { input.removeListener('end', onClose); input.pause(); };
}

async function main() {
  const { values } = parseArgs({ options: {
    config: { type: 'string' }, profile: { type: 'string' },
    browser: { type: 'string', default: 'chrome' },
  } });
  if (!['chrome', 'msedge'].includes(values.browser)) throw new Error('Use --browser chrome or --browser msedge.');
  const configPath = resolve(values.config || join(configDirectory(), 'config.toml'));
  // Parse before opening the browser; malformed config is never overwritten.
  parse(await optionalText(configPath));
  const claudeText = await optionalText(join(homedir(), '.claude.json'));
  let expected;
  try { expected = claudeText ? JSON.parse(claudeText).oauthAccount?.organizationUuid : undefined; }
  catch { throw new Error('Cannot read the account from ~/.claude.json: invalid JSON. Repair it before retrying.'); }
  const profile = resolve(values.profile || join(dirname(configPath), 'claude-browser'));
  const chrome = await openBrowser(profile, values.browser);
  let browser;
  // The widget owns the write end of stdin. EOF also detects abrupt parent
  // exit on Unix, where a Windows job object is unavailable.
  const parentClosed = () => { closeBrowser(browser, chrome).finally(() => process.exit(1)); };
  const stopWatching = process.env.USAGE_WIDGET_HELPER === '1' ? watchParent(process.stdin, parentClosed) : () => {};
  try {
    console.log('Complete verification and sign in to Claude in Chrome. Waiting up to 5 minutes…');
    const signInDeadline = phaseDeadline(300_000);
    // Read only Chrome's tab metadata until sign-in is complete. Attaching a
    // browser automation framework during a production challenge is unsupported.
    let ready;
    while (Date.now() < signInDeadline) {
      ready = (await chrome.tabs()).find(readyForLookup);
      if (ready) break;
      await sleep(1000);
    }
    if (!ready) throw new Error('Timed out waiting for Claude verification/sign-in. Config was not changed.');
    browser = await chromium.connectOverCDP(chrome.endpoint);
    const context = browser.contexts()[0];
    const page = await selectPage(context, ready);
    if (!page) throw new Error('Claude tab changed during sign-in. Please retry.');
    let organizations;
    const apiDeadline = phaseDeadline(60_000);
    while (Date.now() < apiDeadline) {
      if (page.isClosed()) throw new Error('Browser closed before the lookup finished.');
      // Same-origin requests use the browser session; no login cookies leave Chrome.
      if (readyForLookup({ type: 'page', title: await page.title().catch(() => ''), url: page.url() })) {
        organizations = await page.evaluate(async () => {
          try {
            const response = await fetch('/api/organizations', { signal: AbortSignal.timeout(10_000) });
            return response.ok ? await response.json() : null;
          } catch { return null; }
        }).catch(() => null); // Login can navigate while the request is pending.
        if (Array.isArray(organizations) && organizations.length) break;
      }
      await sleep(1000);
    }
    if (!Array.isArray(organizations) || !organizations.length) throw new Error('Claude opened, but its workspace API did not respond. Config was not changed. Please retry.');
    const active = await page.evaluate(() => {
      const value = document.cookie.split('; ').find(cookie => cookie.startsWith('lastActiveOrg='));
      return value ? decodeURIComponent(value.slice('lastActiveOrg='.length)) : undefined;
    });
    const org = selectOrganization(organizations, expected, active);
    const date = renewalDate(await subscriptionDetails(page, org));
    await closeBrowser(browser, chrome);
    browser = undefined;
    // Re-read after sign-in so settings edited while waiting are retained.
    await saveConfig(configPath, date);
    console.log(`Saved claude.renewal_date = "${date}" to ${configPath}`);
    console.log('Renewal saved. The widget applies button lookups automatically.');
  } finally {
    stopWatching();
    await closeBrowser(browser, chrome);
  }
}

export function phaseDeadline(budget, now = Date.now()) {
  return now + budget;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
