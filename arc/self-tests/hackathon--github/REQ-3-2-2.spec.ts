import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-3-2-2
// fixtures: fork_repository
//
// Quoted: the source overview button "Fork"; the fork form field "Repository name"
// and button "Create fork"; success shows a heading containing the new fork name and
// the visible text "Forked from <source repository name>" with a source link. The
// conflict seed uses the existing name `acme-docs-fork` in the target namespace.

async function openForkForm(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await h.openRepository(page, h.SEED.owner.username, h.SEED.repo);
  await h.clickNamed(page, /^fork$/i);
}

test('REQ-3-2-2: the fork form defaults to the personal namespace and submits after only renaming', async ({ page }) => {
  const name = h.unique('pw-fork-');
  await openForkForm(page);
  await expect(h.textbox(page, 'Repository name')).toBeVisible();
  await h.fillField(page, /repository name/i, name);
  await h.clickNamed(page, /create fork/i);

  await h.expectHeading(page, new RegExp(name, 'i'));
  await expect(page.getByText(new RegExp(`forked from\\s+${h.SEED.repo}`, 'i')).first()).toBeVisible();
  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.repo}\\s*$`) }).first()).toBeVisible();
});

test('REQ-3-2-2: the fork heading and source relationship survive reload', async ({ page }) => {
  const name = h.unique('pw-fork-');
  await openForkForm(page);
  await h.fillField(page, /repository name/i, name);
  await h.clickNamed(page, /create fork/i);

  await page.reload();
  await h.expectHeading(page, new RegExp(name, 'i'));
  await expect(page.getByText(new RegExp(`forked from\\s+${h.SEED.repo}`, 'i')).first()).toBeVisible();
});

test('REQ-3-2-2: the conflicting seed name cannot create a second fork', async ({ page }) => {
  await openForkForm(page);
  await h.fillField(page, /repository name/i, h.SEED.forkName);
  await h.clickNamed(page, /create fork/i);

  await expect(h.textbox(page, 'Repository name')).toBeVisible();
  await expect(page.getByRole('button', { name: /create fork/i })).toBeVisible();
});
