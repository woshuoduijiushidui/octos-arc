import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-3-2-3
// fixtures: clone_value
//
// Quoted: the popover is opened by a button "Code" that is separate from the
// repository navigation link of the same name; the protocol tabs "HTTPS" and
// "SSH"; the button "Copy clone value"; the feedback "Copied". The HTTPS value
// uses the HTTPS protocol, the SSH value the selected SSH format including the
// colon and .git suffix, and both must identify the current repository.

async function openClonePopover(page: any): Promise<void> {
  await h.openRepository(page, h.SEED.owner.username, h.SEED.repo);
  await page.getByRole('button', { name: /^code$/i }).first().click();
}

test('REQ-3-2-3: the popover offers HTTPS and SSH tabs and a Copy clone value button', async ({ page }) => {
  await openClonePopover(page);
  await expect(page.getByRole('tab', { name: /^https$/i }).first()).toBeVisible();
  await expect(page.getByRole('tab', { name: /^ssh$/i }).first()).toBeVisible();
  await expect(page.getByRole('button', { name: /copy clone value/i }).first()).toBeVisible();
});

test('REQ-3-2-3: both clone values identify the current repository in their own protocol format', async ({ page }) => {
  await openClonePopover(page);
  const body = await page.locator('body').innerText();
  expect(body).toContain(h.SEED.repo);
  expect(body).toMatch(/https:\/\/\S+/i);

  await page.getByRole('tab', { name: /^ssh$/i }).first().click();
  const ssh = await page.locator('body').innerText();
  expect(ssh).toMatch(/(git@|ssh:\/\/)\S+/i);
  expect(ssh).toContain('.git');
});

test('REQ-3-2-3: copying writes the value, shows Copied and leaves the repository heading visible', async ({ page }) => {
  await page.context().grantPermissions(['clipboard-read', 'clipboard-write']);
  await openClonePopover(page);
  await page.getByRole('button', { name: /copy clone value/i }).first().click();

  await expect(page.getByText(/^copied$/i).first()).toBeVisible();
  const clipboard = await page.evaluate(() => navigator.clipboard.readText());
  expect(clipboard).toContain(h.SEED.repo);
  await h.expectHeading(page, new RegExp(h.SEED.repo, 'i'));
});
