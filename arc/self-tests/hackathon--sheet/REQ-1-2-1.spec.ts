import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-1-2-1
// fixtures: create_blank_workbook
//
// The requirement fixes both accessible names ("New blank workbook", "Create")
// and the post-condition (a single blank worksheet named Sheet1, Sheet1 active,
// A1 selected), and says refreshing or reopening reproduces it.

const CREATED = 'Blank workbook check';

async function createBlankWorkbook(page: any, name: string): Promise<void> {
  await h.openHome(page);
  await h.clickNamed(page, /new blank workbook/i);
  await h.fillField(page, /workbook name|name/i, name);
  await h.clickNamed(page, /^create$/i);
}

test('REQ-1-2-1: creating a blank workbook opens the editor with a single blank Sheet1 and A1 selected', async ({ page }) => {
  await createBlankWorkbook(page, CREATED);

  await h.expectWorksheetTab(page, 'Sheet1');
  await expect(page.getByRole('tab')).toHaveCount(1);
  await expect(h.worksheetTab(page, 'Sheet1')).toHaveAttribute('aria-selected', 'true');

  // A1 selected and blank: the requirement says "only a blank worksheet".
  await expect(h.formulaBar(page)).toHaveValue('');
});

test('REQ-1-2-1: the created workbook survives refresh and reopening from the home page', async ({ page }) => {
  await createBlankWorkbook(page, CREATED);

  await page.reload();
  await expect(page.getByRole('tab')).toHaveCount(1);
  await expect(h.worksheetTab(page, 'Sheet1')).toHaveAttribute('aria-selected', 'true');

  await h.openHome(page);
  await h.expectWorkbookListed(page, CREATED);
  await h.openWorkbook(page, CREATED);
  await h.expectWorksheetTab(page, 'Sheet1');
  await expect(page.getByRole('tab')).toHaveCount(1);
});
