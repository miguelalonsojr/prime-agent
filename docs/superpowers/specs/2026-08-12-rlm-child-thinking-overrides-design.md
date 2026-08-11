# RLM child thinking overrides

## Goal

Let an RLM child use the thinking level configured for its installed role while preserving the current inheritance behavior for ordinary `rlm()` calls.

## API

`rlm()` accepts an optional `thinking` keyword in addition to `name` and `model`:

```python
await rlm(task, name="reviewer", model="openai/gpt-5.6-terra", thinking="high")
```

The accepted values are the existing Prime Agent thinking levels: `off`, `minimal`, `low`, `medium`, `high`, and `xhigh`. The keyword is optional.

## Child-session behavior

When `thinking` is absent, a child starts with its parent's current thinking level. This preserves existing callers and manual child dispatches.

When `thinking` is present, the child starts with that value. Prime Agent clamps the value to the selected child model's supported thinking levels using the same capability logic used for a model switch. The parent session remains unchanged.

Model selection remains independent. An explicit `model` selects the child model; omitting it selects the parent model.

## Installer behavior

The Prime installer uses each role's configured `thinking` value when generating the continual-harness subagent specification. The generated call includes the role name, model selector, and thinking level.

The provider-specific rendered `AGENTS.md` routing table shows role, model, and thinking level. Its Prime Agent instructions describe the optional override and inheritance behavior. This gives the parent a complete provider-specific dispatch mapping even when the compact harness roster omits a role.

## Validation and tests

Prime Agent tests cover omitted-thinking inheritance, explicit override, invalid thinking values and types, and clamping for a child model without the requested capability.

Installer tests cover generated harness specs and rendered instructions for Anthropic and OpenAI. Existing tests continue to verify each provider's role-to-model ladder.

## Scope

This changes only RLM child spawning and Prime Agent installation output. It does not change parent-session effort, model discovery, service tier, or non-Prime harnesses.
