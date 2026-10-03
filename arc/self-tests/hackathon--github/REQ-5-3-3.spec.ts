import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-3-3
// fixtures: issue_milestone
//
// Quoted: the settings icon in the "Milestone" area is a button named "Milestone";
// selectable items have role option with the exact milestone name as their accessible
// name, and one item is "None" to remove the association. Clicking saves immediately
// and closes the picker with no separate save or confirmation; the exact milestone
// name remains visible after reload. Read and Write users may only view.

async function openSeedIssue(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/issues`);
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first().click();
}

test('REQ-5-3-3: the Milestone button opens options named after the repository milestone plus None', async ({ page }) => {
  await openSeedIssue(page);
  await page.getByRole('button', { name: /^milestone$/i }).first().click();

  await expect(h.option(page, h.SEED.milestone)).toBeVisible();
  await expect(h.option(page, 'None')).toBeVisible();
});

test('REQ-5-3-3: choosing the milestone saves immediately and survives reload', async ({ page }) => {
  await openSeedIssue(page);
  await page.getByRole('button', { name: /^milestone$/i }).first().click();
  await h.option(page, h.SEED.milestone).click();

  await expect(page.getByText(h.SEED.milestone, { exact: true }).first()).toBeVisible();
  await page.reload();
  await expect(page.getByText(h.SEED.milestone, { exact: true }).first()).toBeVisible();
});

test('REQ-5-3-3: selecting None removes the association', async ({ page }) => {
  await openSeedIssue(page);
  await page.getByRole('button', { name: /^milestone$/i }).first().click();
  await h.option(page, 'None').click();

  await page.reload();
  await expect(page.getByRole('button', { name: /^milestone$/i }).first()).toBeVisible();
});
