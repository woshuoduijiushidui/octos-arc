import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-1-3-1
// fixtures: import_csv
//
// The requirement names the button "Import CSV", the dialog "Import CSV", the
// file control "CSV file" and the button "Confirm import", fixes the new
// workbook's name as the file name with its final .csv extension removed, and
// fixes the only rejection wording: "Invalid CSV file format. Import failed."
//
// Cloud `986f71047724` failed here: the shipped `parseCSV` threw that rejection
// for well-formed quoted input, so a valid import never landed.

const WORKBOOK = 'sales-report';

const WITH_QUOTES_AND_EMPTY_FIELDS = [
  'Region,Units,Note',
  'East,1200,"North, South"',
  '中文,,"普通, 文本"',
  'West,800,"He said ""hi"""',
  '',
].join('\n');

test('REQ-1-3-1: a quoted, mixed-language CSV imports as a workbook named after the file, in original order', async ({ page }) => {
  await h.openHome(page);
  await h.importCsv(page, { name: `${WORKBOOK}.csv`, content: WITH_QUOTES_AND_EMPTY_FIELDS });

  // Sheet1 opens with the complete rows, columns and original text; the first
  // row stays ordinary data (A1 is "Region", not a header the grid drops).
  await h.expectWorksheetTab(page, 'Sheet1');
  await h.expectCellText(page, 'A1', 'Region');
  await h.expectCellText(page, 'B1', 'Units');
  await h.expectCellText(page, 'C1', 'Note');

  await h.expectCellText(page, 'A2', 'East');
  await h.expectCellText(page, 'B2', '1200');
  await h.expectCellText(page, 'C2', 'North, South');

  // An empty field inside a row is preserved, and UTF-8 text survives.
  await h.expectCellText(page, 'A3', '中 文');
  await expect(h.cell(page, 'B3')).toHaveText(/^\s*$/);
  await h.expectCellText(page, 'C3', '普通, 文本');

  // An escaped pair of double quotes arrives as one double quote.
  await h.expectCellText(page, 'C4', 'He said "hi"');
});

test('REQ-1-3-1: a quoted field containing a line break keeps the row and column order', async ({ page }) => {
  await h.openHome(page);
  await h.importCsv(page, { name: 'linebreaks.csv', content: 'Region,Note\nEast,"line one\nline two"\n' });

  await h.expectCellText(page, 'A1', 'Region');
  await h.expectCellText(page, 'A2', 'East');
  await expect(h.cell(page, 'B2')).toHaveText(/line one[\s\S]*line two/);
});

test('REQ-1-3-1: imported content survives closing and reopening the workbook', async ({ page }) => {
  await h.openHome(page);
  await h.importCsv(page, { name: `${WORKBOOK}.csv`, content: WITH_QUOTES_AND_EMPTY_FIELDS });
  await h.expectCellText(page, 'A2', 'East');

  const entry = page.url();
  await page.goto(entry);
  await h.expectCellText(page, 'A2', 'East');
  await h.expectCellText(page, 'C4', 'He said "hi"');
});

test('REQ-1-3-1: an unclosed leading quote is rejected and leaves no partial workbook', async ({ page }) => {
  await h.openHome(page);
  await h.importCsv(page, { name: 'broken.csv', content: 'Region,Note\nEast,"never closed\n' });

  await h.expectErrorText(page, 'Invalid CSV file format. Import failed.');

  await h.openHome(page);
  await h.expectTextAbsent(page, 'broken');
});
