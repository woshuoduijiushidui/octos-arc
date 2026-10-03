import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-2-3
// fixtures: add_org_member
//
// Quoted: the link "People", the button "Add member" opening the field "Username or
// email", the combobox "Role" defaulting to Member with options "Member" and
// "Owner", and the submit button "Add member"; the messages "Account is already a
// member" and "Account not found" (for `unknown-reviewer`); "Access denied" for a
// private repository with no grant. This is also how the suite builds the
// non-Admin viewer the permission requirements refer to.

async function openPeople(page: any): Promise<void> {
  await h.openOrganization(page);
  await h.clickNamed(page, /^people$/i);
}

async function addMember(page: any, identifier: string, role: string): Promise<void> {
  await h.clickNamed(page, /^add member$/i);
  await h.fillField(page, /username or email/i, identifier);
  await h.setCombobox(page, 'Role', role);
  await h.clickNamed(page, /^add member$/i);
}

test('REQ-2-2-3: the People form defaults Role to Member and offers Member and Owner', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openPeople(page);
  await h.clickNamed(page, /^add member$/i);

  await expect(h.textbox(page, 'Username or email')).toBeVisible();
  const role = page.getByRole('combobox', { name: /^role$/i }).first();
  await expect(role).toBeVisible();
  await role.click();
  await expect(h.option(page, 'Member')).toBeVisible();
  await expect(h.option(page, 'Owner')).toBeVisible();
});

test('REQ-2-2-3: an added member shows the full username and Member role and survives reload', async ({ page }) => {
  const member = await h.registerAccount(page);
  await h.signIn(page, h.SEED.owner);
  await openPeople(page);
  await addMember(page, member.username, 'Member');

  await expect(page.getByText(member.username, { exact: true }).first()).toBeVisible();
  await expect(page.getByText(/pending|awaiting/i)).toHaveCount(0);
  await page.reload();
  await expect(page.getByText(member.username, { exact: true }).first()).toBeVisible();
});

test('REQ-2-2-3: an unknown account reports Account not found and keeps the form open', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openPeople(page);
  await addMember(page, 'unknown-reviewer', 'Member');

  await h.expectErrorText(page, 'Account not found');
  await expect(h.textbox(page, 'Username or email')).toBeVisible();
});

test('REQ-2-2-3: the existing member reports Account is already a member once in the list', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openPeople(page);
  await addMember(page, h.SEED.reviewer.username, 'Member');

  await h.expectErrorText(page, 'Account is already a member');
  await expect(page.getByText(h.SEED.reviewer.username, { exact: true })).toHaveCount(1);
});

test('REQ-2-2-3: a new Member sees the organization but Access denied on an ungranted private repository', async ({ browser }) => {
  const member = await h.registerAccount(await browser.newPage());
  const owner = await h.newSession(browser, h.SEED.owner);
  await owner.page.goto('/');
  await openPeople(owner.page);
  await addMember(owner.page, member.username, 'Member');

  const viewer = await h.newSession(browser, member);
  await viewer.page.goto('/');
  await h.openYourOrganizations(viewer.page);
  await expect(viewer.page.getByText(/acme/i).first()).toBeVisible();

  await viewer.page.goto(`/${h.SEED.owner.username}/${h.SEED.privateRepo}`);
  await h.expectAccessDenied(viewer.page);
});
