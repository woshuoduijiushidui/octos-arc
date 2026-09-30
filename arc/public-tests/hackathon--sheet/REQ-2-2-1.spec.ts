import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-2-2-1
// fixtures: seed_workbook_q3_sales
//
// Quoted strings: the row-number menu items "Insert 1 row above", "Insert 1 row
// below" and "Delete row", and the validation wording "Please enter a number
// from 0 to 100". Fixed behaviour: complete records and dependent formulas move
// together, deletions shift subsequent rows up, and a failed operation leaves
// the pre-operation structure intact.

async function seedRows(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  // A2:A4 = one record per row; B2 is a formula pointing at A2.
  await h.setCell(page, 'A2', 'first');
  await h.setCell(page, 'A3', 'second');
  await h.setCell(page, 'A4', 'third');
  await h.setCell(page, 'B4', '=A2');
}

test('REQ-2-2-1: the row menu offers the three row commands', async ({ page }) => {
  await seedRows(page);
  await h.openRowMenu(page, 2);

  await expect(h.menuItem(page, 'Insert 1 row above')).toBeVisible();
  await expect(h.menuItem(page, 'Insert 1 row below')).toBeVisible();
  await expect(h.menuItem(page, 'Delete row')).toBeVisible();
});

test('REQ-2-2-1: inserting above shifts the target row and the records below it down together', async ({ page }) => {
  await seedRows(page);
  await h.openRowMenu(page, 2);
  await h.menuItem(page, 'Insert 1 row above').click();

  await h.expectCellText(page, 'A3', 'first');
  await h.expectCellText(page, 'A4', 'second');
  await expect(h.cell(page, 'A2')).toHaveText(/^\s*$/);

  await page.reload();
  await h.expectCellText(page, 'A3', 'first');
});

test('REQ-2-2-1: deleting a row shifts subsequent rows up and keeps dependent formulas correct', async ({ page }) => {
  await seedRows(page);
  await h.openRowMenu(page, 2);
  await h.menuItem(page, 'Delete row').click();

  await h.expectCellText(page, 'A2', 'second');
  await h.expectCellText(page, 'A3', 'third');
  // The formula lived below the deleted row and moved with it: =A2 now reads the
  // record that shifted into A2.
  await h.expectCellText(page, 'B3', 'second');

  await page.reload();
  await h.expectCellText(page, 'A2', 'second');
});
