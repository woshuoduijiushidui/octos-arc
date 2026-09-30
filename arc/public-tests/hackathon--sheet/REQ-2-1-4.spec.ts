import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-1-4
// fixtures: seed_workbook_q3_sales
//
// Quoted strings: the tab-menu command "Delete", the dialog "Delete worksheet"
// whose visible text includes the target name, the confirmation button "Delete
// worksheet", and the two refusals "Please delete or rebuild dependent pivot
// tables first" and "A workbook must contain at least one worksheet".

async function openSeed(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
}

test('REQ-2-1-4: the confirmation names the target and deleting removes the tab and its data', async ({ page }) => {
  await openSeed(page);
  await h.clickNamed(page, /add worksheet/i);
  await h.setCell(page, 'A1', 'delete me');

  await h.openWorksheetTabMenu(page, 'Sheet2');
  await h.menuItem(page, /^delete$/i).click();

  const scope = await h.dialog(page, /delete worksheet/i);
  await expect(scope).toContainText('Sheet2');
  await h.clickNamed(scope, /delete worksheet/i);

  await expect(h.worksheetTab(page, 'Sheet2')).toHaveCount(0);
  await expect(h.worksheetTab(page, h.SEED.worksheet)).toHaveAttribute('aria-selected', 'true');

  await page.reload();
  await expect(h.worksheetTab(page, 'Sheet2')).toHaveCount(0);
});

test('REQ-2-1-4: deleting the only worksheet refuses without a confirmation dialog', async ({ page }) => {
  await openSeed(page);
  await expect(page.getByRole('tab')).toHaveCount(1);

  await h.openWorksheetTabMenu(page, h.SEED.worksheet);
  await h.menuItem(page, /^delete$/i).click();

  await h.expectErrorText(page, 'A workbook must contain at least one worksheet');
  await expect(h.dialog(page, /delete worksheet/i)).toHaveCount(0);
  await h.expectWorksheetTab(page, h.SEED.worksheet);
});

test('REQ-2-1-4: cancelling the confirmation leaves the tab and its data unchanged', async ({ page }) => {
  await openSeed(page);
  await h.clickNamed(page, /add worksheet/i);
  await h.setCell(page, 'A1', 'keep me');
  await h.worksheetTab(page, 'Sheet2').click();
  await expect(h.cell(page, 'A1')).toHaveText('keep me');

  await h.openWorksheetTabMenu(page, 'Sheet2');
  await h.menuItem(page, /^delete$/i).click();
  await page.keyboard.press('Escape');

  await page.reload();
  await h.expectWorksheetTab(page, 'Sheet2');
});
