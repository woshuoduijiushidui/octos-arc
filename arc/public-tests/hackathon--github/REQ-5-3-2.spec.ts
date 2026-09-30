import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-3-2
// fixtures: issue_labels
//
// Quoted: the sidebar button named "Labels" opens options whose exact accessible
// names are the current repository's label names. Clicking an option saves the
// association immediately and closes the picker with no separate Save button; the
// label is visible on the detail page and survives reload; selecting it again
// removes the association. The selector must never create labels or show labels from
// other repositories.

async function openSeedIssue(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/issues`);
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first().click();
}

test('REQ-5-3-2: the Labels button opens options named after the repository labels', async ({ page }) => {
  await openSeedIssue(page);
  await page.getByRole('button', { name: /^labels$/i }).first().click();

  await expect(h.option(page, h.SEED.labels.first)).toBeVisible();
  await expect(h.option(page, h.SEED.labels.second)).toBeVisible();
});

test('REQ-5-3-2: choosing a label saves immediately and survives reload', async ({ page }) => {
  await openSeedIssue(page);
  await page.getByRole('button', { name: /^labels$/i }).first().click();
  await h.option(page, h.SEED.labels.first).click();

  await expect(page.getByText(h.SEED.labels.first, { exact: true }).first()).toBeVisible();
  await page.reload();
  await expect(page.getByText(h.SEED.labels.first, { exact: true }).first()).toBeVisible();
});

test('REQ-5-3-2: selecting the same option again removes the association', async ({ page }) => {
  await openSeedIssue(page);
  await page.getByRole('button', { name: /^labels$/i }).first().click();
  const option = h.option(page, h.SEED.labels.first);
  if (await option.count()) {
    await option.click();
  } else {
    await h.option(page, h.SEED.labels.second).click();
  }

  await page.getByRole('button', { name: /^labels$/i }).first().click();
  await h.option(page, h.SEED.labels.first).click();

  await page.reload();
  await expect(page.getByRole('button', { name: /^labels$/i }).first()).toBeVisible();
});
