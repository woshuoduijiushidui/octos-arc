import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-4-2-2
// fixtures: commit_diff
//
// Quoted: the seeded commit has an accessible commit entry, a known changed-file
// path `src/search.ts`, and a readable parent revision. Opening that commit entry
// as a visitor displays the changed-file path as an exact text value, the "Changed
// files" summary, and the numeric additions/deletions summary — without requiring
// previous navigation through history, and without creating a review, comment or
// commit.

test('REQ-4-2-2: a visitor can open the commit entry directly and see the changed file path', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/commits`);
  const entry = page.getByRole('link', { name: new RegExp(h.SEED.commitMessage, 'i') }).first();
  await expect(entry).toBeVisible();
  await entry.click();

  await expect(page.getByText(h.SEED.changedFile, { exact: true }).first()).toBeVisible();
});

test('REQ-4-2-2: the diff page shows the Changed files summary and a numeric additions/deletions summary', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/commits`);
  await page.getByRole('link', { name: new RegExp(h.SEED.commitMessage, 'i') }).first().click();

  await expect(page.getByText(/changed files?/i).first()).toBeVisible();
  await expect(page.getByText(/\b\d+\s+additions?\b/i).first()).toBeVisible();
  await expect(page.getByText(/\b\d+\s+deletions?\b/i).first()).toBeVisible();
});

test('REQ-4-2-2: reading the diff as a visitor does not require sign-in', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/commits`);
  await page.getByRole('link', { name: new RegExp(h.SEED.commitMessage, 'i') }).first().click();

  await expect(await h.signedInUsername(page)).toHaveCount(0);
  await expect(page.getByText(h.SEED.changedFile, { exact: true }).first()).toBeVisible();
});
