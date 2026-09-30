import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-4-4
// fixtures: file_editor
//
// Quoted: the unique "Add file" button opening the "Create new file" menuitem; the
// editor's field "File name", textbox "File contents", initially empty field
// "Commit message", and "Commit changes" button; the messages "Invalid file path"
// and "Commit message is required". Only Write/Maintain/Admin/Owner may submit.

async function openEditor(page: any): Promise<void> {
  await h.signIn(page, h.SEED.owner);
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.repoNav(page, 'Code').click();
  await page.getByRole('button', { name: /add file/i }).first().click();
  await h.clickNamed(page, /create new file/i);
}

test('REQ-4-4: the editor exposes the labeled fields with an initially empty commit message', async ({ page }) => {
  await openEditor(page);
  await expect(page.getByLabel(/file name/i)).toBeVisible();
  await expect(h.textbox(page, 'File contents')).toBeVisible();
  await expect(page.getByLabel(/commit message/i)).toHaveValue('');
  await expect(page.getByRole('button', { name: /commit changes/i })).toBeVisible();
});

test('REQ-4-4: creating a file opens it with the exact saved content and its commit message in history', async ({ page }) => {
  const name = `${h.unique('pw-file-')}.md`;
  await openEditor(page);
  await h.fillField(page, /file name/i, name);
  await h.fillField(page, /file contents/i, 'Content added by the Playwright suite.');
  await h.fillField(page, /commit message/i, `Add ${name}`);
  await h.clickNamed(page, /commit changes/i);

  await expect(page.getByText('Content added by the Playwright suite.', { exact: false }).first()).toBeVisible();

  await page.getByRole('link', { name: /^commits\b/i }).first().click();
  await expect(page.getByText(`Add ${name}`, { exact: true }).first()).toBeVisible();
});

test('REQ-4-4: an invalid path with no commit message is refused and changes neither files nor history', async ({ page }) => {
  await openEditor(page);
  await h.fillField(page, /file name/i, '../invalid.md');
  await h.fillField(page, /file contents/i, 'must not be saved');
  await h.clickNamed(page, /commit changes/i);

  const invalid = page.getByText('Invalid file path', { exact: false }).first();
  const required = page.getByText('Commit message is required', { exact: false }).first();
  await expect(invalid.or(required)).toBeVisible();
  await expect(page.getByText('must not be saved', { exact: false })).toHaveCount(0);
});

test('REQ-4-4: the created file entry is a link named exactly after the file and survives reload', async ({ page }) => {
  const name = `${h.unique('pw-file-')}.md`;
  await openEditor(page);
  await h.fillField(page, /file name/i, name);
  await h.fillField(page, /file contents/i, 'Reload check.');
  await h.fillField(page, /commit message/i, `Add ${name}`);
  await h.clickNamed(page, /commit changes/i);

  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.repoNav(page, 'Code').click();
  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${name.replace('.', '\\.')}\\s*$`) }).first()).toBeVisible();
});
