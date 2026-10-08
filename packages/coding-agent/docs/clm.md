# Editable model context (CLM)

Context Language Model (CLM) mode lets the model edit a session-local working copy of older conversation messages. CLM is off by default. Set `clm.enabled: true` in global or project `settings.json` to default new sessions to CLM on. A project setting overrides the global setting. Reloading settings does not switch an active session. Use `/clm on` or `/clm off` while the session is idle to choose its mode. The session choice persists across restart.

```text
/clm on
Please shorten older context using the model tool and the CLM context.txt path in the prompt.
/clm status
/clm view
/clm off
```

The model must save an edit through one awaited tool action. It must keep protected request references intact. `/clm status` identifies the accepted epoch, revision, working file, estimated occupancy, and any known measured usage. `/clm view` reads the accepted model context. Pending file changes are not accepted until the tool action settles and validation succeeds. `/clm restore <epoch:revision>` makes a new revision from an accepted revision on the selected branch in the current epoch. Restore does not erase audit entries. Turning CLM off freezes the accepted context as a self-contained boundary. A later `/clm on` starts a new epoch from the current normal context. If the saved CLM-off boundary exceeds 1 MiB, the inactive daemon session list may omit the recap and verdict, even after a later status update. The audit remains available, and a valid session can still be reopened.

CLM keeps original message entries in the session audit. Removed text can remain in audit, older revisions, external files, and provider records. CLM does not provide secure deletion. An invalid edit leaves the last accepted revision in place; a corrupt or unavailable accepted view blocks model requests rather than reverting to audit history. Host-owned failed-provider cleanup removes only the known failed assistant entry from accepted model context. It does not delete audit or create a billed-usage receipt without an observed provider result.

While CLM is on, manual and automatic compaction, overflow compact-and-retry, historical branch navigation, `/fork`, and side questions are unavailable. Turn CLM off to use these operations against the frozen accepted context. CLM-on mode rejects `before_provider_request` extension payload mutations that it cannot safely re-budget. Detached or concurrent writers, direct human edits to the working file, and writes that outlive their awaited tool action are unsupported. A conflicted writer requires settled writer ownership and explicit repair before more model requests.

Persistent CLM commits require filesystem support for file and directory fsync. An unsupported filesystem rejects CLM before accepting a context mutation. An accepted revision is flushed before provider use. Usage receipts use ordinary session JSONL writes; there is no additional crash-safe flush for telemetry. If an attempt finishes but its receipt never reaches the file, its attribution is unknown. Older, missing, unfinished, or unrecognized receipts are unknown rather than zero billed usage. CLM status reports observed provider-only input, output, cache read/write, and cost for the current selected epoch and revision. It does not fold earlier revisions or child aggregate totals into that subtotal. Existing `/usage` audit totals remain unchanged in normal and off modes.

Older Prime Agent binaries may ignore CLM entries and replay audited messages into model context. Do not downgrade and resume a CLM session. Shorter projected context is not evidence of lower cost or better task quality. Compare billed cost, cache behavior, and successful outcomes on matched tasks before claiming savings.
