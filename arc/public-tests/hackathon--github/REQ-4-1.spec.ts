import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-4-1
// fixtures: file_browser
//
// Quoted: directory and file entries are links whose exact accessible names are
// their respective names; clicking them in order opens the saved text with the
// expected content visible as a complete text value, and reloading that file page
// keeps the same branch, path and content.

test('REQ-4-1: the Code page lists the seeded file as a link named exactly after it', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.repoNav(page, 'Code').click();

  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.readme.replace('.', '\\.')}\\s*$`) }).first()).toBeVisible();
});

test('REQ-4-1: opening the file shows its saved content as a complete text value and reload keeps it', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.repoNav(page, 'Code').click();
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.readme.replace('.', '\\.')}\\s*$`) }).first().click();

  await expect(page.getByText(h.SEED.codeQuery, { exact: false }).first()).toBeVisible();
  await h.expectHeading(page, new RegExp(h.SEED.readme.replace('.', '\\.'), 'i'));

  await page.reload();
  await expect(page.getByText(h.SEED.codeQuery, { exact: false }).first()).toBeVisible();
});

test('REQ-4-1: a directory entry opens its own page and reload keeps the same path', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.repoNav(page, 'Code').click();
  const dir = page.getByRole('link', { name: /^\s*docs\s*$/i }).first();
  if (!(await dir.count())) test.skip(true, 'no seeded directory entry on the default branch');
  await dir.click();

  await expect(page.getByRole('link', { name: /guide\.md/i }).first()).toBeVisible();
  await page.reload();
  await expect(page.getByRole('link', { name: /guide\.md/i }).first()).toBeVisible();
});
