import { expect, test } from '@playwright/test';
import * as h from './helpers';

// requirement: REQ-5-1-2
// fixtures: issue_detail
//
// The seeded open issue title is `Improve onboarding` with description `Describe
// the onboarding improvement.`. The page shows the number, a heading whose exact
// accessible name is the complete title without the issue number, visible status
// text "Open" or "Closed", the complete saved description as readable text, the
// right-side metadata, and bottom sections containing "Comment" or "Activity".

async function openSeedIssue(page: any): Promise<void> {
  await page.goto(`/${h.SEED.owner.username}/${h.SEED.repo}`);
  await h.openIssues(page);
  await page.getByRole('link', { name: new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`) }).first().click();
}

test('REQ-5-1-2: the detail page shows the number, the exact title heading and the Open status', async ({ page }) => {
  await openSeedIssue(page);

  await h.expectHeading(page, new RegExp(`^\\s*${h.SEED.openIssue}\\s*$`, 'i'));
  await expect(page.getByText(/^open$/i).first()).toBeVisible();
  await expect(page.getByText(/#\d+|\bissue\s+\d+/i).first()).toBeVisible();
});

test('REQ-5-1-2: the complete saved description is readable text', async ({ page }) => {
  await openSeedIssue(page);
  await expect(page.getByText(h.SEED.openIssueBody, { exact: false }).first()).toBeVisible();
});

test('REQ-5-1-2: the right side shows assignees, labels and milestone and the bottom shows comment or activity sections', async ({ page }) => {
  await openSeedIssue(page);

  for (const area of [/assignees/i, /labels/i, /milestone/i]) {
    await expect(page.getByText(area).first()).toBeVisible();
  }
  await expect(page.getByText(/comment|activity/i).first()).toBeVisible();
});

test('REQ-5-1-2: a visitor can read the saved issue without sign-in', async ({ page }) => {
  await openSeedIssue(page);

  await expect(await h.signedInUsername(page)).toHaveCount(0);
  await expect(page.getByText(h.SEED.openIssueBody, { exact: false }).first()).toBeVisible();
});
