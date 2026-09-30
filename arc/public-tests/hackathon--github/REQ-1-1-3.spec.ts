import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-1-1-3
// fixtures: password_recovery
//
// Quoted: the link "Forgot password", the field labels "Email", "Verification
// code", "New password", "Confirm password", the buttons "Send reset link" and
// "Reset password", the fixed code displayed as its own visible text "123456", and
// the rejection "Verification code is invalid". Registered and unknown addresses
// must reach the same next step with the same code, so the page cannot disclose
// whether an account exists.

async function openRecovery(page: any): Promise<void> {
  await h.openHome(page);
  await h.clickNamed(page, /^sign in$/i);
  await h.clickNamed(page, /forgot password/i);
}

async function sendResetLink(page: any, email: string): Promise<void> {
  await openRecovery(page);
  await h.fillField(page, /^email$/i, email);
  await h.clickNamed(page, /send reset link/i);
}

test('REQ-1-1-3: sending the reset link moves to the same next step with the fixed code visible as its own text', async ({ page }) => {
  await sendResetLink(page, h.SEED.owner.email!);

  await expect(page.getByText(h.SEED.resetCode, { exact: true }).first()).toBeVisible();
  await expect(h.textbox(page, 'Verification code')).toBeVisible();
  await expect(page.getByLabel(/new password/i)).toBeVisible();
  await expect(page.getByLabel(/confirm password/i)).toBeVisible();
  await expect(page.getByRole('button', { name: /reset password/i })).toBeVisible();
});

test('REQ-1-1-3: an unknown address reaches the same step and shows the same code', async ({ page }) => {
  await sendResetLink(page, `${h.unique('pw-nobody-')}@example.test`);

  await expect(page.getByText(h.SEED.resetCode, { exact: true }).first()).toBeVisible();
  await expect(h.textbox(page, 'Verification code')).toBeVisible();
});

test('REQ-1-1-3: a wrong code is rejected and leaves the old password usable', async ({ page }) => {
  await sendResetLink(page, h.SEED.owner.email!);
  await h.fillField(page, /verification code/i, '000000');
  await h.fillField(page, /new password/i, h.SEED.resetPassword);
  await h.fillField(page, /confirm password/i, h.SEED.resetPassword);
  await h.clickNamed(page, /reset password/i);

  await h.expectErrorText(page, 'Verification code is invalid');
  await h.signIn(page, h.SEED.owner);
  await expect(await h.signedInUsername(page)).toBeVisible();
});

test('REQ-1-1-3: a correct code updates the credentials and reports Password updated', async ({ page }) => {
  const creds = await h.registerAccount(page);
  const replacement = 'Replacement-password-456!';

  await sendResetLink(page, creds.email!);
  await h.fillField(page, /verification code/i, h.SEED.resetCode);
  await h.fillField(page, /new password/i, replacement);
  await h.fillField(page, /confirm password/i, replacement);
  await h.clickNamed(page, /reset password/i);

  await expect(page.getByText(/password updated/i).first()).toBeVisible();
  await h.signIn(page, { username: creds.username, password: replacement });
  await expect(await h.signedInUsername(page)).toBeVisible();
});
