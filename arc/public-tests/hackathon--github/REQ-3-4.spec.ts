import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-3-4
// fixtures: repository_visibility
//
// Quoted: the repository "Settings" link then its "General" link reaching the
// Danger Zone; the button "Change visibility"; the confirmation's "Public" radio
// and "Confirm visibility" button. The change needs no retyped repository name
// and no other mandatory field. A non-Admin collaborator must not see the Change
// visibility button at all.

async function openDangerZone(page: any, repo = h.SEED.privateRepo): Promise<void> {
  await h.openRepository(page, h.SEED.owner.username, repo);
  await h.openSettings(page);
  await h.clickNamed(page, /^general$/i);
}

test('REQ-3-4: an Admin can switch a private repository to Public without retyping its name', async ({ page }) => {
  await h.signIn(page, h.SEED.owner);
  await openDangerZone(page);
  await h.clickNamed(page, /change visibility/i);

  await page.getByRole('radio', { name: /^public$/i }).first().click();
  await h.clickNamed(page, /confirm visibility/i);

  await h.expectPublicMarker(page);
});

test('REQ-3-4: once Public, an unauthenticated visitor reopening the address sees the heading', async ({ browser }) => {
  const admin = await h.newSession(browser, h.SEED.owner);
  await admin.page.goto(`/${h.SEED.owner.username}/${h.SEED.privateRepo}`);
  await openDangerZone(admin.page);
  await admin.page.getByRole('button', { name: /change visibility/i }).first().click();
  await admin.page.getByRole('radio', { name: /^public$/i }).first().click();
  await h.clickNamed(admin.page, /confirm visibility/i);

  const visitor = await browser.newPage();
  await visitor.goto(`/${h.SEED.owner.username}/${h.SEED.privateRepo}`);
  await h.expectHeading(visitor, new RegExp(h.SEED.privateRepo, 'i'));
});

test('REQ-3-4: a non-Admin collaborator has no Change visibility button', async ({ browser }) => {
  const reviewer = await h.newSession(browser, { username: h.SEED.reviewer.username, password: h.SEED.reviewer.password });
  await reviewer.page.goto(`/${h.SEED.owner.username}/${h.SEED.privateRepo}`);
  await reviewer.page.getByRole('link', { name: /^settings$/i }).first().click().catch(() => undefined);
  await reviewer.page.getByRole('link', { name: /^general$/i }).first().click().catch(() => undefined);

  await h.expectAbsent(reviewer.page, /^change visibility$/i);
});
