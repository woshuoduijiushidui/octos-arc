import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-4-2-1
// fixtures: seed_workbook_q3_sales
//
// Fixed behaviour: after a source-value edit (or paste / range move / structure
// change) every directly and indirectly dependent formula updates in dependency
// order; each formula bar keeps showing its original formula while the grid
// shows the new result; after refresh the results match the current source
// values and never the pre-change ones; formulas on other worksheets that do not
// reference these cells are unchanged.

async function seed(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
}

test('REQ-4-2-1: a source edit updates direct and indirect dependents in order', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', '1');
  await h.setCell(page, 'B2', '=A2+1');
  await h.setCell(page, 'C2', '=B2*2');
  await h.setCell(page, 'D2', '=C2+B2');
  await h.expectCellText(page, 'B2', '2');
  await h.expectCellText(page, 'C2', '4');
  await h.expectCellText(page, 'D2', '6');

  await h.setCell(page, 'A2', '10');

  await h.expectCellText(page, 'B2', '11');
  await h.expectCellText(page, 'C2', '22');
  await h.expectCellText(page, 'D2', '33');
});

test('REQ-4-2-1: each formula bar keeps its original formula after the recalculation', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', '1');
  await h.setCell(page, 'B2', '=A2+1');
  await h.setCell(page, 'A2', '5');

  await h.selectCell(page, 'B2');
  await h.expectFormulaBar(page, '=A2+1');
  await h.expectCellText(page, 'B2', '6');
});

test('REQ-4-2-1: refreshed results follow the current source values, never the old ones', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', '2');
  await h.setCell(page, 'B2', '=A2*5');
  await h.setCell(page, 'A2', '3');
  await h.expectCellText(page, 'B2', '15');

  await page.reload();
  await h.expectCellText(page, 'B2', '15');
  await expect(h.cell(page, 'B2')).not.toHaveText('10');
});

test('REQ-4-2-1: formulas on another worksheet that do not reference the edited cells are unchanged', async ({ page }) => {
  await seed(page);
  await h.clickNamed(page, /add worksheet/i);
  await h.setCell(page, 'A1', '3');
  await h.setCell(page, 'B1', '=A1+1');
  await h.expectCellText(page, 'B1', '4');

  await h.worksheetTab(page, h.SEED.worksheet).click();
  await h.setCell(page, 'A2', '99');
  await h.setCell(page, 'B2', '=A2+1');

  await h.worksheetTab(page, 'Sheet2').click();
  await h.expectCellText(page, 'B1', '4');
});
