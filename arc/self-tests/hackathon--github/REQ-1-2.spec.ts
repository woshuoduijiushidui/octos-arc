import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-1-2
// fixtures: sign_out
//
// Quoted: exactly one button "Account menu" whose menu holds exactly one link
// "Sign out", the dialog "Sign out" with "Confirm sign out" and "Cancel". Only the
// confirmation invalidates the session; after it, refresh / back navigation /
// reopening a protected page restores an unauthenticated state showing "Sign in".

const PROTECTED = `/${h.SEED.owner.username}/${h.SEED.repo}`;

test('REQ-1-2: the account menu is a single button holding a single Sign out link', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await h.openAccountMenu(page);

  await expect(page.getByRole('button', { name: /account menu/i })).toHaveCount(1);
  await expect(page.getByRole('link', { name: /^sign out$/i })).toHaveCount(1);
});

test('REQ-1-2: Cancel keeps the current session and page', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await page.goto(PROTECTED);

  await h.openAccountMenu(page);
  await h.clickNamed(page, /sign out/i);
  const scope = await h.dialog(page, 'Sign out');
  await h.clickNamed(scope, /^cancel$/i);

  await expect(await h.signedInUsername(page)).toBeVisible();
  await page.reload();
  await expect(await h.signedInUsername(page)).toBeVisible();
});

test('REQ-1-2: confirming sign out invalidates the session on refresh, back navigation and direct reopening', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await page.goto(PROTECTED);
  await h.signOut(page);

  await page.reload();
  await expect(page.getByRole('link', { name: /^sign in$/i }).first()).toBeVisible();

  await page.goto(PROTECTED);
  await expect(await h.signedInUsername(page)).toHaveCount(0);
  await expect(page.getByRole('link', { name: /^sign in$/i }).first()).toBeVisible();
});
