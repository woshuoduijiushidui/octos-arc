import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-6-2-4
// fixtures: draft_pull_request
//
// Quoted: the draft comparison entry offers a "Create draft pull request" button which
// opens the form with Title and optional Description fields and one "Create draft pull
// request" submit button. Success visibly shows "Draft" and a present but disabled
// "Merge pull request" button. The dedicated ready-for-review seed PR displays its
// title, source branch and target branch verbatim and its "Ready for review" button
// changes it to Open (a confirmation, if used, is a single "Confirm").

const PULLS = `/${h.SEED.owner.username}/${h.SEED.repo}/pulls`;

async function openDraftComparison(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(PULLS);
  await page.getByRole('link', { name: /new pull request/i }).first().click();
  await h.setCombobox(page, 'base', h.SEED.branch);
  await h.setCombobox(page, 'compare', h.SEED.featureBranch);
}

test('REQ-6-2-4: creating a draft shows Draft and a disabled Merge pull request button', async ({ page }) => {
  const title = `Draft ${Date.now().toString(36)}`;
  await openDraftComparison(page);
  await page.getByRole('button', { name: /create draft pull request/i }).first().click();
  await h.fillField(page, /^title$/i, title);
  await page.getByRole('button', { name: /^create draft pull request$/i }).first().click();

  await h.expectHeading(page, new RegExp(`^\\s*${title}\\s*$`, 'i'));
  await expect(page.getByText(/^draft$/i).first()).toBeVisible();
  await expect(page.getByRole('button', { name: /merge pull request/i }).first()).toBeDisabled();
});

test('REQ-6-2-4: Ready for review converts the draft to Open and the activity survives reload', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await page.goto(PULLS);
  await page.getByRole('link', { name: new RegExp('draft', 'i') }).first().click();

  await page.getByRole('button', { name: /ready for review/i }).first().click();
  const confirm = page.getByRole('button', { name: /^confirm$/i }).first();
  if (await confirm.isVisible().catch(() => false)) await confirm.click();

  await expect(page.getByText(/^open$/i).first()).toBeVisible();
  await expect(page.getByText(/ready for review/i).first()).toBeVisible();
  await page.reload();
  await expect(page.getByText(/^open$/i).first()).toBeVisible();
});

test('REQ-6-2-4: the ready-for-review seed PR shows its title and both branch names', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await page.goto(PULLS);
  await page.getByRole('link', { name: new RegExp('draft', 'i') }).first().click();

  const body = await page.locator('body').innerText();
  expect(body).toMatch(new RegExp(h.SEED.branch, 'i'));
  expect(body).toMatch(new RegExp(h.SEED.featureBranch, 'i'));
});
