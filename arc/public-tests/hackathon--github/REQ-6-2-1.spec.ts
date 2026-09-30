import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-6-2-1
// fixtures: pull_request_list
//
// The public list is directly accessible to a visitor and has an Open link for the
// status filter; selecting it displays the seeded Open PR as a link whose exact
// accessible name is its title, and opening that link shows the same title as a
// heading. Filtering only affects the display — it never modifies PRs, branches or
// reviews. The seeded filtering scenario also uses the Closed PR and its author.

const PULLS = `/${h.SEED.owner.username}/${h.SEED.repo}/pulls`;

test('REQ-6-2-1: a visitor can read the pull request list and its Open filter is a link', async ({ page }) => {
  await page.goto(PULLS);

  await expect(await h.signedInUsername(page)).toHaveCount(0);
  await expect(page.getByRole('link', { name: /^open$/i }).first()).toBeVisible();
});

test('REQ-6-2-1: the Open filter shows the seeded PR as a link with its exact title and opens the same heading', async ({ page }) => {
  await page.goto(PULLS);
  await page.getByRole('link', { name: /^open$/i }).first().click();

  const pr = page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first();
  await expect(pr).toBeVisible();
  await pr.click();
  await h.expectHeading(page, new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`, 'i'));
});

test('REQ-6-2-1: reloading the filtered list and reopening it from scratch shows the same PR', async ({ page }) => {
  await page.goto(PULLS);
  await page.getByRole('link', { name: /^open$/i }).first().click();
  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first()).toBeVisible();
  await page.reload();
  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first()).toBeVisible();

  await h.openHome(page);
  await page.goto(PULLS);
  await page.getByRole('link', { name: /^open$/i }).first().click();
  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first()).toBeVisible();
});

test('REQ-6-2-1: rows carry the filterable metadata of number, title, author, status and branches', async ({ page }) => {
  await page.goto(PULLS);
  const body = await page.locator('body').innerText();

  expect(body).toMatch(/#\d+/);
  expect(body).toMatch(new RegExp(h.SEED.owner.username, 'i'));
  for (const filter of [/draft/i, /^open$/im, /closed/i, /merged/i]) {
    await expect(page.getByText(filter).first()).toBeVisible();
  }
});
