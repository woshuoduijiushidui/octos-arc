
import { expect, Browser, BrowserContext, Locator, Page } from '@playwright/test';

type Scope = Page | Locator;
type Match = string | RegExp | Array<string | RegExp>;

/**
 * The seeded vocabulary every github requirement assumes. Everything here is
 * quoted verbatim by the requirement text — the seed GIVEN lines, the example
 * values, and the exact error wordings.
 *
 * `alice-dev` is the organisation Owner / repository Admin; `bob-reviewer` is the
 * distinct non-author collaborator with Write permission. The requirements also
 * speak of "a readable non-Admin account" and "a Read viewer" without naming one,
 * so those scenarios build the viewer explicitly (`registerAccount` + `addOrgMember`)
 * — which is also the flow REQ-2-2-3 describes: a Member gains only organisation
 * visibility and no private-repository access.
 */
export const SEED = {
  orgDisplayName: 'Acme Demo',
  orgIdentifier: 'acme-demo',
  owner: { username: 'alice-dev', email: 'alice.dev@example.test', password: 'Valid-password-123!' },
  reviewer: { username: 'bob-reviewer', password: 'Valid-password-123!' },
  repo: 'acme-docs',
  privateRepo: 'secret-research',
  forkName: 'acme-docs-fork',
  branch: 'main',
  featureBranch: 'feature-search',
  secondBranch: 'v1.0',
  readme: 'README.md',
  featureOnlyFile: 'main-only.md',
  commitMessage: 'Document search flow',
  changedFile: 'src/search.ts',
  openIssue: 'Improve onboarding',
  openIssueBody: 'Describe the onboarding improvement.',
  closedIssue: 'Legacy welcome text',
  invalidIssue: 'Original issue title',
  labels: { first: 'bug', second: 'documentation' },
  milestone: 'Q3 launch',
  team: 'frontend-team',
  childTeam: 'frontend-child',
  parentTeam: 'platform-team',
  codeQuery: 'search flow',
  absentQuery: 'no-such-token',
  newPassword: 'New-password-456!',
  requiredPassword: 'Required-password-789!',
  resetPassword: 'Replacement-password-456!',
  resetCode: '123456',
} as const;

