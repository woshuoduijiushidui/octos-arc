import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-1-1
// fixtures: seed_workbook_q3_sales
//
// Quoted: the button "Add worksheet". Fixed behaviour: the new tab uses the
// first unused SheetN in positive-integer order (Sheet2 when only Sheet1
// exists), it is blank, it becomes active with A1 selected, existing worksheets
// keep their data, and the tab survives refresh.

async function openSeedWorkbook(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
}

test('REQ-2-1-1: adding a worksheet creates the next unused SheetN as the active blank tab', async ({ page }) => {
  await openSeedWorkbook(page);
  await h.clickNamed(page, /add worksheet/i);

  await h.expectWorksheetTab(page, 'Sheet2');
  await expect(h.worksheetTab(page, 'Sheet2')).toHaveAttribute('aria-selected', 'true');
  await expect(h.formulaBar(page)).toHaveValue('');
  await h.expectWorksheetTab(page, h.SEED.worksheet);
});

test('REQ-2-1-1: a second addition does not reuse a taken name', async ({ page }) => {
  await openSeedWorkbook(page);
  await h.clickNamed(page, /add worksheet/i);
  await h.clickNamed(page, /add worksheet/i);

  await h.expectWorksheetTab(page, 'Sheet3');
  await expect(h.worksheetTab(page, 'Sheet3')).toHaveAttribute('aria-selected', 'true');
});

test('REQ-2-1-1: the new worksheet is blank and leaves the existing worksheet unchanged', async ({ page }) => {
  await openSeedWorkbook(page);
  const before = await h.cellText(page, 'A1');
  await h.clickNamed(page, /add worksheet/i);
  await expect(h.formulaBar(page)).toHaveValue('');

  await h.worksheetTab(page, h.SEED.worksheet).click();
  await expect(h.cell(page, 'A1')).toHaveText(before);
});

test('REQ-2-1-1: the added tab still exists after refresh and reopening', async ({ page }) => {
  await openSeedWorkbook(page);
  await h.clickNamed(page, /add worksheet/i);
  await h.expectWorksheetTab(page, 'Sheet2');

  await page.reload();
  await h.expectWorksheetTab(page, 'Sheet2');

  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.expectWorksheetTab(page, 'Sheet2');
});
