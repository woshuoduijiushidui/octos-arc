import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-6-3-4
// fixtures: submit_review
//
// Quoted: from the Files changed link the reviewer activates the "Review changes" button
// to open one form with an optional "Summary" field, radio controls named "Comment",
// "Approve" and "Request changes", and a "Submit review" button. Approve without a
// summary is valid and displays "Approved"; Request changes with a summary displays
// "Changes requested" and that exact summary, and the decision remains after reload.

const PULLS = `/${h.SEED.owner.username}/${h.SEED.repo}/pulls`;

async function openReviewForm(page: any): Promise<void> {
  await h.signIn(page, h.SEED.reviewer);
  await page.goto(PULLS);
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first().click();
  await page.getByRole('link', { name: /files changed/i }).first().click();
  await page.getByRole('button', { name: /review changes/i }).first().click();
}

test('REQ-6-3-4: the review form exposes Summary, three decisions and Submit review', async ({ page }) => {
  await openReviewForm(page);

  await expect(page.getByLabel(/summary/i)).toBeVisible();
  await expect(page.getByRole('radio', { name: /^comment$/i }).first()).toBeVisible();
  await expect(page.getByRole('radio', { name: /^approve$/i }).first()).toBeVisible();
  await expect(page.getByRole('radio', { name: /^request changes$/i }).first()).toBeVisible();
  await expect(page.getByRole('button', { name: /submit review/i }).first()).toBeVisible();
});

test('REQ-6-3-4: Approve submits without a summary and displays Approved', async ({ page }) => {
  await openReviewForm(page);
  await page.getByRole('radio', { name: /^approve$/i }).first().click();
  await page.getByRole('button', { name: /submit review/i }).first().click();

  await expect(page.getByText(/approved/i).first()).toBeVisible();
});

test('REQ-6-3-4: Request changes with a summary displays Changes requested and the exact summary after reload', async ({ page }) => {
  const summary = `Needs work ${Date.now().toString(36)}`;
  await openReviewForm(page);
  await h.fillField(page, /summary/i, summary);
  await page.getByRole('radio', { name: /^request changes$/i }).first().click();
  await page.getByRole('button', { name: /submit review/i }).first().click();

  await expect(page.getByText(/changes requested/i).first()).toBeVisible();
  await expect(page.getByText(summary, { exact: false }).first()).toBeVisible();
  await page.reload();
  await expect(page.getByText(/changes requested/i).first()).toBeVisible();
});
