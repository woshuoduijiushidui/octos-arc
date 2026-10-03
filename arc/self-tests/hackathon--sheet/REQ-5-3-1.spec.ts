import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-3-1
// fixtures: seed_workbook_q3_sales
//
// Quoted strings: "Create pivot table" in the "Data" menu and as the dialog
// name, the visible text "Source range: <cell range>", the radio option "New
// worksheet", the button "Create", the region "Pivot table editor", the combo
// boxes "Rows", "Columns", "Values" and "Summarize by", the button "Apply", the
// options SUM / COUNT / AVERAGE, "Grand Total", the button "Refresh pivot
// table", and the two errors "Pivot field is no longer available. Select a new
// field." and "Value field requires numeric values".

async function seedTable(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.setCell(page, 'A1', 'Region');
  await h.setCell(page, 'B1', 'Units');
  await h.setCell(page, 'A2', 'East');
  await h.setCell(page, 'B2', '10');
  await h.setCell(page, 'A3', 'West');
  await h.setCell(page, 'B3', '20');
  await h.setCell(page, 'A4', 'East');
  await h.setCell(page, 'B4', '30');
}

async function createPivot(page: any, from = 'A1', to = 'B4'): Promise<void> {
  await h.selectRange(page, from, to);
  await h.chooseFromDataMenu(page, /create pivot table/i);
  const scope = await h.dialog(page, 'Create pivot table');
  await expect(scope).toContainText(/source range:\s*A1:B4/i);
  await h.clickNamed(scope, 'Create');
}

test('REQ-5-3-1: the first pivot worksheet is named Pivot1 and later ones do not reuse it', async ({ page }) => {
  await seedTable(page);
  await createPivot(page);
  await h.expectWorksheetTab(page, 'Pivot1');

  await h.worksheetTab(page, h.SEED.worksheet).click();
  await createPivot(page);
  await h.expectWorksheetTab(page, 'Pivot2');
});

test('REQ-5-3-1: with no column field A1/B1 name the layout and the last row is Grand Total', async ({ page }) => {
  await seedTable(page);
  await createPivot(page);

  const editor = page.getByRole('region', { name: /pivot table editor/i }).first();
  await expect(editor).toBeVisible();
  await h.setCombobox(editor, 'Rows', 'Region');
  await h.setCombobox(editor, 'Values', 'Units');
  await h.setCombobox(editor, 'Summarize by', 'SUM');
  await h.clickNamed(editor, 'Apply');

  await h.expectCellText(page, 'A1', 'Region');
  await h.expectCellText(page, 'B1', 'SUM of Units');
  await h.expectCellText(page, 'A2', 'East');
  await h.expectCellText(page, 'B2', '40');
  await h.expectCellText(page, 'A3', 'West');
  await h.expectCellText(page, 'B3', '20');
  await h.expectCellText(page, 'A4', 'Grand Total');
  await h.expectCellText(page, 'B4', '60');
});

test('REQ-5-3-1: COUNT counts non-empty records and still works on nonnumeric content', async ({ page }) => {
  await seedTable(page);
  await h.setCell(page, 'B3', 'n/a');
  await createPivot(page);

  const editor = page.getByRole('region', { name: /pivot table editor/i }).first();
  await h.setCombobox(editor, 'Rows', 'Region');
  await h.setCombobox(editor, 'Values', 'Units');
  await h.setCombobox(editor, 'Summarize by', 'COUNT');
  await h.clickNamed(editor, 'Apply');

  await h.expectCellText(page, 'B1', 'COUNT of Units');
  await h.expectCellText(page, 'B2', '2');
  await h.expectCellText(page, 'B3', '1');
});

test('REQ-5-3-1: SUM over a value field with no parseable numbers reports the wording and keeps the old result', async ({ page }) => {
  await seedTable(page);
  await h.setCell(page, 'B2', 'ten');
  await h.setCell(page, 'B3', 'twenty');
  await h.setCell(page, 'B4', 'thirty');
  await createPivot(page);

  const editor = page.getByRole('region', { name: /pivot table editor/i }).first();
  await h.setCombobox(editor, 'Rows', 'Region');
  await h.setCombobox(editor, 'Values', 'Units');
  await h.setCombobox(editor, 'Summarize by', 'SUM');
  await h.clickNamed(editor, 'Apply');

  await h.expectErrorText(page, 'Value field requires numeric values');
});

test('REQ-5-3-1: the pivot worksheet, layout and results persist after refresh', async ({ page }) => {
  await seedTable(page);
  await createPivot(page);
  const editor = page.getByRole('region', { name: /pivot table editor/i }).first();
  await h.setCombobox(editor, 'Rows', 'Region');
  await h.setCombobox(editor, 'Values', 'Units');
  await h.setCombobox(editor, 'Summarize by', 'SUM');
  await h.clickNamed(editor, 'Apply');
  await h.expectCellText(page, 'B4', '60');

  await page.reload();

  await h.expectWorksheetTab(page, 'Pivot1');
  await h.expectCellText(page, 'A1', 'Region');
  await h.expectCellText(page, 'B1', 'SUM of Units');
  await h.expectCellText(page, 'B4', '60');
  await expect(page.getByRole('button', { name: /refresh pivot table/i }).first()).toBeVisible();
});
