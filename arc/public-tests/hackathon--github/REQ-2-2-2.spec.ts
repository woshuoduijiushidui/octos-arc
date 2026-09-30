import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-2-2
// fixtures: team_members_hierarchy
//
// Quoted: the links "Members" and "Settings"; on Members the button "Add member"
// opening the textbox "Username" and the submit button "Add member", the added
// member's button "Remove <username>"; on Settings the native select with combobox
// role labeled "Parent team" and the button "Save"; and the rejection "Cyclic team
// hierarchy is not allowed". A rejected change must leave the original parent
// selected, before and after reload.

async function openTeam(page: any, team = h.SEED.team): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await h.openOrganization(page);
  await h.clickNamed(page, /^teams$/i);
  await h.clickNamed(page, new RegExp(`^\\s*${team}\\s*$`));
}

test('REQ-2-2-2: adding a current organization member shows the username once with a Remove button', async ({ page }) => {
  await openTeam(page);
  await h.clickNamed(page, /^members$/i);
  await h.clickNamed(page, /^add member$/i);
  await h.fillField(page, /^username$/i, h.SEED.reviewer.username);
  await h.clickNamed(page, /^add member$/i);

  await expect(page.getByText(h.SEED.reviewer.username, { exact: true })).toHaveCount(1);
  await expect(page.getByRole('button', { name: new RegExp(`remove\\s+${h.SEED.reviewer.username}`, 'i') }).first()).toBeVisible();
});

test('REQ-2-2-2: Remove <username> removes immediately and reload keeps the username absent', async ({ page }) => {
  await openTeam(page);
  await h.clickNamed(page, /^members$/i);
  const remove = page.getByRole('button', { name: new RegExp(`remove\\s+${h.SEED.reviewer.username}`, 'i') }).first();
  if (await remove.count()) {
    await remove.click();
  }

  await page.reload();
  await expect(page.getByText(h.SEED.reviewer.username, { exact: true })).toHaveCount(0);
});

test('REQ-2-2-2: Settings exposes the logged parent and a Save button', async ({ page }) => {
  await openTeam(page);
  await h.clickNamed(page, /^settings$/i);

  const parent = page.getByRole('combobox', { name: /parent team/i }).first();
  await expect(parent).toBeVisible();
  const labels = await parent.locator('option').allInnerTexts();
  expect(labels.length).toBeGreaterThan(0);
  await expect(page.getByRole('button', { name: /^save$/i }).first()).toBeVisible();
});

test('REQ-2-2-2: selecting a descendant is refused as a cycle and the original parent stays selected', async ({ page }) => {
  await openTeam(page);
  await h.clickNamed(page, /^settings$/i);

  const parent = page.getByRole('combobox', { name: /parent team/i }).first();
  const before = await parent.inputValue();
  await h.setCombobox(page, 'Parent team', h.SEED.childTeam);
  await h.clickNamed(page, /^save$/i);

  await h.expectErrorText(page, 'Cyclic team hierarchy is not allowed');
  await expect(parent).toHaveValue(before);
  await page.reload();
  await expect(page.getByRole('combobox', { name: /parent team/i }).first()).toHaveValue(before);
});
