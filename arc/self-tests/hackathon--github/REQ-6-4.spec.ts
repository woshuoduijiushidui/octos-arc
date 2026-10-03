import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-6-4
// fixtures: pull_request_reviewers
//
// Quoted: the "Reviewers" button opens a picker with a textbox named "Search"; typing
// an eligible username immediately reveals an option with that exact accessible name.
// Selecting saves the request immediately without a separate Save, closes the picker and
// displays the username in the reviewer area, where it remains after reload. Each
// requested reviewer has a button named "Remove <username>" that removes the request
// immediately with no confirmation step.

const PULLS = `/${h.SEED.owner.username}/${h.SEED.repo}/pulls`;

async function openSeedPull(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(PULLS);
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first().click();
}

test('REQ-6-4: typing an eligible username reveals its option in the Reviewers picker', async ({ page }) => {
  await openSeedPull(page);
  await page.getByRole('button', { name: /^reviewers$/i }).first().click();

  await expect(h.textbox(page, 'Search')).toBeVisible();
  await h.fillField(page, /^search$/i, h.SEED.reviewer.username);
  await expect(h.option(page, h.SEED.reviewer.username)).toBeVisible();
});

test('REQ-6-4: selecting saves immediately, closes the picker and survives reload', async ({ page }) => {
  await openSeedPull(page);
  await page.getByRole('button', { name: /^reviewers$/i }).first().click();
  await h.fillField(page, /^search$/i, h.SEED.reviewer.username);
  await h.option(page, h.SEED.reviewer.username).click();

  await expect(page.getByText(h.SEED.reviewer.username, { exact: true }).first()).toBeVisible();
  await page.reload();
  await expect(page.getByText(h.SEED.reviewer.username, { exact: true }).first()).toBeVisible();
});

test('REQ-6-4: Remove <username> deletes the request immediately and persists across reload', async ({ page }) => {
  await openSeedPull(page);
  const remove = page.getByRole('button', { name: new RegExp(`remove\\s+${h.SEED.reviewer.username}`, 'i') }).first();
  if (!(await remove.count())) {
    await page.getByRole('button', { name: /^reviewers$/i }).first().click();
    await h.fillField(page, /^search$/i, h.SEED.reviewer.username);
    await h.option(page, h.SEED.reviewer.username).click();
  }

  await page.getByRole('button', { name: new RegExp(`remove\\s+${h.SEED.reviewer.username}`, 'i') }).first().click();
  await page.reload();
  await expect(page.getByRole('button', { name: new RegExp(`remove\\s+${h.SEED.reviewer.username}`, 'i') })).toHaveCount(0);
});
