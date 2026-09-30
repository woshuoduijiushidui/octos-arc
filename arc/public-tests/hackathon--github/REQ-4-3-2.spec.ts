import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-4-3-2
// fixtures: create_branch
//
// Quoted: as the user types, a valid unused name shows the option "Create branch:
// <name>" with no Enter or separate search action, and an invalid name immediately
// shows "Invalid branch". `invalid..branch` is the fixed invalid example and
// `pw-branch-<unique suffix>` the generated valid form. Creating switches the
// selector to the new branch and reload keeps it selected.

async function openBranchSelector(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.repoNav(page, 'Code').click();
  await page.getByRole('button', { name: new RegExp(`^\\s*branch\\s+${h.SEED.branch}\\s*$`, 'i') }).first().click();
}

test('REQ-4-3-2: typing a valid unused name reveals Create branch:<name> without pressing Enter', async ({ page }) => {
  const name = h.unique('pw-branch-');
  await openBranchSelector(page);
  await h.fillField(page, /find branch/i, name);

  await expect(page.getByRole('option', { name: new RegExp(`create branch:\\s*${name}`, 'i') }).first()).toBeVisible();
});

test('REQ-4-3-2: an invalid name immediately shows Invalid branch and cannot create a reference', async ({ page }) => {
  await openBranchSelector(page);
  await h.fillField(page, /find branch/i, 'invalid..branch');

  await h.expectErrorText(page, 'Invalid branch');
  await expect(page.getByRole('option', { name: /create branch:/i })).toHaveCount(0);
});

test('REQ-4-3-2: creating the branch switches the selector to it and reload keeps it selected', async ({ page }) => {
  const name = h.unique('pw-branch-');
  await openBranchSelector(page);
  await h.fillField(page, /find branch/i, name);
  await page.getByRole('option', { name: new RegExp(`create branch:\\s*${name}`, 'i') }).first().click();

  await expect(page.getByRole('button', { name: new RegExp(`^\\s*branch\\s+${name}\\s*$`, 'i') }).first()).toBeVisible();
  await page.reload();
  await expect(page.getByRole('button', { name: new RegExp(`^\\s*branch\\s+${name}\\s*$`, 'i') }).first()).toBeVisible();
});
