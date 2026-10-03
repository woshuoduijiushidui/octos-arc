import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-1-1
// fixtures: seed_workbook_q3_sales
//
// Quoted strings: the "Data" menu, the dialog "Sort range", the combo boxes
// "Sort by" and "Order", the checkbox "Data has header row", the button "Sort",
// and the options "Ascending" / "Descending". A declared header row does not
// participate in sorting, whole records move together, and the order persists
// after refresh.

async function seedTable(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.setCell(page, 'A1', 'Name');
  await h.setCell(page, 'B1', 'Score');
  await h.setCell(page, 'A2', 'Bob');
  await h.setCell(page, 'B2', '30');
  await h.setCell(page, 'A3', 'Alice');
  await h.setCell(page, 'B3', '10');
  await h.setCell(page, 'A4', 'Carl');
  await h.setCell(page, 'B4', '20');
}

async function sortRange(page: any, column: string, order: string, from = 'A1', to = 'B4'): Promise<void> {
  await h.selectRange(page, from, to);
  await h.chooseFromDataMenu(page, /sort range/i);
  await h.expectDialogVisible(page, 'Sort range');
  await h.checkNamed(page, 'Data has header row');
  await h.setCombobox(page, 'Sort by', column);
  await h.setCombobox(page, 'Order', order);
  await h.clickNamed(page, 'Sort');
}

test('REQ-5-1-1: the sort dialog offers the named controls and order options', async ({ page }) => {
  await seedTable(page);
  await h.selectRange(page, 'A1', 'B4');
  await h.chooseFromDataMenu(page, /sort range/i);
  await h.expectDialogVisible(page, 'Sort range');

  await expect(page.getByRole('combobox', { name: /sort by/i }).first()).toBeVisible();
  await expect(page.getByRole('checkbox', { name: /data has header row/i }).first()).toBeVisible();

  await h.setCombobox(page, 'Order', 'Descending');
  await expect(page.getByRole('option', { name: /^ascending$/i })).toHaveCount(0);
});
test('REQ-5-1-1: ascending by a column keeps the header in place and moves whole records together', async ({ page }) => {
  await seedTable(page);
  await sortRange(page, 'Score', 'Ascending');

  await h.expectCellText(page, 'A1', 'Name');
  await h.expectCellText(page, 'B1', 'Score');
  await h.expectCellText(page, 'A2', 'Alice');
  await h.expectCellText(page, 'B2', '10');
  await h.expectCellText(page, 'A3', 'Carl');
  await h.expectCellText(page, 'B3', '20');
  await h.expectCellText(page, 'A4', 'Bob');
  await h.expectCellText(page, 'B4', '30');
});

test('REQ-5-1-1: descending reverses the records', async ({ page }) => {
  await seedTable(page);
  await sortRange(page, 'Score', 'Descending');

  await h.expectCellText(page, 'A2', 'Bob');
  await h.expectCellText(page, 'A4', 'Alice');
});

test('REQ-5-1-1: the sorted order and results persist after refresh', async ({ page }) => {
  await seedTable(page);
  await sortRange(page, 'Name', 'Ascending');

  await page.reload();
  await h.expectCellText(page, 'A2', 'Alice');
  await h.expectCellText(page, 'A4', 'Carl');
  await h.expectCellText(page, 'B2', '10');
});
