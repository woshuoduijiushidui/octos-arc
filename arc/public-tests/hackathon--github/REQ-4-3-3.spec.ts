import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-4-3-3
// fixtures: default_branch
//
// Quoted: "Settings" then "Branches" are unique links on their pages; "Default
// branch" is a native select exposing the combobox role with options labeled by
// exact existing branch names; "Update" is the button and "Confirm" the button in
// the confirmation dialog. After the change, opening the repository entry without a
// branch shows "Branch <new default branch name>" and the selector still offers the
// old default. For a non-Admin the combobox and update button must be absent, not
// merely disabled.

async function openBranches(page: any): Promise<void> {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.openSettingsSub(page, /^branches$/i);
}

test('REQ-4-3-3: an Admin sees a native Default branch select listing exact branch names', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openBranches(page);

  const select = page.getByRole('combobox', { name: /default branch/i }).first();
  await expect(select).toBeVisible();
  expect(await select.evaluate((el) => el.tagName)).toBe('SELECT');
  const labels = await select.locator('option').allInnerTexts();
  expect(labels.map((l) => l.trim())).toContain(h.SEED.branch);
});

test('REQ-4-3-3: updating and confirming sets the new default and keeps the old branch selectable', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openBranches(page);

  await h.setCombobox(page, 'Default branch', h.SEED.featureBranch);
  await h.clickNamed(page, /^update$/i);
  const confirm = page.getByRole('button', { name: /^confirm$/i }).first();
  if (await confirm.isVisible().catch(() => false)) await confirm.click();

  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await expect(page.getByRole('button', { name: new RegExp(`^\\s*branch\\s+${h.SEED.featureBranch}\\s*$`, 'i') }).first()).toBeVisible();
  await page.getByRole('button', { name: new RegExp(`^\\s*branch\\s+${h.SEED.featureBranch}\\s*$`, 'i') }).first().click();
  await expect(h.option(page, h.SEED.branch)).toBeVisible();
});

test('REQ-4-3-3: a non-Admin gets no Default branch combobox and no update button', async ({ browser }) => {
  const reviewer = await h.newSession(browser, { username: h.SEED.reviewer.username, password: h.SEED.reviewer.password });
  await reviewer.page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await reviewer.page.getByRole('link', { name: /^settings$/i }).first().click().catch(() => undefined);
  await reviewer.page.getByRole('link', { name: /^branches$/i }).first().click().catch(() => undefined);

  await expect(reviewer.page.getByRole('combobox', { name: /default branch/i })).toHaveCount(0);
  await expect(reviewer.page.getByRole('button', { name: /^update$/i })).toHaveCount(0);
});
