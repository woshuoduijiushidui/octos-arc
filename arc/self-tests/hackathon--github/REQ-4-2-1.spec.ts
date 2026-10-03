import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-4-2-1
// fixtures: commit_history
//
// Quoted: the repository and file pages each expose one history link named
// "Commits" (a commit count may accompany the label without creating a second
// matching link). The seeded commit message `Document search flow` and its author
// are displayed in full as distinct readable text values, with a relative
// timestamp containing "ago".

test('REQ-4-2-1: the repository page exposes exactly one Commits history link', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);

  await expect(page.getByRole('link', { name: /^commits\b/i })).toHaveCount(1);
});

test('REQ-4-2-1: opening the branch history directly shows the seeded message, author and relative time', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await page.getByRole('link', { name: /^commits\b/i }).first().click();

  await expect(page.getByText(h.SEED.commitMessage, { exact: true }).first()).toBeVisible();
  await expect(page.getByText(new RegExp(h.SEED.owner.username, 'i')).first()).toBeVisible();
  await expect(page.getByText(/\bago\b/i).first()).toBeVisible();
});

test('REQ-4-2-1: the file page exposes its own single Commits link', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.repoNav(page, 'Code').click();
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.readme.replace('.', '\\.')}\\s*$`) }).first().click();

  await expect(page.getByRole('link', { name: /^commits\b/i })).toHaveCount(1);
});
