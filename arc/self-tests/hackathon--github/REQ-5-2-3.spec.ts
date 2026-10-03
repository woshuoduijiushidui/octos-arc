import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-2-3
// fixtures: issue_comment
//
// Quoted: the comment editor is labeled "Comment" and its submit button is named
// exactly "Comment". After submission the complete comment text and the author
// username are visible and the comment remains after reload. Whitespace-only input
// may either disable the button or report "Comment is required" — neither path adds
// a comment or an activity entry.

async function openSeedIssue(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/issues`);
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first().click();
}

test('REQ-5-2-3: the comment editor and its submit button are both named Comment', async ({ page }) => {
  await openSeedIssue(page);
  await expect(page.getByLabel(/^comment$/i).first()).toBeVisible();
  await expect(page.getByRole('button', { name: /^comment$/i }).first()).toBeVisible();
});

test('REQ-5-2-3: a submitted comment shows its text and author and stays after reload', async ({ page }) => {
  const body = `Comment from the suite ${Date.now().toString(36)}`;
  await openSeedIssue(page);
  await h.fillField(page, /^comment$/i, body);
  await page.getByRole('button', { name: /^comment$/i }).first().click();

  await expect(page.getByText(body, { exact: false }).first()).toBeVisible();
  await expect(page.getByText(h.SEED.owner.username, { exact: false }).first()).toBeVisible();

  await page.reload();
  await expect(page.getByText(body, { exact: false }).first()).toBeVisible();
});

test('REQ-5-2-3: whitespace-only input adds no comment and no activity article', async ({ page }) => {
  await openSeedIssue(page);
  const articlesBefore = await page.getByRole('article').count();
  await h.fillField(page, /^comment$/i, '   ');

  const button = page.getByRole('button', { name: /^comment$/i }).first();
  if (await button.isEnabled()) {
    await button.click();
    await h.expectErrorText(page, 'Comment is required');
  } else {
    await expect(button).toBeDisabled();
  }

  await expect(page.getByRole('article')).toHaveCount(articlesBefore);
});
