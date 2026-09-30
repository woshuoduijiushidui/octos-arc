import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-6-2-2
// fixtures: compare_branches
//
// Quoted: "New pull request" is a link; the comparison page has native selects with
// combobox roles named base and compare whose options are the exact branch names; and
// "Compare changes" is a button. A valid selection shows the seeded changed file path
// `src/search.ts` verbatim, a "Commit summary" with the comparable commit count and an
// enabled "Create pull request" button. Selecting the same branch in both fields
// immediately shows "No changes" and disables creation without needing Compare
// changes to be clicked first.

const PULLS = `/${h.SEED.owner.username}/${h.SEED.repo}/pulls`;

test('REQ-6-2-2: New pull request is a link and the comparison page exposes base and compare comboboxes', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await page.goto(PULLS);
  await expect(page.getByRole('link', { name: /new pull request/i }).first()).toBeVisible();
  await page.getByRole('link', { name: /new pull request/i }).first().click();

  const base = page.getByRole('combobox', { name: /^base$/i }).first();
  const compare = page.getByRole('combobox', { name: /^compare$/i }).first();
  await expect(base).toBeVisible();
  await expect(compare).toBeVisible();
  expect(await base.evaluate((el) => el.tagName)).toBe('SELECT');
  expect(await compare.evaluate((el) => el.tagName)).toBe('SELECT');
  expect((await base.locator('option').allInnerTexts()).map((t) => t.trim())).toContain(h.SEED.branch);
});

test('REQ-6-2-2: distinct branches show the changed file, a commit summary and an enabled creation button', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await page.goto(PULLS);
  await page.getByRole('link', { name: /new pull request/i }).first().click();
  await h.setCombobox(page, 'base', h.SEED.branch);
  await h.setCombobox(page, 'compare', h.SEED.featureBranch);

  await expect(page.getByText(h.SEED.changedFile, { exact: true }).first()).toBeVisible();
  await expect(page.getByText(/commit summary/i).first()).toBeVisible();
  await expect(page.getByRole('button', { name: /create pull request/i }).first()).toBeEnabled();
});

test('REQ-6-2-2: the same branch on both sides shows No changes and disables creation without clicking Compare changes', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await page.goto(PULLS);
  await page.getByRole('link', { name: /new pull request/i }).first().click();
  await h.setCombobox(page, 'base', h.SEED.branch);
  await h.setCombobox(page, 'compare', h.SEED.branch);

  await h.expectErrorText(page, 'No changes');
  await expect(page.getByRole('button', { name: /create pull request/i }).first()).toBeDisabled();
});

test('REQ-6-2-2: comparison saves nothing — the pull request list is unchanged', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await page.goto(PULLS);
  const before = await page.getByRole('link', { name: /^\s*#\d+/ }).count();
  await page.getByRole('link', { name: /new pull request/i }).first().click();
  await h.setCombobox(page, 'base', h.SEED.branch);
  await h.setCombobox(page, 'compare', h.SEED.featureBranch);
  await page.getByRole('button', { name: /compare changes/i }).first().click();

  await page.goto(PULLS);
  expect(await page.getByRole('link', { name: /^\s*#\d+/ }).count()).toBe(before);
});
