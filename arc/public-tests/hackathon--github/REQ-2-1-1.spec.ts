import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-1-1
// fixtures: org_repositories
//
// Quoted: the navigation link "Repositories" (visually usable as a tab) and the
// textbox "Find a repository". Filtering updates as the user types with no separate
// submit. Each result is a link named exactly the repository name and opens a
// heading reading "organization name/repository name". A visitor filtering by the
// exact private repository name must never get its link.

test('REQ-2-1-1: the organization page offers Repositories and a live Find a repository box', async ({ page }) => {
  await h.openOrganization(page);
  await h.clickNamed(page, /^repositories$/i);

  await expect(page.getByRole('link', { name: /^repositories$/i }).first()).toBeVisible();
  await expect(h.textbox(page, 'Find a repository')).toBeVisible();
});

test('REQ-2-1-1: typing the public repository name filters the list and opens its overview', async ({ page }) => {
  await h.openOrganization(page);
  await h.clickNamed(page, /^repositories$/i);
  await h.fillField(page, /find a repository/i, h.SEED.repo);

  const link = page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.repo}\\s*$`) }).first();
  await expect(link).toBeVisible();
  await link.click();
  await h.expectHeading(page, new RegExp(`${h.SEED.owner.username}/${h.SEED.repo}`, 'i'));
});

test('REQ-2-1-1: filtering by the exact private repository name never exposes its link to a visitor', async ({ page }) => {
  await h.openOrganization(page);
  await h.clickNamed(page, /^repositories$/i);
  await h.fillField(page, /find a repository/i, h.SEED.privateRepo);

  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.privateRepo}\\s*$`) })).toHaveCount(0);
});

test('REQ-2-1-1: going Back keeps the public result available', async ({ page }) => {
  await h.openOrganization(page);
  await h.clickNamed(page, /^repositories$/i);
  const link = page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.repo}\\s*$`) }).first();
  await link.click();
  await page.goBack();

  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.repo}\\s*$`) }).first()).toBeVisible();
});
