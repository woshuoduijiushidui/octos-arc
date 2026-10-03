import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-1-2
// fixtures: seed_workbook_q3_sales
//
// Quoted: the text box "Formula bar". Fixed behaviour: clicking another ARIA tab
// switches grid/structure/selection/filter/validation/pivot state to that
// worksheet; a worksheet opened for the first time with no selection history
// selects A1; the target's formula bar shows either its ordinary value or the
// original formula; switching does not modify the source worksheet; reopening
// shows the last active tab.

test('REQ-2-1-2: clicking another tab switches the grid and the formula bar to that worksheet', async ({ page }) => {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.clickNamed(page, /add worksheet/i);

  // Sheet2 starts with no selection history -> A1 is selected and blank.
  await expect(h.formulaBar(page)).toHaveValue('');

  await h.setCell(page, 'A1', 'only on sheet two');
  await h.worksheetTab(page, h.SEED.worksheet).click();
  await expect(h.formulaBar(page)).toHaveValue(new RegExp(h.SEED.a1));

  await h.worksheetTab(page, 'Sheet2').click();
  await expect(h.formulaBar(page)).toHaveValue('only on sheet two');
});

test('REQ-2-1-2: switching away and back restores the source worksheet unchanged', async ({ page }) => {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  const original = await h.cellText(page, 'A1');

  await h.clickNamed(page, /add worksheet/i);
  await h.worksheetTab(page, h.SEED.worksheet).click();

  await expect(h.cell(page, 'A1')).toHaveText(original);
});

test('REQ-2-1-2: reopening the workbook shows the last active tab', async ({ page }) => {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.clickNamed(page, /add worksheet/i);
  await expect(h.worksheetTab(page, 'Sheet2')).toHaveAttribute('aria-selected', 'true');

  await page.reload();
  await expect(h.worksheetTab(page, 'Sheet2')).toHaveAttribute('aria-selected', 'true');
});
