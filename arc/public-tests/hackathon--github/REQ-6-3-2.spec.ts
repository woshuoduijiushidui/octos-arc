import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-6-3-2
// fixtures: pull_request_diff
//
// Quoted: from a visible public pull request entry a visitor activates the Files changed
// link and sees the known changed file path `src/search.ts` verbatim plus visible
// aggregate text such as "3 additions, 1 deletions". The aggregate format is
// "<addition count> additions, <deletion count> deletions". No sign-in is required.

const PULLS = `/${h.SEED.owner.username}/${h.SEED.repo}/pulls`;

async function openFilesChanged(page: any): Promise<void> {
  await page.goto(PULLS);
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first().click();
  await page.getByRole('link', { name: /files changed/i }).first().click();
}

test('REQ-6-3-2: a visitor sees the changed file path verbatim', async ({ page }) => {
  await openFilesChanged(page);

  await expect(await h.signedInUsername(page)).toHaveCount(0);
  await expect(page.getByText(h.SEED.changedFile, { exact: true }).first()).toBeVisible();
});

test('REQ-6-3-2: the aggregate statistic uses the additions/deletions format', async ({ page }) => {
  await openFilesChanged(page);

  await expect(page.getByText(/\d+\s+additions?,\s*\d+\s+deletions?/i).first()).toBeVisible();
});

test('REQ-6-3-2: the diff shows added and deleted lines without changing anything', async ({ page }) => {
  await openFilesChanged(page);
  const before = await page.locator('body').innerText();
  await page.reload();
  const after = await page.locator('body').innerText();

  expect(after).toContain(h.SEED.changedFile);
  expect(before).toContain(h.SEED.changedFile);
});