export function unique(prefix: string): string {
  return `${prefix}${Date.now().toString(36)}${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function toPatterns(value: Match): RegExp[] {
  const items = Array.isArray(value) ? value : [value];
  return items.map((item) =>
    item instanceof RegExp ? item : new RegExp(escapeRegExp(item).replace(/\s+/g, '\\s+'), 'i'));
}

function target(scope: Scope): any {
  return scope as any;
}

async function firstVisible(locators: Locator[]): Promise<Locator> {
  for (const locator of locators) {
    const candidate = locator.first();
    try {
      if (await candidate.isVisible({ timeout: 500 })) return candidate;
    } catch {
      // continue
    }
  }
  return locators[0].first();
}

function namedLocators(scope: Scope, pattern: RegExp): Locator[] {
  const t = target(scope);
  return [
    t.getByRole('button', { name: pattern }),
    t.getByRole('link', { name: pattern }),
    t.getByRole('menuitem', { name: pattern }),
    t.getByRole('tab', { name: pattern }),
    t.getByRole('option', { name: pattern }),
    t.getByRole('combobox', { name: pattern }),
    t.getByRole('checkbox', { name: pattern }),
    t.getByRole('radio', { name: pattern }),
    t.getByRole('heading', { name: pattern }),
    t.getByLabel(pattern),
    t.getByPlaceholder(pattern),
    t.getByText(pattern),
  ];
}

export async function resolveNamed(scope: Scope, value: Match): Promise<Locator> {
  const patterns = toPatterns(value);
  for (const pattern of patterns) {
    const locator = await firstVisible(namedLocators(scope, pattern));
    try {
      if (await locator.isVisible({ timeout: 200 })) return locator;
    } catch {
      // continue
    }
  }
  return firstVisible(namedLocators(scope, patterns[0]));
}

/** A locator that must NOT be present — for the "hidden, not disabled" rules. */
export function absentCandidates(scope: Scope, value: Match): Locator[] {
  return toPatterns(value).flatMap((pattern) => namedLocators(scope, pattern));
}

export async function openHome(page: Page): Promise<void> {
  await page.goto('/');
}

export async function clickNamed(scope: Scope, value: Match): Promise<void> {
  await (await resolveNamed(scope, value)).click();
}

export async function expectVisible(scope: Scope, value: Match): Promise<void> {
  await expect(await resolveNamed(scope, value)).toBeVisible();
}

export async function expectAbsent(scope: Scope, value: Match): Promise<void> {
  for (const locator of absentCandidates(scope, value)) {
    await expect(locator).toHaveCount(0);
  }
}

export async function fillField(scope: Scope, labelOrPlaceholder: Match, value: string): Promise<void> {
  const patterns = toPatterns(labelOrPlaceholder);
  for (const pattern of patterns) {
    const locator = await firstVisible([
      target(scope).getByLabel(pattern),
      target(scope).getByRole('textbox', { name: pattern }),
      target(scope).getByPlaceholder(pattern),
    ]);
    try {
      if (await locator.isVisible({ timeout: 200 })) {
        await locator.fill(value);
        return;
      }
    } catch {
      // continue
    }
  }
  await (await firstVisible([target(scope).getByRole('textbox'), target(scope).locator('input')])).fill(value);
}

export async function expectErrorText(page: Page, message: string | RegExp): Promise<void> {
  const pattern = message instanceof RegExp ? message : new RegExp(escapeRegExp(message), 'i');
  await expect(page.getByText(pattern).first()).toBeVisible();
}

export async function expectHeading(page: Page, name: string | RegExp): Promise<void> {
  const pattern = name instanceof RegExp ? name : new RegExp(escapeRegExp(name), 'i');
  await expect(page.getByRole('heading', { name: pattern }).first()).toBeVisible();
}

export async function dialog(page: Page, name: string | RegExp): Promise<Locator> {
  const pattern = name instanceof RegExp ? name : new RegExp(`^\\s*${escapeRegExp(name)}\\s*$`, 'i');
  return page.getByRole('dialog', { name: pattern }).first();
}

export async function setCombobox(page: Scope, label: string | RegExp, option: string | RegExp): Promise<void> {
  const t = target(page);
  const box = t.getByRole('combobox', { name: label instanceof RegExp ? label : new RegExp(escapeRegExp(label), 'i') }).first();
  const pattern = option instanceof RegExp ? option : new RegExp(`^\\s*${escapeRegExp(option)}\\s*$`, 'i');
  try {
    await box.selectOption({ label: typeof option === 'string' ? option : undefined });
    return;
  } catch {
    // not a native select
  }
  await box.click();
  const root = typeof t.page === 'function' ? t.page() : t;
  const scoped = t.getByRole('option', { name: pattern }).first();
  if (await scoped.count()) {
    await scoped.click();
    return;
  }
  await root.getByRole('option', { name: pattern }).first().click();
}

// ------------------------------------------------------------------ session

export interface Credentials {
  username: string;
  email?: string;
  password: string;
}

export async function signIn(page: Page, who: { username: string; password: string }): Promise<void> {
  await openHome(page);
  await clickNamed(page, /^sign in$/i);
  await fillField(page, /username or email/i, who.username);
  await fillField(page, /^password$/i, who.password);
  await clickNamed(page, /^sign in$/i);
  await expect(page.getByRole('button', { name: /account menu/i })).toBeVisible();
}

/** REQ-1-1-1's happy path; returns the credentials it registered. */
export async function registerAccount(page: Page, overrides: Partial<Credentials> = {}): Promise<Credentials> {
  const creds: Credentials = {
    username: overrides.username ?? unique('pw-user-'),
    email: overrides.email ?? `${unique('pw-')}@example.test`,
    password: overrides.password ?? SEED.owner.password,
  };
  await openHome(page);
  await clickNamed(page, /^sign in$/i);
  await clickNamed(page, /create an account/i);
  await fillField(page, /^username$/i, creds.username);
  await fillField(page, /^email$/i, creds.email!);
  await fillField(page, /^password$/i, creds.password);
  await fillField(page, /confirm password/i, creds.password);
  await clickNamed(page, /agree to the terms/i);
  await clickNamed(page, /create account/i);
  return creds;
}

/** A separate browser context: the requirement's "isolated browser session". */
export async function newSession(browser: Browser, who?: { username: string; password: string }): Promise<{ context: BrowserContext; page: Page }> {
  const context = await browser.newContext();
  const page = await context.newPage();
  if (who) await signIn(page, who);
  return { context, page };
}

export function signedInUsername(page: Page): Locator {
  return page.getByRole('button', { name: /account menu/i }).first();
}

export async function openAccountMenu(page: Page): Promise<void> {
  await clickNamed(page, /account menu/i);
}

export async function signOut(page: Page): Promise<void> {
  await openAccountMenu(page);
  await clickNamed(page, /sign out/i);
  const scope = await dialog(page, 'Sign out');
  await clickNamed(scope, /confirm sign out/i);
}

// --------------------------------------------------------------- navigation

export async function openYourOrganizations(page: Page): Promise<void> {
  await openAccountMenu(page);
  await clickNamed(page, /your organizations/i);
}

export async function openOrganization(page: Page, name: string | RegExp = /acme/i): Promise<void> {
  await openYourOrganizations(page);
  await clickNamed(page, name);
}

/** Repository entry through the global "Search" searchbox (REQ-3-1). */
export async function searchRepository(page: Page, name: string): Promise<void> {
  const box = page.getByRole('searchbox', { name: /^search$/i }).first();
  await box.click();
  await box.fill(name);
  await box.press('Enter');
}

export async function openRepository(page: Page, owner: string, name: string): Promise<void> {
  await openHome(page);
  await searchRepository(page, `${owner}/${name}`);
  const result = page.getByRole('link', { name: new RegExp(`^\\s*${escapeRegExp(name)}\\s*$`) }).first();
  if (await result.count()) {
    await result.click();
    return;
  }
  await page.goto(`/${owner}/${name}`);
}

export function repoNav(page: Page, label: string | RegExp): Locator {
  const pattern = label instanceof RegExp ? label : new RegExp(`^\\s*${escapeRegExp(label)}\\s*$`, 'i');
  return page.getByRole('link', { name: pattern }).first();
}

export async function openIssues(page: Page): Promise<void> {
  await repoNav(page, 'Issues').click();
}

export async function openPullRequests(page: Page): Promise<void> {
  await repoNav(page, 'Pull requests').click();
}

export async function openSettings(page: Page): Promise<void> {
  await page.getByRole('link', { name: /^settings$/i }).first().click();
}

export async function openSettingsSub(page: Page, sub: string | RegExp): Promise<void> {
  await openSettings(page);
  await page.getByRole('link', { name: sub instanceof RegExp ? sub : new RegExp(`^\\s*${escapeRegExp(sub)}\\s*$`, 'i') }).first().click();
}

export async function expectAccessDenied(page: Page): Promise<void> {
  await expect(
    page.getByText(/access denied|not found|404/i).first(),
  ).toBeVisible();
}

export async function expectPublicMarker(page: Page): Promise<void> {
  await expect(page.getByText(/^\s*public\s*$/i).first()).toBeVisible();
}

export async function expectPrivateMarker(page: Page): Promise<void> {
  await expect(page.getByText(/^\s*private\s*$/i).first()).toBeVisible();
}

export async function searchbox(page: Page, name: string | RegExp): Locator {
  const pattern = name instanceof RegExp ? name : new RegExp(`^\\s*${escapeRegExp(name)}\\s*$`, 'i');
  return page.getByRole('searchbox', { name: pattern }).first();
}

export async function textbox(page: Page, name: string | RegExp): Locator {
  const pattern = name instanceof RegExp ? name : new RegExp(`^\\s*${escapeRegExp(name)}\\s*$`, 'i');
  return page.getByRole('textbox', { name: pattern }).first();
}

export async function option(page: Page, name: string | RegExp): Locator {
  const pattern = name instanceof RegExp ? name : new RegExp(`^\\s*${escapeRegExp(name)}\\s*$`, 'i');
  return page.getByRole('option', { name: pattern }).first();
}
