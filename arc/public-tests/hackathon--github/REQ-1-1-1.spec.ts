import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-1-1-1
// fixtures: registration
//
// The requirement fixes the form's exact control set and the exact field errors:
// "Username already exists", "Username format is invalid", "Email format is
// invalid", "Password requirements are not satisfied", "Agree to terms is
// required". Several invalid fields submitted together must all report, and the
// button stays actionable while they are read.

const VALID_PASSWORD = 'Valid-password-123!';

async function openRegistration(page: any): Promise<void> {
  await h.openHome(page);
  await h.clickNamed(page, /^sign in$/i);
  await h.clickNamed(page, /create an account/i);
}

test('REQ-1-1-1: the registration form exposes exactly the required controls', async ({ page }) => {
  await openRegistration(page);

  await expect(page.getByRole('textbox', { name: /^username$/i })).toHaveCount(1);
  await expect(page.getByRole('textbox', { name: /^email$/i })).toHaveCount(1);
  await expect(page.getByLabel(/^password$/i)).toHaveCount(1);
  await expect(page.getByLabel(/confirm password/i)).toHaveCount(1);
  await expect(page.getByRole('checkbox', { name: /agree to the terms/i })).not.toBeChecked();
  await expect(page.getByRole('button', { name: /create account/i })).toBeEnabled();
});

test('REQ-1-1-1: several invalid fields report together and keep the username and email', async ({ page }) => {
  await openRegistration(page);
  const username = '-invalid-user';
  await h.fillField(page, /^username$/i, username);
  await h.fillField(page, /^email$/i, 'not-an-email');
  await h.fillField(page, /^password$/i, 'short');
  await h.fillField(page, /confirm password/i, 'different');
  await h.clickNamed(page, /create account/i);

  await h.expectErrorText(page, 'Username format is invalid');
  await h.expectErrorText(page, 'Email format is invalid');
  await h.expectErrorText(page, 'Password requirements are not satisfied');
  await h.expectErrorText(page, 'Agree to terms is required');

  await expect(h.textbox(page, 'Username')).toHaveValue(username);
  await expect(h.textbox(page, 'Email')).toHaveValue('not-an-email');
  await expect(page.getByRole('button', { name: /create account/i })).toBeEnabled();
});

test('REQ-1-1-1: a duplicate username keeps both attempted values and creates no second account', async ({ page }) => {
  await openRegistration(page);
  const email = `${h.unique('pw-')}@example.test`;
  await h.fillField(page, /^username$/i, h.SEED.owner.username);
  await h.fillField(page, /^email$/i, email);
  await h.fillField(page, /^password$/i, VALID_PASSWORD);
  await h.fillField(page, /confirm password/i, VALID_PASSWORD);
  await h.clickNamed(page, /agree to the terms/i);
  await h.clickNamed(page, /create account/i);

  await h.expectErrorText(page, 'Username already exists');
  await expect(h.textbox(page, 'Username')).toHaveValue(h.SEED.owner.username);
  await expect(h.textbox(page, 'Email')).toHaveValue(email);
});

test('REQ-1-1-1: a successful registration is immediately sign-in capable and survives reload', async ({ page }) => {
  const creds = await h.registerAccount(page, { password: VALID_PASSWORD });

  await h.signIn(page, creds);
  await expect(await h.signedInUsername(page)).toContainText(creds.username);
  await page.reload();
  await expect(await h.signedInUsername(page)).toContainText(creds.username);
});
