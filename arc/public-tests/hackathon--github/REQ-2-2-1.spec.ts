import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-2-1
// fixtures: create_team
//
// Quoted: the organization overview link "Teams", the link "New team", the field
// "Team name", the button "Create team", and the rejection "Team name format is
// invalid". A unique compliant name submits without a description or parent, the
// resulting heading contains the name, and it survives reload.

async function openNewTeam(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await h.openOrganization(page);
  await h.clickNamed(page, /^teams$/i);
  await h.clickNamed(page, /new team/i);
}

test('REQ-2-2-1: a unique compliant name creates the team with no description or parent', async ({ page }) => {
  const name = h.unique('pw-team-');
  await openNewTeam(page);
  await h.fillField(page, /team name/i, name);
  await h.clickNamed(page, /create team/i);

  await h.expectHeading(page, new RegExp(name, 'i'));
  await page.reload();
  await h.expectHeading(page, new RegExp(name, 'i'));
});

test('REQ-2-2-1: the team page shows its Members tab and Settings', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await h.openOrganization(page);
  await h.clickNamed(page, /^teams$/i);
  await h.clickNamed(page, new RegExp(`^\\s*${h.SEED.team}\\s*$`));

  await expect(page.getByRole('link', { name: /^members$/i }).first()).toBeVisible();
  await expect(page.getByRole('link', { name: /^settings$/i }).first()).toBeVisible();
});

test('REQ-2-2-1: a malformed team name reports the format error and creates no team', async ({ page }) => {
  const name = h.unique('PW-BAD-');
  await openNewTeam(page);
  await h.fillField(page, /team name/i, name);
  await h.clickNamed(page, /create team/i);

  await h.expectErrorText(page, 'Team name format is invalid');
});
