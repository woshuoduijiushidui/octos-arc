import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-3-2-1
// fixtures: seed_workbook_q3_sales
//
// Fixed behaviour: only same-worksheet range transfers; after copy the source is
// unchanged, after cut the source is cleared only once the target is displayed;
// values and formulas keep their two-dimensional layout and cells outside both
// ranges do not change. Ctrl+C / Ctrl+X / Ctrl+V are the standard shortcuts for
// the same operations.

async function seed(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.setCell(page, 'A2', 'one');
  await h.setCell(page, 'B2', 'two');
}

test('REQ-3-2-1: copying a range leaves the source unchanged and pastes the same layout', async ({ page }) => {
  await seed(page);
  await h.selectRange(page, 'A2', 'B2');
  await h.press(page, 'Control+C');

  await h.selectCell(page, 'A5');
  await h.press(page, 'Control+V');

  await h.expectCellText(page, 'A5', 'one');
  await h.expectCellText(page, 'B5', 'two');
  await h.expectCellText(page, 'A2', 'one');
  await h.expectCellText(page, 'B2', 'two');
});

test('REQ-3-2-1: cutting clears the source and moves the values to the target', async ({ page }) => {
  await seed(page);
  await h.selectRange(page, 'A2', 'B2');
  await h.press(page, 'Control+X');

  await h.selectCell(page, 'A5');
  await h.press(page, 'Control+V');

  await h.expectCellText(page, 'A5', 'one');
  await h.expectCellText(page, 'B5', 'two');
  await expect(h.cell(page, 'A2')).toHaveText(/^\s*$/);
  await expect(h.cell(page, 'B2')).toHaveText(/^\s*$/);
});

test('REQ-3-2-1: the transferred range persists after refresh', async ({ page }) => {
  await seed(page);
  await h.selectRange(page, 'A2', 'B2');
  await h.press(page, 'Control+C');
  await h.selectCell(page, 'A5');
  await h.press(page, 'Control+V');

  await page.reload();
  await h.expectCellText(page, 'A5', 'one');
  await h.expectCellText(page, 'B5', 'two');
});

test('REQ-3-2-1: cells outside the source and target ranges are untouched', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'D2', 'bystander');
  await h.selectRange(page, 'A2', 'B2');
  await h.press(page, 'Control+C');
  await h.selectCell(page, 'A5');
  await h.press(page, 'Control+V');

  await h.expectCellText(page, 'D2', 'bystander');
  await h.expectCellText(page, 'A1', h.SEED.a1);
});
