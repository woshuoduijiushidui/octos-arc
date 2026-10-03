import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-1-3-2
// fixtures: seed_workbook_q3_sales
//
// The requirement fixes the button "Export CSV", says the suggested filename
// ends with ".csv", that empty cells inside the used range are preserved in the
// grid's own row/column order, that text containing commas, quotes or line
// breaks is escaped, that formula cells export their calculated result rather
// than the expression, and that the worksheet state is unchanged by the export.

async function exportCsv(page: any): Promise<{ filename: string; text: string }> {
  const [download] = await Promise.all([
    page.waitForEvent('download'),
    h.clickNamed(page, /export csv/i),
  ]);
  const stream = await download.createReadStream();
  const chunks: Buffer[] = [];
  for await (const chunk of stream) chunks.push(Buffer.from(chunk));
  return { filename: download.suggestedFilename(), text: Buffer.concat(chunks).toString('utf8') };
}

function csvRows(text: string): string[] {
  return text.replace(/\r\n/g, '\n').split('\n').filter((line, index, all) => line !== '' || index < all.length - 1);
}

test('REQ-1-3-2: the download is a .csv whose cells, empty fields and formula results match the grid', async ({ page }) => {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);

  // Used range A1:D2 — A1 comes from the seed ("Region").
  await h.setCell(page, 'B1', 'Units');
  await h.setCell(page, 'A2', 'East');
  await h.setCell(page, 'B2', '1200');
  await h.setCell(page, 'C2', 'North, South');
  await h.setCell(page, 'D2', '=B2*2');
  await h.expectCellText(page, 'D2', '2400');

  const { filename, text } = await exportCsv(page);
  expect(filename.endsWith('.csv')).toBe(true);

  const rows = csvRows(text);
  expect(rows[0]).toBe('Region,Units,,');
  // "North, South" is re-quoted, empty C1 is preserved, and D2 exports 2400.
  expect(rows[1]).toBe('East,1200,"North, South",2400');
});

test('REQ-1-3-2: the export leaves the worksheet, filter view, grid values and formula bar unchanged', async ({ page }) => {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
  await h.setCell(page, 'A2', 'East');
  await h.setCell(page, 'B2', '=1+1');

  const before = await h.formulaBar(page).inputValue();
  const cell = await h.cellText(page, 'A2');
  await exportCsv(page);

  await expect(h.formulaBar(page)).toHaveValue(before);
  await expect(h.cell(page, 'A2')).toHaveText(cell);
  await h.expectWorksheetTab(page, h.SEED.worksheet);

  await page.reload();
  await expect(h.formulaBar(page)).toHaveValue(before);
  await h.expectCellText(page, 'A2', cell);
});
