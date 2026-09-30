import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-4-1-1
// fixtures: seed_workbook_q3_sales
//
// Fixed behaviour: numeric constants, parentheses, + - * /, A1-style references
// in the same worksheet, and SUM, AVERAGE, COUNT, MIN, MAX over contiguous
// ranges; function names are case-insensitive; aggregate functions ignore empty
// cells, COUNT counts only numeric cells, and SUM/AVERAGE/MIN/MAX use only
// numeric cells rather than treating blanks as zero.

async function seed(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  // B2:B5 are 2, 4, 6 and one empty cell; D2/D3 mix text with a number.
  await h.setCell(page, 'B2', '2');
  await h.setCell(page, 'B3', '4');
  await h.setCell(page, 'B4', '6');
  await h.setCell(page, 'D2', 'text');
  await h.setCell(page, 'D3', '5');
}

test('REQ-4-1-1: arithmetic, parentheses and same-worksheet references evaluate correctly', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'C7', '=(B2+B3)*B4');
  await h.expectCellText(page, 'C7', '36');
  await h.expectFormulaBar(page, '=(B2+B3)*B4');

  await h.setCell(page, 'C8', '=B4/B2+B3');
  await h.expectCellText(page, 'C8', '7');
});

test('REQ-4-1-1: SUM, AVERAGE, COUNT, MIN and MAX ignore the empty cell in the range', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'C10', '=SUM(B2:B5)');
  await h.setCell(page, 'C11', '=AVERAGE(B2:B5)');
  await h.setCell(page, 'C12', '=COUNT(B2:B5)');
  await h.setCell(page, 'C13', '=MIN(B2:B5)');
  await h.setCell(page, 'C14', '=MAX(B2:B5)');

  await h.expectCellText(page, 'C10', '12');
  await h.expectCellText(page, 'C11', '4');
  await h.expectCellText(page, 'C12', '3');
  await h.expectCellText(page, 'C13', '2');
  await h.expectCellText(page, 'C14', '6');
});

test('REQ-4-1-1: function names are case-insensitive and text is not counted as zero', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'C16', '=sum(b2:b4)');
  await h.expectCellText(page, 'C16', '12');

  await h.setCell(page, 'C17', '=SUM(D2:D3)');
  await h.expectCellText(page, 'C17', '5');
  await h.setCell(page, 'C18', '=COUNT(D2:D3)');
  await h.expectCellText(page, 'C18', '1');
});

test('REQ-4-1-1: results and original expressions persist after refresh', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'C20', '=SUM(B2:B5)');
  await h.expectCellText(page, 'C20', '12');

  await page.reload();
  await h.expectCellText(page, 'C20', '12');
  await h.selectCell(page, 'C20');
  await h.expectFormulaBar(page, '=SUM(B2:B5)');
});
