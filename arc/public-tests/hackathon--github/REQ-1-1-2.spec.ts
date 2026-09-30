import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-1-1-2
// fixtures: sign_in
//
// Quoted: the fields "Username or email" and "Password" with the button "Sign in"
// on the account-access page, and the single generic failure "Invalid credentials"
// — the same wording for an unknown account and for a wrong password, so the page
// never discloses which one was wrong or whether the account exists.

async function attemptSignIn(page: any, identifier: string, password: string): Promise<void> {
  await h.openHome(page);
  await h.clickNamed(page, /^sign in$/i);
  await h.fillField(page, /username or email/i, identifier);
  await h.fillField(page, /^password$/i, password);
  await h.clickNamed(page, /^sign in$/i);
}

test('REQ-1-1-2: an unknown account and a wrong password both say only Invalid credentials', async ({ page }) => {
  await attemptSignIn(page, h.unique('pw-unknown-'), 'Valid-password-123!');
  await h.expectErrorText(page, 'Invalid credentials');
  await expect(await h.signedInUsername(page)).toHaveCount(0);

  await attemptSignIn(page, h.SEED.owner.username, 'Definitely-wrong-999!');
  await h.expectErrorText(page, 'Invalid credentials');
  await expect(await h.signedInUsername(page)).toHaveCount(0);
});

test('REQ-1-1-2: a signed-in session shows the account menu and survives refreshing the workspace', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);

  await expect(await h.signedInUsername(page)).toBeVisible();
  await page.reload();
  await expect(await h.signedInUsername(page)).toBeVisible();

  // Protected pages keep using this session after the reload.
  await h.openRepository(page, h.SEED.owner.username, h.SEED.repo);
  await h.expectHeading(page, new RegExp(`${h.SEED.owner.username}/${h.SEED.repo}`, 'i'));
});

test('REQ-1-1-2: both the username and the email identify the account', async ({ page }) => {
  await attemptSignIn(page, h.SEED.owner.email!, h.SEED.owner.password);
  await expect(await h.signedInUsername(page)).toBeVisible();
});
