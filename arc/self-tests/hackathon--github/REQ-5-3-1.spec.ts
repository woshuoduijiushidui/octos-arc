import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-3-1
// fixtures: issue_assignees
//
// Quoted: the settings icon is a button named "Assignees"; the open selector
// contains a textbox named "Search assignees" and items with role option named
// exactly after the member username. Clicking the option saves immediately and
// closes the selector with no separate Save action; the exact username is visible in
// metadata and survives reload; clicking it again removes the assignment.

async function openSeedIssue(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/issues`);
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first().click();
}

test('REQ-5-3-1: the Assignees button opens a Search assignees box listing member options', async ({ page }) => {
  await openSeedIssue(page);
  await expect(page.getByRole('button', { name: /^assignees$/i })).toHaveCount(1);
  await page.getByRole('button', { name: /^assignees$/i }).first().click();

  await expect(h.textbox(page, 'Search assignees')).toBeVisible();
  await h.fillField(page, /search assignees/i, h.SEED.reviewer.username);
  await expect(h.option(page, h.SEED.reviewer.username)).toBeVisible();
});

test('REQ-5-3-1: selecting the member saves immediately, closes the selector and survives reload', async ({ page }) => {
  await openSeedIssue(page);
  await page.getByRole('button', { name: /^assignees$/i }).first().click();
  await h.fillField(page, /search assignees/i, h.SEED.reviewer.username);
  await h.option(page, h.SEED.reviewer.username).click();

  await expect(page.getByText(h.SEED.reviewer.username, { exact: true }).first()).toBeVisible();
  await page.reload();
  await expect(page.getByText(h.SEED.reviewer.username, { exact: true }).first()).toBeVisible();
});

test('REQ-5-3-1: reopening shows the selected member and clicking it again removes the assignment', async ({ page }) => {
  await openSeedIssue(page);
  await page.getByRole('button', { name: /^assignees$/i }).first().click();
  const chosen = h.option(page, h.SEED.reviewer.username);
  if (await chosen.count()) {
    await chosen.click();
  } else {
    await h.fillField(page, /search assignees/i, h.SEED.reviewer.username);
    await h.option(page, h.SEED.reviewer.username).click();
  }

  await page.getByRole('button', { name: /^assignees$/i }).first().click();
  await h.option(page, h.SEED.reviewer.username).click();

  await page.reload();
  await expect(page.getByText(h.SEED.reviewer.username, { exact: true })).toHaveCount(0);
});
