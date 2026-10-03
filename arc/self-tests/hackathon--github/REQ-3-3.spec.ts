import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-3-3
// fixtures: public_repository_overview
//
// Quoted: the overview identifies the repository in a heading containing
// "owner/repository name", shows a visible "Public" marker, and provides a
// navigation link "Code" that is distinct from the clone-menu button. A seeded
// public repository is readable without sign-in and keeps the same heading after
// reload.

test('REQ-3-3: a visitor sees the owner/name heading and the Public marker without signing in', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);

  await h.expectHeading(page, new RegExp(`${h.SEED.owner.username}/${h.SEED.repo}`, 'i'));
  await h.expectPublicMarker(page);
  await expect(await h.signedInUsername(page)).toHaveCount(0);
});

test('REQ-3-3: the overview provides a Code navigation link distinct from the clone menu', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);

  await expect(page.getByRole('link', { name: /^code$/i }).first()).toBeVisible();
  await expect(page.getByRole('button', { name: /^code$/i }).first()).toBeVisible();
});

test('REQ-3-3: the heading survives reload at the same address', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await page.reload();

  await h.expectHeading(page, new RegExp(`${h.SEED.owner.username}/${h.SEED.repo}`, 'i'));
});
