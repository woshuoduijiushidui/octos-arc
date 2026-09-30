import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-1-2-2
// fixtures: seed_workbook_q3_sales
//
// Quoted strings the requirement fixes: the button "Rename workbook", the text
// box "Workbook name" prefilled with the last saved name, the button "Save", and
// the rejection "Workbook name cannot be empty" after trimming leading and
// trailing spaces.

const RENAMED = 'Q3 Sales renamed';

async function openRenameDialog(page: any): Promise<any> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.clickNamed(page, /rename workbook/i);
  const box = page.getByRole('textbox', { name: /workbook name/i }).first();
  await expect(box).toBeVisible();
  return box;
}

test('REQ-1-2-2: the rename text box is prefilled with the last saved name', async ({ page }) => {
  const box = await openRenameDialog(page);
  await expect(box).toHaveValue(new RegExp(h.SEED.workbook, 'i'));
});

test('REQ-1-2-2: a name that is empty after trimming is rejected and the original name remains', async ({ page }) => {
  const box = await openRenameDialog(page);
  await box.fill('   ');
  await h.clickNamed(page, /^save$/i);

  await h.expectErrorText(page, 'Workbook name cannot be empty');
  await h.expectVisible(page, h.SEED.workbook);
});

test('REQ-1-2-2: a saved rename shows on the editor and the home page and survives reopening', async ({ page }) => {
  const box = await openRenameDialog(page);
  await box.fill(RENAMED);
  await h.clickNamed(page, /^save$/i);

  await h.expectVisible(page, RENAMED);
  await h.openHome(page);
  await h.expectWorkbookListed(page, RENAMED);
  await h.openWorkbook(page, RENAMED);
  await h.expectVisible(page, RENAMED);
});
