import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-3-1-1
// fixtures: seed_workbook_q3_sales
//
// Fixed behaviour: edit in the grid or the "Formula bar"; Enter or clicking
// another cell commits, Escape cancels; ordinary cells read back the same text
// in grid and bar while formula cells show the result in the grid and the
// original expression in the bar; dependents recalculate; values, original
// formulas and results persist after refresh.

async function seed(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
}

test('REQ-3-1-1: an ordinary value shows the same text in the grid and the formula bar', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'B2', 'plain text');

  await h.expectCellText(page, 'B2', 'plain text');
  await h.expectFormulaBar(page, 'plain text');
});

test('REQ-3-1-1: a formula shows its result in the grid and its original expression in the formula bar', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', '2');
  await h.setCell(page, 'B2', '3');
  await h.setCell(page, 'C2', '=A2+B2');

  await h.expectCellText(page, 'C2', '5');
  await h.expectFormulaBar(page, '=A2+B2');
});

test('REQ-3-1-1: pressing Enter commits and dependent formulas update immediately', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', '2');
  await h.setCell(page, 'B2', '3');
  await h.setCell(page, 'C2', '=A2*B2');
  await h.expectCellText(page, 'C2', '6');

  await h.setCell(page, 'A2', '4');
  await h.expectCellText(page, 'C2', '12');
});

test('REQ-3-1-1: Escape cancels an uncommitted change', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'B2', 'committed');
  await h.selectCell(page, 'B2');

  await h.formulaBar(page).fill('discarded');
  await h.press(page, 'Escape');

  await h.expectCellText(page, 'B2', 'committed');
  await h.expectFormulaBar(page, 'committed');
});

test('REQ-3-1-1: values, original formulas and results persist after refresh', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', '7');
  await h.setCell(page, 'B2', '=A2*3');
  await h.expectCellText(page, 'B2', '21');

  await page.reload();
  await h.expectCellText(page, 'A2', '7');
  await h.expectCellText(page, 'B2', '21');
  await h.selectCell(page, 'B2');
  await h.expectFormulaBar(page, '=A2*3');
});
