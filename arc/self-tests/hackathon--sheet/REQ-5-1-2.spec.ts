import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-1-2
// fixtures: seed_workbook_q3_sales
//
// Quoted strings: "Create filter" and "Clear filter" in the "Data" menu; a
// per-header button named "Filter <header text>"; a dialog with the same name;
// "Clear selection", "Apply", the combo box "Condition", the text box "Value",
// and the condition options "Text contains", "Greater than", "Before", "Is
// empty", "Is not empty". Nonmatching rows are hidden only; conditions on
// different columns are combined with AND.

async function seedTable(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.setCell(page, 'A1', 'Region');
  await h.setCell(page, 'B1', 'Units');
  await h.setCell(page, 'A2', 'East');
  await h.setCell(page, 'B2', '1200');
  await h.setCell(page, 'A3', 'West');
  await h.setCell(page, 'B3', '800');
  await h.setCell(page, 'A4', 'East');
  await h.setCell(page, 'B4', '400');
  await h.selectRange(page, 'A1', 'B4');
  await h.chooseFromDataMenu(page, /create filter/i);
}

async function openHeaderFilter(page: any, header: string): Promise<any> {
  await page.getByRole('button', { name: new RegExp(`filter\\s+${header}`, 'i') }).first().click();
  return h.dialog(page, new RegExp(`filter\\s+${header}`, 'i'));
}

test('REQ-5-1-2: creating a filter adds a named button to each header', async ({ page }) => {
  await seedTable(page);
  await expect(page.getByRole('button', { name: /filter\s+region/i }).first()).toBeVisible();
  await expect(page.getByRole('button', { name: /filter\s+units/i }).first()).toBeVisible();
});

test('REQ-5-1-2: a value filter hides nonmatching rows without deleting or reordering them', async ({ page }) => {
  await seedTable(page);
  const scope = await openHeaderFilter(page, 'Region');
  await h.clickNamed(scope, 'Clear selection');
  await h.checkNamed(scope, 'East');
  await h.clickNamed(scope, 'Apply');

  await expect(h.cell(page, 'A2')).toBeVisible();
  await expect(h.cell(page, 'A4')).toBeVisible();
  // The West record is hidden, not removed.
  await expect(h.cell(page, 'A3')).toBeHidden();
});

test('REQ-5-1-2: a condition filter combines with AND across columns and persists after refresh', async ({ page }) => {
  await seedTable(page);
  const region = await openHeaderFilter(page, 'Region');
  await h.clickNamed(region, 'Clear selection');
  await h.checkNamed(region, 'East');
  await h.clickNamed(region, 'Apply');

  const units = await openHeaderFilter(page, 'Units');
  await h.setCombobox(units, 'Condition', 'Greater than');
  await h.fillField(units, /^value$/i, '500');
  await h.clickNamed(units, 'Apply');

  await expect(h.cell(page, 'A2')).toBeVisible();
  await expect(h.cell(page, 'A3')).toBeHidden();
  await expect(h.cell(page, 'A4')).toBeHidden();

  await page.reload();
  await expect(h.cell(page, 'A2')).toBeVisible();
  await expect(h.cell(page, 'A4')).toBeHidden();
});

test('REQ-5-1-2: Clear filter restores every source record in its original order', async ({ page }) => {
  await seedTable(page);
  const scope = await openHeaderFilter(page, 'Region');
  await h.clickNamed(scope, 'Clear selection');
  await h.checkNamed(scope, 'East');
  await h.clickNamed(scope, 'Apply');
  await expect(h.cell(page, 'A3')).toBeHidden();

  await h.chooseFromDataMenu(page, /clear filter/i);

  await expect(h.cell(page, 'A2')).toHaveText('East');
  await expect(h.cell(page, 'A3')).toHaveText('West');
  await expect(h.cell(page, 'A4')).toHaveText('East');
  await page.reload();
  await expect(h.cell(page, 'A3')).toBeVisible();
});
