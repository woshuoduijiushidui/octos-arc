import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-4-2-2
// fixtures: seed_workbook_q3_sales
//
// The requirement fixes the visible error values: division by zero #DIV/0!, an
// invalid reference #REF!, an unsupported function #NAME?, a malformed
// expression #ERROR!, and a direct or indirect circular reference #REF!. When an
// error cell is selected the formula bar shows the original formula submitted,
// both survive refresh, and changing the cell to a valid formula recovers.

async function seed(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
}

test('REQ-4-2-2: each error class shows its own stable visible value', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', '4');
  await h.setCell(page, 'B2', '0');
  await h.setCell(page, 'C2', '=A2/B2');
  await h.setCell(page, 'C3', '=NOSUCHFN(A2)');
  await h.setCell(page, 'C4', '=1+');
  await h.setCell(page, 'C5', '=C5');

  await h.expectCellText(page, 'C2', '#DIV/0!');
  await h.expectCellText(page, 'C3', '#NAME?');
  await h.expectCellText(page, 'C4', '#ERROR!');
  await h.expectCellText(page, 'C5', '#REF!');
});

test('REQ-4-2-2: selecting an error cell shows the original formula, and both survive refresh', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'B2', '0');
  await h.setCell(page, 'C2', '=A2/B2');

  await h.selectCell(page, 'C2');
  await h.expectFormulaBar(page, '=A2/B2');

  await page.reload();
  await h.expectCellText(page, 'C2', '#DIV/0!');
  await h.selectCell(page, 'C2');
  await h.expectFormulaBar(page, '=A2/B2');
});

test('REQ-4-2-2: an error cell does not block viewing or editing other cells', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'B2', '0');
  await h.setCell(page, 'C2', '=A2/B2');
  await h.setCell(page, 'D2', 'still editable');

  await h.expectCellText(page, 'D2', 'still editable');
  await h.expectCellText(page, 'A1', h.SEED.a1);
});

test('REQ-4-2-2: replacing the source of the error recovers the cell and its dependents', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', '8');
  await h.setCell(page, 'B2', '0');
  await h.setCell(page, 'C2', '=A2/B2');
  await h.expectCellText(page, 'C2', '#DIV/0!');

  await h.setCell(page, 'B2', '2');

  await h.expectCellText(page, 'C2', '4');
  await h.selectCell(page, 'C2');
  await h.expectFormulaBar(page, '=A2/B2');

  await page.reload();
  await h.expectCellText(page, 'C2', '4');
});
