import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-1-3
// fixtures: seed_workbook_q3_sales
//
// Quoted strings: the menu item "Rename", the dialog "Rename worksheet", the
// text box "Worksheet name" (prefilled with the current name), the button
// "Save", and the two rejections "Worksheet name cannot be empty" and
// "Worksheet name already exists".

async function openRenameDialog(page: any, from = h.SEED.worksheet): Promise<any> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.openWorksheetTabMenu(page, from);
  await h.menuItem(page, /^rename$/i).click();
  const box = page.getByRole('textbox', { name: /worksheet name/i }).first();
  await expect(box).toBeVisible();
  return box;
}

test('REQ-2-1-3: the dialog is named and its text box is prefilled with the current name', async ({ page }) => {
  const box = await openRenameDialog(page);
  await h.expectDialogVisible(page, 'Rename worksheet');
  await expect(box).toHaveValue(new RegExp(h.SEED.worksheet, 'i'));
});

test('REQ-2-1-3: a name that is empty after trimming is rejected and the original name remains', async ({ page }) => {
  const box = await openRenameDialog(page);
  await box.fill('  ');
  await h.clickNamed(page, /^save$/i);

  await h.expectErrorText(page, 'Worksheet name cannot be empty');
  await h.expectWorksheetTab(page, h.SEED.worksheet);
});

test('REQ-2-1-3: a duplicate name is rejected and the original name remains', async ({ page }) => {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.clickNamed(page, /add worksheet/i);

  const box = await openRenameDialog(page, 'Sheet2');
  await box.fill(h.SEED.worksheet);
  await h.clickNamed(page, /^save$/i);

  await h.expectErrorText(page, 'Worksheet name already exists');
  await h.expectWorksheetTab(page, 'Sheet2');
});

test('REQ-2-1-3: a saved rename shows on the tab and survives refresh', async ({ page }) => {
  const box = await openRenameDialog(page);
  await box.fill('Renamed sheet');
  await h.clickNamed(page, /^save$/i);

  await h.expectWorksheetTab(page, 'Renamed sheet');
  await page.reload();
  await h.expectWorksheetTab(page, 'Renamed sheet');
});
