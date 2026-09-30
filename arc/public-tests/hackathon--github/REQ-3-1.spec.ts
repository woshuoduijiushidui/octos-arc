import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-3-1
// fixtures: repository_search
//
// Quoted: the global search control has searchbox role and accessible name
// "Search"; pressing Enter shows repository results without an extra type-filter
// click. Each result is a link whose accessible name is exactly its repository
// name while the result also shows owner/name metadata. A query with no match
// displays "No results"; searching the private name exposes no result link.

test('REQ-3-1: the global search box is named Search and Enter shows repository results', async ({ page }) => {
  await h.openHome(page);
  const box = h.searchbox(page, 'Search');
  await expect(box).toBeVisible();
  await box.fill(h.SEED.repo);
  await box.press('Enter');

  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.repo}\\s*$`) }).first()).toBeVisible();
});

test('REQ-3-1: clicking a result opens the repository overview and reload keeps it', async ({ page }) => {
  await h.openHome(page);
  await h.searchRepository(page, h.SEED.repo);
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.repo}\\s*$`) }).first().click();

  await h.expectHeading(page, new RegExp(h.SEED.repo, 'i'));
  await page.reload();
  await h.expectHeading(page, new RegExp(h.SEED.repo, 'i'));
});

test('REQ-3-1: an unmatched query displays No results, including when repeated from the home page', async ({ page }) => {
  await h.openHome(page);
  await h.searchRepository(page, h.SEED.absentQuery);
  await h.expectErrorText(page, 'No results');

  await h.openHome(page);
  await h.searchRepository(page, h.SEED.absentQuery);
  await h.expectErrorText(page, 'No results');
});

test('REQ-3-1: searching the private repository name exposes no result link to a visitor', async ({ page }) => {
  await h.openHome(page);
  await h.searchRepository(page, h.SEED.privateRepo);

  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.privateRepo}\\s*$`) })).toHaveCount(0);
});
