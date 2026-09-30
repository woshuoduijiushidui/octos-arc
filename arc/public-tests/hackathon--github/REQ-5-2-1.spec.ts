import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-2-1
// fixtures: create_issue
//
// Quoted: "New issue" is a link on the Issues page; the creation form has fields
// labeled "Title" and "Description" and a "Submit new issue" button. Success shows a
// heading named exactly after the entered title and the exact saved description, and
// the list carries a title link with the same exact name. A title of only three
// spaces is blank: it displays "Title is required" and creates no issue even when the
// optional description is empty.

async function openNewIssue(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.openIssues(page);
  await h.clickNamed(page, /new issue/i);
}

test('REQ-5-2-1: New issue is a link and the form exposes the labeled fields and submit button', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.openIssues(page);
  await expect(page.getByRole('link', { name: /new issue/i }).first()).toBeVisible();
  await page.getByRole('link', { name: /new issue/i }).first().click();

  await expect(h.textbox(page, 'Title')).toBeVisible();
  await expect(page.getByLabel(/^description$/i)).toBeVisible();
  await expect(page.getByRole('button', { name: /submit new issue/i })).toBeVisible();
});

test('REQ-5-2-1: a submitted issue opens with the exact title heading and description, and the list finds it', async ({ page }) => {
  const title = `Playwright issue ${Date.now().toString(36)}`;
  const body = 'Body written by the Playwright suite.';
  await openNewIssue(page);
  await h.fillField(page, /^title$/i, title);
  await h.fillField(page, /^description$/i, body);
  await h.clickNamed(page, /submit new issue/i);

  await h.expectHeading(page, new RegExp(`^\\s*${title}\\s*$`, 'i'));
  await expect(page.getByText(body, { exact: false }).first()).toBeVisible();

  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/issues`);
  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${title}\\s*$`) }).first()).toBeVisible();
});

test('REQ-5-2-1: a three-space title is treated as blank and creates no issue', async ({ page }) => {
  await openNewIssue(page);
  await h.fillField(page, /^title$/i, '   ');
  await h.clickNamed(page, /submit new issue/i);

  await h.expectErrorText(page, 'Title is required');
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/issues`);
  await expect(page.getByRole('link', { name: /^\s*$/ })).toHaveCount(0);
});
