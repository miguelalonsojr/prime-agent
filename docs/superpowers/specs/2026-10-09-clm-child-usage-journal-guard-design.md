# CLM journal guard for child-usage attribution

## Purpose and scope

A child owns its context and usage. The parent records attributed child usage in its session journal so the parent total remains durable across reloads. Accounting entries must not reject a parent CLM tool action. Model-facing entries and unknown journal writes must still trigger the existing conflict guard. The blocked Gambit session `01a11d4c-c50e-766e-bd4c-430a0b03501f` is outside the repair scope; its JSONL, writer marker, and context mirror remain unchanged.

## Failure

In the Gambit journal, the parent persisted an assistant tool call and CLM usage receipt, then appended `child_usage_attributed` while the tool ran. `LiveContextController` had captured the earlier journal leaf. `settleToolAction` rejected the changed leaf before importing the tool result. The controller retained its conflict and writer marker. Failed-provider cleanup then called `assertIdle`, which rejected the conflict and left model requests safety-locked. The journal sequence identifies the intervening accounting entry; it does not record the exact tool-preparation timestamp. Source inspection confirms that child attribution appends without the CLM tool-action fence.

## Journal boundary

Keep child usage in the durable parent journal. `LiveContextController` distinguishes an accounting-only extension from a conflicting session change. Given a captured leaf, the controller verifies that the leaf remains on the selected branch and that every entry after it, in order, has type `child_usage_attributed`. The check accepts zero or more such entries without changing the accepted CLM snapshot or audit watermark. It does not accept an unrelated custom entry, message, CLM boundary, branch switch, or missing captured leaf. The existing conflict and writer-marker behavior remains in place for those cases.

Apply this check after preparing the tool mirror, when settling the tool, and immediately before appending an edited CLM revision. The second settlement check matters because mirror reads are asynchronous. The controller must not treat the current leaf as a new trusted baseline without checking the intervening entries. No session format, daemon command, or public API changes are planned.

## Accounting and failure behavior

`SessionManager.appendChildUsageAttribution` continues to persist parent totals on the existing schedule. A long tool run does not defer child usage or extend the current crash-loss window. An accepted accounting suffix does not make an invalid context edit valid. An unexpected journal entry still fails closed before CLM import. The fix does not clear a writer marker or recover a controller after an actual conflict. It does not modify the already blocked session.

## Verification

Add a deterministic regression in the existing `live-context.test.ts` suite: append child usage after tool preparation, settle a CLM edit, reopen the session, and check the accepted revision and durable parent usage total. Cover accounting entries before preparation and during an awaited mirror read where practical. Add a negative case that appends a model-facing or unknown entry during the tool and confirms rejection with no accepted revision. Use the existing parent-child usage suite for integration coverage if the controller-level test cannot exercise the real attribution path. Tests must fail when the new guard is removed. Run each modified test file, `npm run check:test-policy`, and `npm run check` after code changes. Run a user-facing manual check in a fresh session without touching the blocked Gambit session.
