import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-6-6
// fixtures: pull_request_status
//
// Quoted: the PR page provides "Close pull request" and "Reopen pull request" as buttons
// for authorized users. Closing immediately sets Closed without an extra confirmation
// dialog, reopening immediately restores Open, and after reload Close pull request is
// available again. A viewer who is neither the author nor Maintain/Admin/Owner has both
// controls absent — not disabled, and not rejected after activation.

const PULLS = `/${h.SEED.owner.username}/${h.SEED.repo}/pulls`;

async function openPullByTitle(page: any, title: string): Promise<void> {
  await page.goto(PULLS);
  await page.getByRole('link', { name: new RegExp(title, 'i') }).first().click();
}

test('REQ-6-6: closing and reopening need no confirmation dialog and reload restores Close', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openPullByTitle(page, h.SEED.openIssue);

  await page.getByRole('button', { name: /close pull request/i }).first().click();
  await expect(page.getByText(/^closed$/i).first()).toBeVisible();
  await expect(page.getByRole('dialog')).toHaveCount(0);

  await page.getByRole('button', { name: /reopen pull request/i }).first().click();
  await expect(page.getByText(/^open$/i).first()).toBeVisible();
  await page.reload();
  await expect(page.getByRole('button', { name: /close pull request/i }).first()).toBeVisible();
});

test('REQ-6-6: closing keeps the discussion, reviews and diff viewable', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openPullByTitle(page, h.SEED.openIssue);
  await page.getByRole('button', { name: /close pull request/i }).first().click();

  await expect(page.getByRole('link', { name: /files changed/i }).first()).toBeVisible();
  await expect(page.getByRole('link', { name: /^commits$/i }).first()).toBeVisible();
});

test('REQ-6-6: a viewer who is neither author nor maintainer has no close or reopen control', async ({ browser }) => {
  const member = await h.registerAccount(await browser.newPage());
  const owner = await h.newSession(browser, h.SEED.owner);
  await owner.page.goto('/');
  await h.openOrganization(owner.page);
  await h.clickNamed(owner.page, /^people$/i);
  await h.clickNamed(owner.page, /^add member$/i);
  await h.fillField(owner.page, /username or email/i, member.username);
  await h.setCombobox(owner.page, 'Role', 'Member');
  await h.clickNamed(owner.page, /^add member$/i);

  const viewer = await h.newSession(browser, member);
  await viewer.page.goto(PULLS);
  await viewer.page.getByRole('link', { name: new RegExp(h.SEED.openIssue, 'i') }).first().click();

  await h.expectAbsent(viewer.page, /^close pull request$/i);
  await h.expectAbsent(viewer.page, /^reopen pull request$/i);
});
