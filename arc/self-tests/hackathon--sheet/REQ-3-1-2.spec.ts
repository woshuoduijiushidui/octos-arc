import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-3-1-2
// fixtures: seed_workbook_q3_sales
//
// Fixed behaviour: pasting tab-separated columns and newline-separated rows
// applies the whole rectangle, preserves empty fields, overwrites only the
// target rectangle and leaves everything else alone. Quoted: the grid context
// menu command with the ARIA menuitem role named "Paste", and Ctrl+V for the
// same external clipboard content.

const TABLE = ['alpha\t1\t', 'beta\t2\tgamma'].join('\n');

async function seed(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
}

test('REQ-3-1-2: Ctrl+V applies the whole rectangle, empty fields included', async ({ page }) => {
  await seed(page);
  await h.selectCell(page, 'A2');
  await h.pasteExternalText(page, TABLE);

  await h.expectCellText(page, 'A2', 'alpha');
  await h.expectCellText(page, 'B2', '1');
  await expect(h.cell(page, 'C2')).toHaveText(/^\s*$/);
  await h.expectCellText(page, 'A3', 'beta');
  await h.expectCellText(page, 'B3', '2');
  await h.expectCellText(page, 'C3', 'gamma');
});

test('REQ-3-1-2: the grid context menu exposes a menuitem named Paste', async ({ page }) => {
  await seed(page);
  await h.selectCell(page, 'A2');
  await h.cell(page, 'A2').click({ button: 'right' });

  await expect(h.menuItem(page, 'Paste')).toBeVisible();
});

test('REQ-3-1-2: only the target rectangle is overwritten', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'D2', 'outside');
  await h.selectCell(page, 'A2');
  await h.pasteExternalText(page, TABLE);

  await h.expectCellText(page, 'D2', 'outside');
  await h.expectCellText(page, 'A1', h.SEED.a1);
});

test('REQ-3-1-2: the pasted rectangle persists after refresh', async ({ page }) => {
  await seed(page);
  await h.selectCell(page, 'A2');
  await h.pasteExternalText(page, TABLE);

  await page.reload();
  await h.expectCellText(page, 'A2', 'alpha');
  await h.expectCellText(page, 'C3', 'gamma');
});
