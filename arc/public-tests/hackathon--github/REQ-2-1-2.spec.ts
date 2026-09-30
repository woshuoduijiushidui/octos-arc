import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-1-2
// fixtures: create_organization
//
// Quoted: the account-menu link "Your organizations", the link "New organization",
// the fields "Organization name" and "Display name", the button "Create
// organization", and the messages "Organization name already exists",
// "Organization name format is invalid", "Display name is required". A duplicate
// identifier reports even when the display name is also missing, and must not
// navigate to the existing organization.

async function openCreateForm(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await h.openYourOrganizations(page);
  await h.clickNamed(page, /new organization/i);
}

test('REQ-2-1-2: a unique identifier creates the organization and its overview survives reload', async ({ page }) => {
  const name = h.unique('pw-org-');
  await openCreateForm(page);
  await h.fillField(page, /organization name/i, name);
  await h.fillField(page, /display name/i, 'Playwright Organization');
  await h.clickNamed(page, /create organization/i);

  await h.expectHeading(page, new RegExp(name, 'i'));
  await page.reload();
  await h.expectHeading(page, new RegExp(name, 'i'));
});

test('REQ-2-1-2: a duplicate identifier reports even when the display name is missing', async ({ page }) => {
  await openCreateForm(page);
  await h.fillField(page, /organization name/i, h.SEED.orgIdentifier);
  await h.fillField(page, /display name/i, '');
  await h.clickNamed(page, /create organization/i);

  await h.expectErrorText(page, 'Organization name already exists');
  await expect(h.textbox(page, 'Organization name')).toBeVisible();
});

test('REQ-2-1-2: a malformed identifier and a whitespace-only display name each report their own error', async ({ page }) => {
  await openCreateForm(page);
  await h.fillField(page, /organization name/i, '-invalid-organization');
  await h.fillField(page, /display name/i, '   ');
  await h.clickNamed(page, /create organization/i);

  await h.expectErrorText(page, 'Organization name format is invalid');
  await h.expectErrorText(page, 'Display name is required');
});

test('REQ-2-1-2: the new organization appears in the current user organization list', async ({ page }) => {
  const name = h.unique('pw-org-');
  await openCreateForm(page);
  await h.fillField(page, /organization name/i, name);
  await h.fillField(page, /display name/i, 'Listed Organization');
  await h.clickNamed(page, /create organization/i);

  await h.openYourOrganizations(page);
  await expect(page.getByText(new RegExp(name, 'i')).first()).toBeVisible();
});
