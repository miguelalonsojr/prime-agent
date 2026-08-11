# RLM executable model discovery

## Goal

Make `rlm.find_models()` list every model that the authenticated provider authorizes and that Prime Agent can spawn. A listed selector must pass the same model-resolution path used by `rlm()`.

## Catalog rules

`ModelRegistry` remains the source of executable model metadata. Provider discovery remains the authorization source. For `openai-codex`, Prime Agent fetches the account catalog and returns only configured Codex models whose IDs occur in that catalog.

A provider model absent from the local registry is not returned and cannot be selected. Prime Agent does not synthesize model metadata from a provider slug.

## Failure behavior

A failed or invalid Codex catalog request must be observable to the caller. The model-discovery API returns a typed discovery failure instead of an indistinguishable empty model list. The error contains no credential material.

The existing short-lived successful catalog cache remains usable when a refresh fails. Without a usable cache, discovery reports the failure and `rlm()` rejects explicit non-parent Codex model selection for the same reason.

## API limits

`rlm.find_models()` keeps its bounded result contract. The limit applies after authorization and executable-model filtering. The maximum remains 20; callers that need all matching models request the maximum and narrow with a query.

## Validation

Tests cover a successful intersection, a provider catalog containing an unsupported local selector, a failed initial catalog lookup, and a failed refresh with a usable cache. Host-handler tests confirm the discovery failure reaches Python without exposing credentials. Existing selected-parent behavior remains unchanged.

## Scope

This changes RLM discovery and selected-child preflight for provider catalogs. It does not add dynamic model definitions, change model generation, or alter the role-model mapping installed by agentic-swe-setup.
