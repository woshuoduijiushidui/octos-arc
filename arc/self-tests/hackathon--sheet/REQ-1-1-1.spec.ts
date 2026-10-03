import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-1-1-1
// fixtures: seed_workbook_q3_sales
//
// Every observable named here is quoted by the requirement: the home page lists
// each record with "Last updated: <value>" and a link whose accessible name is
// the workbook name, and the editor must show the same value plus the workbook's
// tabs, structure and grid for that workbook only.

test('REQ-1-1-1: the home page lists the seeded workbook with its last-updated value and an open link', async ({ page }) => {
  await h.openHome(page);
  await h.expectWorkbookListed(page, h.SEED.workbook);
  await h.expectLastUpdated(page);
});

test('REQ-1-1-1: opening the workbook shows the workbook name, worksheet tabs and the seeded grid', async ({ page }) => {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);

  await h.expectEditorFor(page, h.SEED.workbook);
  await h.expectLastUpdated(page);
  await h.expectWorksheetTab(page, h.SEED.worksheet);
  await h.expectCellText(page, 'A1', h.SEED.a1);
});

test('REQ-1-1-1: the editor entry is directly re-enterable and still identifies the same workbook after refresh', async ({ page }) => {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.expectCellText(page, 'A1', h.SEED.a1);

  const entry = page.url();
  await page.reload();
  await expect(page).toHaveURL(entry);
  await h.expectVisible(page, h.SEED.workbook);
  await h.expectCellText(page, 'A1', h.SEED.a1);
});
