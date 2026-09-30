import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-3-2-1
// fixtures: create_repository
//
// Quoted: the signed-in workspace link "New repository"; the form labels "Owner",
// "Repository name", "Description"; the visibility radios "Public" and "Private";
// the initialization checkbox "Add a README file"; the button "Create
// repository". The personal namespace is selected by default. A successful
// initialized private repository shows a heading with the new name, a visible
// Private marker and a README link, and the overview survives reload.

async function openCreateForm(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await h.openHome(page);
  await h.clickNamed(page, /new repository/i);
}

test('REQ-3-2-1: the form exposes the labeled controls with the personal namespace preselected', async ({ page }) => {
  await openCreateForm(page);
  await expect(page.getByLabel(/^owner$/i)).toBeVisible();
  await expect(h.textbox(page, 'Repository name')).toBeVisible();
  await expect(page.getByLabel(/^description$/i)).toBeVisible();
  await expect(page.getByRole('radio', { name: /^public$/i }).first()).toBeVisible();
  await expect(page.getByRole('radio', { name: /^private$/i }).first()).toBeVisible();
  await expect(page.getByRole('checkbox', { name: /add a readme file/i }).first()).toBeVisible();
  await expect(page.getByRole('button', { name: /create repository/i })).toBeVisible();
});

test('REQ-3-2-1: an initialized private repository shows its name, a Private marker and a README link', async ({ page }) => {
  const name = h.unique('pw-repo-');
  await openCreateForm(page);
  await h.fillField(page, /repository name/i, name);
  await h.fillField(page, /description/i, 'Repository created by Playwright');
  await page.getByRole('radio', { name: /^private$/i }).first().click();
  await page.getByRole('checkbox', { name: /add a readme file/i }).first().click();
  await page.getByRole('button', { name: /create repository/i }).click();

  await h.expectHeading(page, new RegExp(name, 'i'));
  await h.expectPrivateMarker(page);
  await expect(page.getByRole('link', { name: /readme/i }).first()).toBeVisible();

  await page.reload();
  await h.expectHeading(page, new RegExp(name, 'i'));
});

test('REQ-3-2-1: a duplicate name in the same namespace stays on the form and does not open the existing repository', async ({ page }) => {
  await openCreateForm(page);
  await h.fillField(page, /repository name/i, h.SEED.repo);
  await page.getByRole('button', { name: /create repository/i }).click();

  await expect(h.textbox(page, 'Repository name')).toBeVisible();
  await expect(page.getByRole('button', { name: /create repository/i })).toBeVisible();
});

test('REQ-3-2-1: the entered description is saved on the new repository', async ({ page }) => {
  const name = h.unique('pw-repo-');
  await openCreateForm(page);
  await h.fillField(page, /repository name/i, name);
  await h.fillField(page, /description/i, 'Repository created by Playwright');
  await page.getByRole('button', { name: /create repository/i }).click();

  await expect(page.getByText('Repository created by Playwright').first()).toBeVisible();
});
