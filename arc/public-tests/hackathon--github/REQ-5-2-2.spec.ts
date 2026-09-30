import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-2-2
// fixtures: edit_issue
//
// Quoted: the unique buttons "Edit issue title" and "Edit issue description"; the
// former opens a form with a textbox labeled "Issue title" and a button "Save issue
// title", the latter a textbox labeled "Issue description" and a button "Save issue
// description". The invalid-edit seed is a separate issue with the original title
// `Original issue title`: replacing it with three spaces shows "Title is required"
// and reload restores the original heading.

async function openIssueByTitle(page: any, title: string): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/issues`);
  await page.getByRole('link', { name: new RegExp(`^\\s*${title}\\s*$`) }).first().click();
}

test('REQ-5-2-2: both edit controls and their separate forms are available to a writer', async ({ page }) => {
  await openIssueByTitle(page, h.SEED.openIssue);
  await expect(page.getByRole('button', { name: /^edit issue title$/i })).toHaveCount(1);
  await expect(page.getByRole('button', { name: /^edit issue description$/i })).toHaveCount(1);

  await page.getByRole('button', { name: /^edit issue title$/i }).first().click();
  await expect(h.textbox(page, 'Issue title')).toBeVisible();
  await expect(page.getByRole('button', { name: /save issue title/i })).toBeVisible();
});

test('REQ-5-2-2: saving a new title and description shows both exactly after reload', async ({ page }) => {
  const title = `Retitled ${Date.now().toString(36)}`;
  const body = 'Rewritten description from the Playwright suite.';
  await openIssueByTitle(page, h.SEED.openIssue);

  await page.getByRole('button', { name: /^edit issue title$/i }).first().click();
  await h.fillField(page, /issue title/i, title);
  await page.getByRole('button', { name: /save issue title/i }).first().click();

  await page.getByRole('button', { name: /^edit issue description$/i }).first().click();
  await h.fillField(page, /issue description/i, body);
  await page.getByRole('button', { name: /save issue description/i }).first().click();

  await page.reload();
  await h.expectHeading(page, new RegExp(`^\\s*${title}\\s*$`, 'i'));
  await expect(page.getByText(body, { exact: false }).first()).toBeVisible();
});

test('REQ-5-2-2: a three-space title is refused and reload restores the original title', async ({ page }) => {
  await openIssueByTitle(page, h.SEED.invalidIssue);

  await page.getByRole('button', { name: /^edit issue title$/i }).first().click();
  await h.fillField(page, /issue title/i, '   ');
  await page.getByRole('button', { name: /save issue title/i }).first().click();

  await h.expectErrorText(page, 'Title is required');
  await page.reload();
  await h.expectHeading(page, new RegExp(`^\\s*${h.SEED.invalidIssue}\\s*$`, 'i'));
});
