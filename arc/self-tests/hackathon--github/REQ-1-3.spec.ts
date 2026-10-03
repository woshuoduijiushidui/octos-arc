import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-1-3
// fixtures: password_settings
//
// Quoted: the page "Password and authentication" under account Settings; the
// uniquely labeled inputs "Current password", "New password", "Confirm password";
// the button "Update password"; and the messages "Current password is required",
// "Current password is incorrect", "Password confirmation does not match",
// "Password updated". The seed values are `New-password-456!` for the successful
// update and `Required-password-789!` for the missing-current-password scenario.

async function openSecurityForm(page: any, creds = h.SEED.owner): Promise<void> {
  await h.signIn(page, creds);
  await h.openAccountMenu(page);
  await h.clickNamed(page, /settings/i);
  await h.clickNamed(page, /password and authentication/i);
  await expect(page.getByLabel(/current password/i)).toBeVisible();
}

test('REQ-1-3: the security form exposes the three labeled inputs and the update button', async ({ page }) => {
  await openSecurityForm(page);
  await expect(page.getByLabel(/new password/i)).toBeVisible();
  await expect(page.getByLabel(/confirm password/i)).toBeVisible();
  await expect(page.getByRole('button', { name: /update password/i })).toBeVisible();
});

test('REQ-1-3: an empty current password reports Current password is required', async ({ page }) => {
  await openSecurityForm(page);
  await h.fillField(page, /current password/i, '');
  await h.fillField(page, /new password/i, h.SEED.requiredPassword);
  await h.fillField(page, /confirm password/i, h.SEED.requiredPassword);
  await h.clickNamed(page, /update password/i);

  await h.expectErrorText(page, 'Current password is required');
});

test('REQ-1-3: an incorrect current password is rejected and the old credentials keep working', async ({ page }) => {
  await openSecurityForm(page);
  await h.fillField(page, /current password/i, 'Not-the-current-000!');
  await h.fillField(page, /new password/i, h.SEED.newPassword);
  await h.fillField(page, /confirm password/i, h.SEED.newPassword);
  await h.clickNamed(page, /update password/i);

  await h.expectErrorText(page, 'Current password is incorrect');
  await h.signIn(page, h.SEED.owner);
  await expect(await h.signedInUsername(page)).toBeVisible();
});

test('REQ-1-3: a mismatched confirmation is rejected with its own message', async ({ page }) => {
  await openSecurityForm(page);
  await h.fillField(page, /current password/i, h.SEED.owner.password);
  await h.fillField(page, /new password/i, h.SEED.newPassword);
  await h.fillField(page, /confirm password/i, 'does-not-match');
  await h.clickNamed(page, /update password/i);

  await h.expectErrorText(page, 'Password confirmation does not match');
});

test('REQ-1-3: a successful update reports Password updated and swaps which password signs in', async ({ page }) => {
  const creds = await h.registerAccount(page);
  await openSecurityForm(page, creds);
  await h.fillField(page, /current password/i, creds.password);
  await h.fillField(page, /new password/i, h.SEED.newPassword);
  await h.fillField(page, /confirm password/i, h.SEED.newPassword);
  await h.clickNamed(page, /update password/i);

  await expect(page.getByText(/password updated/i).first()).toBeVisible();
  await h.signIn(page, { username: creds.username, password: h.SEED.newPassword });
  await expect(await h.signedInUsername(page)).toBeVisible();

  await h.signOut(page);
  await h.openHome(page);
  await h.clickNamed(page, /^sign in$/i);
  await h.fillField(page, /username or email/i, creds.username);
  await h.fillField(page, /^password$/i, creds.password);
  await h.clickNamed(page, /^sign in$/i);
  await h.expectErrorText(page, 'Invalid credentials');
});
