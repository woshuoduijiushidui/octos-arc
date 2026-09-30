import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-4
// fixtures: issue_status
//
// Quoted: the detail page offers "Close issue" when Open and "Reopen issue" when
// Closed; activating either saves immediately with no extra confirmation. Closing
// shows Closed status and a "Closed issue" activity; reopening shows Open status and
// after reload the "Close issue" button is available again. The Read viewer sees
// neither control — hidden, not merely disabled.

async function openSeedIssue(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/issues`);
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first().click();
}

test('REQ-5-4: closing saves immediately, shows Closed and records a Closed issue activity', async ({ page }) => {
  await openSeedIssue(page);
  await page.getByRole('button', { name: /close issue/i }).first().click();

  await expect(page.getByText(/^closed$/i).first()).toBeVisible();
  await expect(page.getByText(/closed issue/i).first()).toBeVisible();
  await page.reload();
  await expect(page.getByRole('button', { name: /reopen issue/i }).first()).toBeVisible();
});

test('REQ-5-4: reopening restores Open and makes Close issue available again after reload', async ({ page }) => {
  await openSeedIssue(page);
  await page.getByRole('button', { name: /close issue/i }).first().click();
  await page.getByRole('button', { name: /reopen issue/i }).first().click();

  await expect(page.getByText(/^open$/i).first()).toBeVisible();
  await page.reload();
  await expect(page.getByRole('button', { name: /close issue/i }).first()).toBeVisible();
});

test('REQ-5-4: the status transition does not modify title, description or metadata', async ({ page }) => {
  await openSeedIssue(page);
  const before = await page.locator('body').innerText();
  await page.getByRole('button', { name: /close issue/i }).first().click();
  await page.getByRole('button', { name: /reopen issue/i }).first().click();

  await h.expectHeading(page, new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`, 'i'));
  await expect(page.getByText(h.SEED.openIssueBody, { exact: false }).first()).toBeVisible();
  expect(before).toContain(h.SEED.openIssueBody);
});

test('REQ-5-4: a reader without Triage shows neither close nor reopen', async ({ browser }) => {
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
  await viewer.page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/issues`);
  await viewer.page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first().click();

  await h.expectAbsent(viewer.page, /^close issue$/i);
  await h.expectAbsent(viewer.page, /^reopen issue$/i);
});
