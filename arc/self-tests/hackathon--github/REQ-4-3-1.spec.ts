import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-4-3-1
// fixtures: branch_selector
//
// Quoted: the selector is a unique button named "Branch <current branch name>"; it
// opens a textbox named "Find branch" and selectable items with role option whose
// exact accessible names are the branch names. Seed: active branch `main`, target
// `feature-search` containing the file `main-only.md` that is absent from `main`,
// and an unknown search term matching nothing ("No matching branch"). Escape closes
// the selector and an unmatched search preserves the original active branch.

async function openBranchSelector(page: any): Promise<void> {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.repoNav(page, 'Code').click();
  await page.getByRole('button', { name: new RegExp(`^\\s*branch\\s+${h.SEED.branch}\\s*$`, 'i') }).first().click();
}

test('REQ-4-3-1: the Code page exposes a unique button named for the current branch', async ({ page }) => {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.repoNav(page, 'Code').click();

  await expect(page.getByRole('button', { name: new RegExp(`^\\s*branch\\s+${h.SEED.branch}\\s*$`, 'i') })).toHaveCount(1);
});

test('REQ-4-3-1: the selector opens a Find branch box listing branch names as options', async ({ page }) => {
  await openBranchSelector(page);

  await expect(h.textbox(page, 'Find branch')).toBeVisible();
  await expect(h.option(page, h.SEED.featureBranch)).toBeVisible();
});

test('REQ-4-3-1: selecting the target branch shows its branch-only file', async ({ page }) => {
  await openBranchSelector(page);
  await h.option(page, h.SEED.featureBranch).click();

  await expect(page.getByRole('button', { name: new RegExp(`^\\s*branch\\s+${h.SEED.featureBranch}\\s*$`, 'i') }).first()).toBeVisible();
  await expect(page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.featureOnlyFile.replace('.', '\\.')}\\s*$`) }).first()).toBeVisible();
});

test('REQ-4-3-1: an unmatched search reports No matching branch and Escape preserves the active branch', async ({ page }) => {
  await openBranchSelector(page);
  await h.fillField(page, /find branch/i, 'no-such-branch-name');

  await h.expectErrorText(page, 'No matching branch');
  await page.keyboard.press('Escape');
  await expect(page.getByRole('button', { name: new RegExp(`^\\s*branch\\s+${h.SEED.branch}\\s*$`, 'i') }).first()).toBeVisible();
  await page.reload();
  await expect(page.getByRole('button', { name: new RegExp(`^\\s*branch\\s+${h.SEED.branch}\\s*$`, 'i') }).first()).toBeVisible();
});
