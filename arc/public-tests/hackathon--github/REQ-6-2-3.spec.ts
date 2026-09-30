import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-6-2-3
// fixtures: create_pull_request
//
// Quoted: the Create pull request button on a valid comparison page opens a form with
// a field labeled Title, an optional Description field and a single Create pull request
// submit button (the comparison-page action is no longer a competing active button).
// Success shows the entered title as the exact PR heading and visible Open status, and
// the title survives reload. A spaces-only title is rejected with "Title is required"
// and creates no PR.

const PULLS = `/${h.SEED.owner.username}/${h.SEED.repo}/pulls`;

async function openValidComparison(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(PULLS);
  await page.getByRole('link', { name: /new pull request/i }).first().click();
  await h.setCombobox(page, 'base', h.SEED.branch);
  await h.setCombobox(page, 'compare', h.SEED.featureBranch);
}

test('REQ-6-2-3: the comparison action opens a form with Title, Description and one submit button', async ({ page }) => {
  await openValidComparison(page);
  await page.getByRole('button', { name: /^create pull request$/i }).first().click();

  await expect(h.textbox(page, 'Title')).toBeVisible();
  await expect(page.getByLabel(/description/i)).toBeVisible();
  await expect(page.getByRole('button', { name: /^create pull request$/i })).toHaveCount(1);
});

test('REQ-6-2-3: a submitted PR shows its exact title heading and Open status and survives reload', async ({ page }) => {
  const title = `Playwright PR ${Date.now().toString(36)}`;
  await openValidComparison(page);
  await page.getByRole('button', { name: /^create pull request$/i }).first().click();
  await h.fillField(page, /^title$/i, title);
  await page.getByRole('button', { name: /^create pull request$/i }).first().click();

  await h.expectHeading(page, new RegExp(`^\\s*${title}\\s*$`, 'i'));
  await expect(page.getByText(/^open$/i).first()).toBeVisible();
  await page.reload();
  await h.expectHeading(page, new RegExp(`^\\s*${title}\\s*$`, 'i'));
});

test('REQ-6-2-3: a spaces-only title is rejected and creates no pull request', async ({ page }) => {
  const before = await (async () => {
    await page.goto(PULLS);
    return page.getByRole('link', { name: /^\s*#\d+/ }).count();
  })();

  await openValidComparison(page);
  await page.getByRole('button', { name: /^create pull request$/i }).first().click();
  await h.fillField(page, /^title$/i, '   ');
  await page.getByRole('button', { name: /^create pull request$/i }).first().click();

  await h.expectErrorText(page, 'Title is required');
  await page.goto(PULLS);
  expect(await page.getByRole('link', { name: /^\s*#\d+/ }).count()).toBe(before);
});
