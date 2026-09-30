import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-2-1
// fixtures: seed_workbook_q3_sales
//
// Quoted strings: "Data validation" in the "Data" menu and as the dialog name,
// the combo box "Rule type", the text boxes "Allowed values", "Minimum" and
// "Maximum", the button "Save", the cell button "Open dropdown for <cell
// coordinate>", the rejection "Please enter a number between <minimum> and
// <maximum>", the persisted 0-to-100 wording "Please enter a number from 0 to
// 100", and the button "Delete rule".

async function seed(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
}

async function openValidation(page: any, from: string, to?: string): Promise<any> {
  await h.selectRange(page, from, to ?? from);
  await h.chooseFromDataMenu(page, /data validation/i);
  const scope = await h.dialog(page, 'Data validation');
  await expect(scope).toBeVisible();
  return scope;
}

test('REQ-5-2-1: a number range rule rejects an out-of-range grid entry and keeps the original', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'B3', '50');
  const scope = await openValidation(page, 'B3');
  await h.setCombobox(scope, 'Rule type', 'Number range');
  await h.fillField(scope, /^minimum$/i, '0');
  await h.fillField(scope, /^maximum$/i, '100');
  await h.clickNamed(scope, 'Save');

  await h.setCell(page, 'B3', '101');

  await h.expectErrorText(page, 'Please enter a number from 0 to 100');
  await h.expectCellText(page, 'B3', '50');
});

test('REQ-5-2-1: the rule stays active after refresh', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'B3', '50');
  const scope = await openValidation(page, 'B3');
  await h.setCombobox(scope, 'Rule type', 'Number range');
  await h.fillField(scope, /^minimum$/i, '0');
  await h.fillField(scope, /^maximum$/i, '100');
  await h.clickNamed(scope, 'Save');

  await page.reload();
  await h.setCell(page, 'B3', '101');
  await h.expectErrorText(page, 'Please enter a number from 0 to 100');
  await h.expectCellText(page, 'B3', '50');
});

test('REQ-5-2-1: a dropdown rule exposes a named opener whose options are the allowed values', async ({ page }) => {
  await seed(page);
  const scope = await openValidation(page, 'C5');
  await h.setCombobox(scope, 'Rule type', 'Dropdown');
  await h.fillField(scope, /allowed values/i, 'Red, Green , Blue');
  await h.clickNamed(scope, 'Save');

  await page.getByRole('button', { name: /open dropdown for\s*C5/i }).first().click();
  for (const value of ['Red', 'Green', 'Blue']) {
    await expect(page.getByRole('option', { name: new RegExp(`^${value}$`, 'i') }).first()).toBeVisible();
  }
  await page.getByRole('option', { name: /^Green$/i }).first().click();
  await h.expectCellText(page, 'C5', 'Green');
});

test('REQ-5-2-1: reopening the rule prefills it and offers Delete rule', async ({ page }) => {
  await seed(page);
  const scope = await openValidation(page, 'B3');
  await h.setCombobox(scope, 'Rule type', 'Number range');
  await h.fillField(scope, /^minimum$/i, '0');
  await h.fillField(scope, /^maximum$/i, '100');
  await h.clickNamed(scope, 'Save');

  const reopened = await openValidation(page, 'B3');
  await expect(reopened.getByRole('textbox', { name: /^minimum$/i }).first()).toHaveValue('0');
  await expect(reopened.getByRole('textbox', { name: /^maximum$/i }).first()).toHaveValue('100');
  await expect(reopened.getByRole('combobox', { name: /rule type/i }).first()).toBeVisible();
  await expect(reopened.getByRole('button', { name: /delete rule/i }).first()).toBeVisible();
});
