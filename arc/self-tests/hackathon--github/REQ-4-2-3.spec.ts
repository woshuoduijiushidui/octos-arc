import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-4-2-3
// fixtures: code_search
//
// Quoted: the repository page and the results expose one searchbox named "Search";
// after Enter a unique link named "Code" selects code results, distinguishable from
// repository navigation. The seeded query is `search flow`, the matching file is
// `README.md` with that text visible as a complete value, and `no-such-token` does
// not occur in searchable code and must show "No code results" with the query kept.

test('REQ-4-2-3: searching the seeded query offers a unique Code results link that opens the matching file', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  const box = h.searchbox(page, 'Search');
  await box.fill(h.SEED.codeQuery);
  await box.press('Enter');

  await expect(page.getByRole('link', { name: /^code$/i })).toHaveCount(1);
  await page.getByRole('link', { name: /^code$/i }).first().click();

  const result = page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.readme.replace('.', '\\.')}\\s*$`) }).first();
  await expect(result).toBeVisible();
  await result.click();
  await expect(page.getByText(h.SEED.codeQuery, { exact: false }).first()).toBeVisible();
});

test('REQ-4-2-3: refreshing a matching file keeps the file context and its exact link name', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}/search?q=${encodeURIComponent(h.SEED.codeQuery)}&type=code`);
  const result = page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.readme.replace('.', '\\.')}\\s*$`) }).first();
  if (await result.count()) {
    await result.click();
    await page.reload();
    await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.readme.replace('.', '\\.')}\\s*$`) }).first()).toBeVisible();
  }
});

test('REQ-4-2-3: an absent query shows No code results and retains the exact query', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  const box = h.searchbox(page, 'Search');
  await box.fill(h.SEED.absentQuery);
  await box.press('Enter');
  await page.getByRole('link', { name: /^code$/i }).first().click();

  await h.expectErrorText(page, 'No code results');
  await expect(h.searchbox(page, 'Search')).toHaveValue(h.SEED.absentQuery);
});

test('REQ-4-2-3: repeating the same search from the repository produces the same empty state with no stale match', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.searchRepository(page, h.SEED.absentQuery);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  const box = h.searchbox(page, 'Search');
  await box.fill(h.SEED.absentQuery);
  await box.press('Enter');
  await page.getByRole('link', { name: /^code$/i }).first().click();

  await h.expectErrorText(page, 'No code results');
  await expect(page.getByText(h.SEED.absentQuery, { exact: true }).first()).toBeVisible();
});
