# RLM manual-model eligibility

## Goal

An RLM child can use any model that the current session can select manually. `rlm.find_models()` and explicit `rlm(..., model=...)` selection use the same availability rule as the interactive model selector.

## Current behavior

Manual model selection uses `ModelRegistry.refreshAvailableModels()`. RLM uses `ModelRegistry.getExecutableModels()`, which applies an additional `openai-codex` account-catalog request and exact model-ID intersection. The two paths can disagree: a configured Codex model can be selected manually but omitted from RLM discovery and rejected for an explicit child.

The account-catalog check has an additional failure mode. An unavailable catalog endpoint, an expired cache, or a catalog ID mismatch blocks explicit non-parent Codex children even when the provider can stream requests for that model.

## Design

`AgentSession._authenticatedRlmModels()` will obtain its catalog from `ModelRegistry.refreshAvailableModels()`. It will retain the existing stale-auth status filter. This matches the refreshed catalog used for manual model cycling and selection.

Remove `ModelRegistry.getExecutableModels()` and its Codex-specific catalog request, token account-ID parsing, cache, and helper functions. No other production code calls this method. RLM keeps the existing `getApiKeyAndHeaders()` preflight before child creation, so a model with unavailable request credentials still fails before a child starts.

## Tests

Replace the Codex account-catalog intersection, discovery-failure, and cache-race regressions with a test that configures two authenticated Codex models, makes the catalog endpoint unavailable or return a mismatched catalog, and verifies that both models remain available to `findRlmModels()`. Verify that an explicitly selected manually available model starts an RLM child.

Keep the existing tests for stale credentials, selector validation, and request-auth preflight.

## Scope

This changes only RLM model eligibility. It does not change manual model selection, provider request behavior, generated model metadata, or the model selector API.
