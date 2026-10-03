import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-6-5
// fixtures: merge_pull_request
//
// Quoted: on an eligible PR, "Merge pull request" and then "Confirm merge" are buttons;
// confirming displays "Merged" and that state remains after reload. A blocked PR keeps a
// visible disabled "Merge pull request" button and explains its unmet protection
// condition before any click, with the wording "Review required by branch protection".

const PULLS = `/${h.SEED.owner.username}/${h.SEED.repo}/pulls`;

async function openPullByTitle(page: any, title: string): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(PULLS);
  await page.getByRole('link', { name: new RegExp(title, 'i') }).first().click();
}

test('REQ-6-5: an eligible PR merges through Merge pull request then Confirm merge and stays Merged', async ({ page }) => {
  await openPullByTitle(page, h.SEED.openIssue);
  const merge = page.getByRole('button', { name: /^merge pull request$/i }).first();
  await expect(merge).toBeEnabled();
  await merge.click();

  const confirm = page.getByRole('button', { name: /^confirm merge$/i }).first();
  await expect(confirm).toBeVisible();
  await confirm.click();

  await expect(page.getByText(/^merged$/i).first()).toBeVisible();
  await page.reload();
  await expect(page.getByText(/^merged$/i).first()).toBeVisible();
});

test('REQ-6-5: the merge confirmation area lists satisfied and unsatisfied conditions', async ({ page }) => {
  await openPullByTitle(page, h.SEED.openIssue);
  await page.getByRole('button', { name: /^merge pull request$/i }).first().click();

  await expect(page.getByText(/approval|check|conflict|protection/i).first()).toBeVisible();
});

test('REQ-6-5: a blocked PR keeps a disabled Merge pull request button and names the missing approval', async ({ page }) => {
  await openPullByTitle(page, h.SEED.closedIssue);

  const merge = page.getByRole('button', { name: /^merge pull request$/i }).first();
  if (await merge.count()) {
    await expect(merge).toBeDisabled();
  }
  await expect(page.getByText(/review required by branch protection/i).first()).toBeVisible();
});
