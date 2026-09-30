import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-3
// fixtures: repo_access
//
// Quoted: the repository "Settings" link, the "Manage access" link, the button
// "Add people or teams"; in the picker the textbox "Search", a matching team
// option, the combobox "Role" with a clickable "Write" option and an "Add" button;
// then rows whose accessible names include the subject name, each with a native
// select "Role" and a "Save" button. While the picker is open the opening button
// must be hidden so the submit action is unambiguous.

async function openManageAccess(page: any, repo = h.SEED.repo): Promise<void> {
  await h.openRepository(page, h.SEED.owner.username, repo);
  await h.openSettingsSub(page, /^manage access$/i);
}

test('REQ-2-3: the picker searches, offers the team as an option and a Write role', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openManageAccess(page);
  await h.clickNamed(page, /add people or teams/i);

  await expect(h.textbox(page, 'Search')).toBeVisible();
  await expect(page.getByRole('combobox', { name: /^role$/i }).first()).toBeVisible();
  await expect(page.getByRole('button', { name: /^add$/i }).first()).toBeVisible();

  await h.fillField(page, /^search$/i, h.SEED.team);
  await expect(page.getByRole('option', { name: new RegExp(h.SEED.team, 'i') }).first()).toBeVisible();
});

test('REQ-2-3: the opening button is hidden while the picker is active', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openManageAccess(page);
  await h.clickNamed(page, /add people or teams/i);

  await h.expectAbsent(page, /^add people or teams$/i);
});

test('REQ-2-3: granting Write to the team shows it in the list and survives reload', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openManageAccess(page);
  await h.clickNamed(page, /add people or teams/i);
  await h.fillField(page, /^search$/i, h.SEED.team);
  await page.getByRole('option', { name: new RegExp(h.SEED.team, 'i') }).first().click();
  await h.setCombobox(page, 'Role', 'Write');
  await h.clickNamed(page, /^add$/i);

  const row = page.getByRole('row', { name: new RegExp(h.SEED.team, 'i') }).first();
  if (!(await row.count())) {
    await expect(page.getByText(h.SEED.team, { exact: true }).first()).toBeVisible();
  }
  await page.reload();
  await expect(page.getByText(h.SEED.team, { exact: true }).first()).toBeVisible();
});

test('REQ-2-3: saving another role replaces the existing grant instead of adding a second row', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openManageAccess(page);

  const row = page.getByRole('row', { name: new RegExp(h.SEED.team, 'i') }).first();
  if (await row.count()) {
    await row.getByRole('combobox', { name: /^role$/i }).first().selectOption({ label: 'Read' });
    await row.getByRole('button', { name: /^save$/i }).first().click();
    await page.reload();
  }

  await expect(page.getByRole('row', { name: new RegExp(h.SEED.team, 'i') })).toHaveCount(1);
});

test('REQ-2-3: an ordinary member has no access to a private repository without a grant', async ({ browser }) => {
  const member = await h.registerAccount(await browser.newPage());
  const owner = await h.newSession(browser, h.SEED.owner);
  await owner.page.goto('/');
  await h.openOrganization(owner.page);
  await h.clickNamed(owner.page, /^people$/i);
  await h.clickNamed(owner.page, /^add member$/i);
  await h.fillField(owner.page, /username or email/i, member.username);
  await h.setCombobox(owner.page, 'Role', 'Member');
  await h.clickNamed(owner.page, /^add member$/i);

  const viewer = await h.newSession(browser, member);
  await viewer.page.goto(`/${h.SEED.owner.username}/${h.SEED.privateRepo}`);
  await h.expectAccessDenied(viewer.page);
});
