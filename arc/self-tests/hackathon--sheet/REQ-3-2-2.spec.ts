import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-3-2-2
// fixtures: seed_workbook_q3_sales
//
// Quoted: the toolbar buttons "Undo" and "Redo"; Ctrl+Z and Ctrl+Y do the same.
// Fixed behaviour: undo restores values, original formulas, structure, rule
// ranges and results from before the operation; consecutive undos go back in
// reverse order; redo reapplies the whole operation; the state after each undo
// or redo persists after refresh; a new modification after an undo disables Redo.

async function seed(page: any): Promise<void> {
  await h.openHome(page);
  await h.openWorkbook(page, h.SEED.workbook);
}

test('REQ-3-2-2: undo reverts a cell edit and redo reapplies it', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', 'before');
  await h.setCell(page, 'A2', 'after');
  await h.expectCellText(page, 'A2', 'after');

  await h.clickNamed(page, 'Undo');
  await h.expectCellText(page, 'A2', 'before');

  await h.clickNamed(page, 'Redo');
  await h.expectCellText(page, 'A2', 'after');
});

test('REQ-3-2-2: consecutive undos restore changes in reverse order', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', 'one');
  await h.setCell(page, 'A2', 'two');
  await h.setCell(page, 'A2', 'three');

  await h.clickNamed(page, 'Undo');
  await h.expectCellText(page, 'A2', 'two');
  await h.clickNamed(page, 'Undo');
  await h.expectCellText(page, 'A2', 'one');
});

test('REQ-3-2-2: Ctrl+Z and Ctrl+Y perform the same operations', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', 'value');
  await h.press(page, 'Control+Z');
  await expect(h.cell(page, 'A2')).toHaveText(/^\s*$/);
  await h.press(page, 'Control+Y');
  await h.expectCellText(page, 'A2', 'value');
});

test('REQ-3-2-2: the state after an undo persists after refresh', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', 'first');
  await h.setCell(page, 'A2', 'second');
  await h.clickNamed(page, 'Undo');
  await h.expectCellText(page, 'A2', 'first');

  // The history itself may be empty after reopening, but the undone *state* has to be
  // what is stored.
  await page.reload();
  await h.expectCellText(page, 'A2', 'first');
});

test('REQ-3-2-2: a modification after an undo disables Redo', async ({ page }) => {
  await seed(page);
  await h.setCell(page, 'A2', 'first');
  await h.clickNamed(page, 'Undo');
  await h.setCell(page, 'A2', 'new branch');

  await h.expectDisabled(page, 'Redo');
});
