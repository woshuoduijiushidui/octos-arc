import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-4-1-2
// fixtures: seed_workbook_q3_sales
//
// Fixed behaviour: copying a formula cell within the same worksheet moves its
// relative references by the target offset while absolute references stay
// unchanged; the source stays as it was; the target grid shows the result of the
// new references; an offset that pushes a relative reference outside the
// worksheet shows =#REF! in the formula bar and #REF! in the grid.

async function seed(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.setCell(page, 'A2', '10');
  await h.setCell(page, 'A3', '20');
}

async function copyCell(page: any, from: string, to: string): Promise<void> {
  await h.selectCell(page, from);
  await h.press(page, 'Control+C');
  await h.selectCell(page, to);
  await h.press(page, 'Control+V');
}

test('REQ-4-1-2: a copied relative reference moves by the target offset', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'B2', '=A2*2');
  await h.expectCellText(page, 'B2', '20');

  await copyCell(page, 'B2', 'B3');

  await h.selectCell(page, 'B3');
  await h.expectFormulaBar(page, '=A3*2');
  await h.expectCellText(page, 'B3', '40');
  // The source formula and its result are unchanged by the copy.
  await h.selectCell(page, 'B2');
  await h.expectFormulaBar(page, '=A2*2');
  await h.expectCellText(page, 'B2', '20');
});

test('REQ-4-1-2: an absolute reference stays fixed when the formula is copied', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'C2', '=A2*$A$2');
  await h.expectCellText(page, 'C2', '100');

  await copyCell(page, 'C2', 'C3');

  await h.selectCell(page, 'C3');
  await h.expectFormulaBar(page, '=A3*$A$2');
  await h.expectCellText(page, 'C3', '200');
});

test('REQ-4-1-2: an offset that leaves the worksheet shows =#REF! and #REF!', async ({ page }) => {
  await seed(page);
  // =A2 copied one column to the left has a relative reference in column 0.
  await h.setCell(page, 'B2', '=A2');
  await h.expectCellText(page, 'B2', '10');

  await copyCell(page, 'B2', 'A2');

  await h.expectCellText(page, 'A2', '#REF!');
  await h.selectCell(page, 'A2');
  await h.expectFormulaBar(page, '=#REF!');
});

test('REQ-4-1-2: the adjusted formula and result persist after refresh', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'B2', '=A2*2');
  await copyCell(page, 'B2', 'B3');

  await page.reload();
  await h.selectCell(page, 'B3');
  await h.expectFormulaBar(page, '=A3*2');
  await h.expectCellText(page, 'B3', '40');
});
