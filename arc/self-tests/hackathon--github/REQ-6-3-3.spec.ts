import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-6-3-3
// fixtures: review_comments
//
// Quoted: a non-author reviewer hovers a changed line, clicks the button shown as "+"
// and accessibly named "Add comment", and chooses "Add single comment" to publish
// immediately or "Start a review" to keep a pending draft. The changed-line Add comment
// buttons are directly activatable without a prerequisite hover and the first one in
// document order targets the first commentable line; activating one opens a single
// editor labeled "Comment". "Add single comment" displays the exact body in that diff
// view and retains it after reload; "Start a review" shows the body and "Pending review"
// to its author.

const PULLS = `/${h.SEED.owner.username}/${h.SEED.repo}/pulls`;

async function openFilesChanged(page: any, who = h.SEED.reviewer): Promise<void> {
  await h.signIn(page, who);
  await page.goto(PULLS);
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first().click();
  await page.getByRole('link', { name: /files changed/i }).first().click();
}

test('REQ-6-3-3: the changed-line Add comment controls are directly activatable and open one Comment editor', async ({ page }) => {
  await openFilesChanged(page);
  const add = page.getByRole('button', { name: /add comment/i }).first();
  await expect(add).toBeVisible();
  await add.click();

  await expect(page.getByLabel(/^comment$/i).first()).toBeVisible();
  await expect(page.getByRole('button', { name: /add single comment/i }).first()).toBeVisible();
  await expect(page.getByRole('button', { name: /start a review/i }).first()).toBeVisible();
});

test('REQ-6-3-3: Add single comment publishes the exact body in the diff view and keeps it after reload', async ({ page }) => {
  const body = `Inline comment ${Date.now().toString(36)}`;
  await openFilesChanged(page);
  await page.getByRole('button', { name: /add comment/i }).first().click();
  await h.fillField(page, /^comment$/i, body);
  await page.getByRole('button', { name: /add single comment/i }).first().click();

  await expect(page.getByText(body, { exact: false }).first()).toBeVisible();
  await page.reload();
  await expect(page.getByText(body, { exact: false }).first()).toBeVisible();
});

test('REQ-6-3-3: Start a review keeps a pending draft that its author can see after reload', async ({ page }) => {
  const body = `Pending draft ${Date.now().toString(36)}`;
  await openFilesChanged(page);
  await page.getByRole('button', { name: /add comment/i }).first().click();
  await h.fillField(page, /^comment$/i, body);
  await page.getByRole('button', { name: /start a review/i }).first().click();

  await expect(page.getByText(body, { exact: false }).first()).toBeVisible();
  await expect(page.getByText(/pending review/i).first()).toBeVisible();
  await page.reload();
  await expect(page.getByText(body, { exact: false }).first()).toBeVisible();
});
