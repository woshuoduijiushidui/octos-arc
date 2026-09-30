import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-1-1
// fixtures: issue_list
//
// Quoted: "Open" and "Closed" are links, not buttons or tabs; a unique searchbox
// named "Search issues" filters as the user types with no Enter or submit; each
// result title is a link whose exact accessible name is the title. The seeded
// closed issue title is `Legacy welcome text`.

async function openIssues(page: any): Promise<void> {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.openIssues(page);
}

test('REQ-5-1-1: Open and Closed are links and the search box is a unique Search issues', async ({ page }) => {
  await openIssues(page);

  await expect(page.getByRole('link', { name: /^open$/i }).first()).toBeVisible();
  await expect(page.getByRole('link', { name: /^closed$/i }).first()).toBeVisible();
  await expect(h.searchbox(page, 'Search issues')).toHaveCount(1);
});

test('REQ-5-1-1: the search box filters as the user types the whole seeded title', async ({ page }) => {
  await openIssues(page);
  await h.searchbox(page, 'Search issues').fill(h.SEED.openIssue);

  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first()).toBeVisible();
  await page.reload();
  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first()).toBeVisible();
});

test('REQ-5-1-1: Closed plus the known closed title shows it and excludes the open issue', async ({ page }) => {
  await openIssues(page);
  await page.getByRole('link', { name: /^closed$/i }).first().click();

  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.closedIssue}\\s*$`) }).first()).toBeVisible();
  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) })).toHaveCount(0);
});

test('REQ-5-1-1: Open plus the known open title keeps the filter context after reload', async ({ page }) => {
  await openIssues(page);
  await page.getByRole('link', { name: /^open$/i }).first().click();
  await h.searchbox(page, 'Search issues').fill(h.SEED.openIssue);

  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first()).toBeVisible();
  await page.reload();
  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first()).toBeVisible();
});
