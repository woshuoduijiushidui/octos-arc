import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-6-1
// fixtures: branch_protection
//
// Quoted: Settings then Branches; the "Add branch protection rule" button; the form's
// field "Branch name pattern" (an exact branch name, no wildcard semantics), the
// checkboxes "Require 1 approval" and "Require status check test", and the "Create"
// button (an existing rule uses "Save changes"). After saving and reloading the branch
// name is visible verbatim with the summaries "1 approval" and "Require status check
// test". For a non-Admin the button is absent. The PR detail page has a Checks area
// showing "test: pending" with an Admin-only combobox "test status" offering
// "success" and a "Save" button, and the result identifies setter and time.

const RULE_BRANCH = 'protection-check';

async function openBranches(page: any, who = h.SEED.owner): Promise<void> {
  await h.signIn(page, who);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.openSettingsSub(page, /^branches$/i);
}

test('REQ-6-1: the rule form exposes the pattern field, both requirement toggles and Create', async ({ page }) => {
  await openBranches(page);
  await h.clickNamed(page, /add branch protection rule/i);

  await expect(page.getByLabel(/branch name pattern/i)).toBeVisible();
  await expect(page.getByRole('checkbox', { name: /require 1 approval/i }).first()).toBeVisible();
  await expect(page.getByRole('checkbox', { name: /require status check test/i }).first()).toBeVisible();
  await expect(page.getByRole('button', { name: /^create$/i }).first()).toBeVisible();
});

test('REQ-6-1: a saved rule shows the branch verbatim with both summaries after reload', async ({ page }) => {
  await openBranches(page);
  await h.clickNamed(page, /add branch protection rule/i);
  await h.fillField(page, /branch name pattern/i, RULE_BRANCH);
  await page.getByRole('checkbox', { name: /require 1 approval/i }).first().click();
  await page.getByRole('checkbox', { name: /require status check test/i }).first().click();
  await page.getByRole('button', { name: /^create$/i }).first().click();

  await page.reload();
  await expect(page.getByText(RULE_BRANCH, { exact: true }).first()).toBeVisible();
  await expect(page.getByText(/1 approval/i).first()).toBeVisible();
  await expect(page.getByText(/require status check test/i).first()).toBeVisible();
});

test('REQ-6-1: a non-Admin has no Add branch protection rule button', async ({ browser }) => {
  const reviewer = await h.newSession(browser, { username: h.SEED.reviewer.username, password: h.SEED.reviewer.password });
  await reviewer.page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await reviewer.page.getByRole('link', { name: /^settings$/i }).first().click().catch(() => undefined);
  await reviewer.page.getByRole('link', { name: /^branches$/i }).first().click().catch(() => undefined);

  await h.expectAbsent(reviewer.page, /add branch protection rule/i);
});

test('REQ-6-1: the Checks area starts at test: pending and an Admin can save test: success with setter and time', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/pulls`);
  await page.getByRole('link', { name: new RegExp(h.SEED.openIssue, 'i') }).first().click();

  await expect(page.getByText(/test:\s*pending/i).first()).toBeVisible();
  const status = page.getByRole('combobox', { name: /test status/i }).first();
  await expect(status).toBeVisible();
  await h.setCombobox(page, 'test status', 'success');
  await page.getByRole('button', { name: /^save$/i }).first().click();

  await expect(page.getByText(/test:\s*success/i).first()).toBeVisible();
  await expect(page.getByText(new RegExp(h.SEED.owner.username, 'i')).first()).toBeVisible();

  await page.reload();
  await expect(page.getByText(/test:\s*success/i).first()).toBeVisible();
});
