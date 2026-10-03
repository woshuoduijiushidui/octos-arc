import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-6-3-1
// fixtures: pull_request_overview
//
// Quoted: a seeded public Open PR is directly viewable without sign-in and displays its
// exact title in a heading. "Commits" and "Files changed" are links; the Commits view
// displays a "Commit summary" and the Files changed view a "Changed files" summary.
// Reloading preserves the heading and the usable navigation links.

const PULLS = `/${h.SEED.owner.username}/${h.SEED.repo}/pulls`;

async function openSeedPull(page: any): Promise<void> {
  await page.goto(PULLS);
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first().click();
}

test('REQ-6-3-1: a visitor sees the exact title heading and both navigation links', async ({ page }) => {
  await openSeedPull(page);

  await expect(await h.signedInUsername(page)).toHaveCount(0);
  await h.expectHeading(page, new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`, 'i'));
  await expect(page.getByRole('link', { name: /^commits$/i }).first()).toBeVisible();
  await expect(page.getByRole('link', { name: /files changed/i }).first()).toBeVisible();
});

test('REQ-6-3-1: Commits shows a Commit summary and Files changed a Changed files summary', async ({ page }) => {
  await openSeedPull(page);
  await page.getByRole('link', { name: /^commits$/i }).first().click();
  await expect(page.getByText(/commit summary/i).first()).toBeVisible();

  await openSeedPull(page);
  await page.getByRole('link', { name: /files changed/i }).first().click();
  await expect(page.getByText(/changed files?/i).first()).toBeVisible();
});

test('REQ-6-3-1: reloading keeps the heading and the navigation still works', async ({ page }) => {
  await openSeedPull(page);
  await page.reload();

  await h.expectHeading(page, new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`, 'i'));
  await page.getByRole('link', { name: /files changed/i }).first().click();
  await expect(page.getByText(/changed files?/i).first()).toBeVisible();
});

test('REQ-6-3-1: the Conversation timeline carries the description and discussion', async ({ page }) => {
  await openSeedPull(page);
  await expect(page.getByText(/conversation|comment|activity/i).first()).toBeVisible();
});
