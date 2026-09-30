import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-2-4
// fixtures: remove_org_member
//
// Quoted: the link "People", each member's button "Member menu <username>", the
// menuitem "Remove from organization", and the confirmation button "Remove".
// Success makes the complete username absent immediately and after reload. A
// non-Owner must not have the member menu at all — absent, not merely disabled.

async function openPeople(page: any): Promise<void> {
  await h.openOrganization(page);
  await h.clickNamed(page, /^people$/i);
}

test('REQ-2-2-4: the member menu holds Remove from organization and the confirmation is named Remove', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openPeople(page);

  await page.getByRole('button', { name: new RegExp(`member menu\\s+${h.SEED.reviewer.username}`, 'i') }).first().click();
  await expect(page.getByRole('menuitem', { name: /remove from organization/i }).first()).toBeVisible();
  await page.getByRole('menuitem', { name: /remove from organization/i }).first().click();
  await expect(page.getByRole('button', { name: /^remove$/i }).first()).toBeVisible();
});

test('REQ-2-2-4: a removed member is absent from People immediately and after reload', async ({ page }) => {
  const member = await h.registerAccount(page);
  await h.signIn(page, h.SEED.owner);
  await openPeople(page);
  await h.clickNamed(page, /^add member$/i);
  await h.fillField(page, /username or email/i, member.username);
  await h.setCombobox(page, 'Role', 'Member');
  await h.clickNamed(page, /^add member$/i);
  await expect(page.getByText(member.username, { exact: true }).first()).toBeVisible();

  await page.getByRole('button', { name: new RegExp(`member menu\\s+${member.username}`, 'i') }).first().click();
  await page.getByRole('menuitem', { name: /remove from organization/i }).first().click();
  await page.getByRole('button', { name: /^remove$/i }).first().click();

  await expect(page.getByText(member.username, { exact: true })).toHaveCount(0);
  await page.reload();
  await expect(page.getByText(member.username, { exact: true })).toHaveCount(0);
});

test('REQ-2-2-4: a non-Owner member has no member menu and no Remove from organization item', async ({ browser }) => {
  const member = await h.registerAccount(await browser.newPage());
  const owner = await h.newSession(browser, h.SEED.owner);
  await owner.page.goto('/');
  await openPeople(owner.page);
  await h.clickNamed(owner.page, /^add member$/i);
  await h.fillField(owner.page, /username or email/i, member.username);
  await h.setCombobox(owner.page, 'Role', 'Member');
  await h.clickNamed(owner.page, /^add member$/i);

  const viewer = await h.newSession(browser, member);
  await viewer.page.goto('/');
  await openPeople(viewer.page);

  await expect(viewer.page.getByRole('button', { name: new RegExp('member menu', 'i') })).toHaveCount(0);
  await expect(viewer.page.getByRole('menuitem', { name: /remove from organization/i })).toHaveCount(0);
});
