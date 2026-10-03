import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-2-2
// fixtures: seed_workbook_q3_sales
//
// Quoted strings: the column-header menu items "Insert 1 column left", "Insert 1
// column right" and "Delete column". Fixed behaviour: data in the target column
// and later columns shift right on insertion and left on deletion, formulas
// adjust, and a direct reference that cannot be preserved displays #REF!.

async function seedColumns(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.setCell(page, 'B1', 'left');
  await h.setCell(page, 'C1', 'right');
}

test('REQ-2-2-2: the column menu offers the three column commands', async ({ page }) => {
  await seedColumns(page);
  await h.openColumnMenu(page, 'B');

  await expect(h.menuItem(page, 'Insert 1 column left')).toBeVisible();
  await expect(h.menuItem(page, 'Insert 1 column right')).toBeVisible();
  await expect(h.menuItem(page, 'Delete column')).toBeVisible();
});

test('REQ-2-2-2: inserting to the left shifts the target column and everything right of it', async ({ page }) => {
  await seedColumns(page);
  await h.openColumnMenu(page, 'B');
  await h.menuItem(page, 'Insert 1 column left').click();

  await h.expectCellText(page, 'C1', 'left');
  await h.expectCellText(page, 'D1', 'right');
  await expect(h.cell(page, 'B1')).toHaveText(/^\s*$/);

  await page.reload();
  await h.expectCellText(page, 'C1', 'left');
});

test('REQ-2-2-2: deleting a column shifts later columns left, preserves their data and adjusts formulas', async ({ page }) => {
  await seedColumns(page);
  // A formula that lives outside the deleted column but references it directly.
  await h.setCell(page, 'D1', '=B1');
  await h.expectCellText(page, 'D1', 'left');

  await h.openColumnMenu(page, 'B');
  await h.menuItem(page, 'Delete column').click();

  // C1 ('right') shifts into B1; the =B1 reference cannot be preserved and has
  // to say so rather than silently retarget at the shifted neighbour.
  await h.expectCellText(page, 'B1', 'right');
  await h.expectCellText(page, 'C1', '#REF!');

  await page.reload();
  await h.expectCellText(page, 'B1', 'right');
  await h.expectCellText(page, 'C1', '#REF!');
});
