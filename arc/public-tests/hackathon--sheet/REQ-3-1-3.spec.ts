import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-3-1-3
// fixtures: seed_workbook_q3_sales
//
// The requirement fixes the ARIA contract itself: the grid exposes
// aria-multiselectable="true", every gridcell inside the rectangle exposes
// aria-selected="true" and every gridcell outside it "false". It also requires
// the whole rectangle (not just its top-left corner) to persist per worksheet.

async function seed(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
}

test('REQ-3-1-3: the grid is multiselectable and a dragged rectangle marks exactly its own cells', async ({ page }) => {
  await seed(page);
  await h.expectGridMultiselectable(page);

  await h.selectRange(page, 'A2', 'B3');

  for (const coord of ['A2', 'B2', 'A3', 'B3']) {
    await h.expectCellSelected(page, coord, true);
  }
  for (const coord of ['A1', 'C2', 'C3', 'A4', 'B4']) {
    await h.expectCellSelected(page, coord, false);
  }
});

test('REQ-3-1-3: a new selection replaces the previous one', async ({ page }) => {
  await seed(page);
  await h.selectRange(page, 'A2', 'B3');
  await h.selectRange(page, 'D5', 'E6');

  await h.expectCellSelected(page, 'D5', true);
  await h.expectCellSelected(page, 'E6', true);
  await h.expectCellSelected(page, 'A2', false);
});

test('REQ-3-1-3: the complete rectangle persists after refresh, not just its corner', async ({ page }) => {
  await seed(page);
  await h.selectRange(page, 'A2', 'B3');

  await page.reload();

  for (const coord of ['A2', 'B2', 'A3', 'B3']) {
    await h.expectCellSelected(page, coord, true);
  }
  await h.expectCellSelected(page, 'C3', false);
});

test('REQ-3-1-3: switching worksheets does not overwrite the other worksheet selection', async ({ page }) => {
  await seed(page);
  await h.selectRange(page, 'A2', 'B3');

  await h.clickNamed(page, /add worksheet/i);
  await h.selectRange(page, 'A1', 'A1');
  await h.worksheetTab(page, h.SEED.worksheet).click();

  for (const coord of ['A2', 'B2', 'A3', 'B3']) {
    await h.expectCellSelected(page, coord, true);
  }
});
