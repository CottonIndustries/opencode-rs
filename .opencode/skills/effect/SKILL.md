---
name: effect
description: Work with Effect v4 TypeScript code in the opencode-rs Deno plugin host
---

# Effect

The plugin host (`src/plugin/host.ts`) uses Effect v4 for typed, composable services, schemas and workflows. The Rust core is the source of truth for session and tool behavior; Effect is the bridge language for plugin code.

## Source of truth

Use the current Effect v4 source and the installed `effect@4.1.0` types, not memory or older Effect v2/v3 examples. Check the package's generated `.d.ts` files or the reference tree when an API is unfamiliar.

## Services and layers

- Read the existing host patterns before adding a service. `src/plugin/host.ts` detects Effect values with `isEffect` and runs them at the bridge boundary with `Effect.runPromise`.
- Effect plugins may be given a `Scope.Scope` while activating. Keep the scope on the plugin record and close it with `Scope.close` on unload, as the host does.
- Keep dependency provisioning explicit; do not hide it in module-level mutable state. When an effect needs a service, provide it where the effect is run.
- Run effects at the boundary, not inside code that should stay composable.

## Schemas

Use `Schema` for data that crosses the host boundary: plugin options, hook payloads, stored state and RPC results. The host uses `Schema.isSchema` and `Schema.toJsonSchemaDocument` when converting tool schemas, so keep domain types simple and JSON-serializable. Encode/decode at the edge.

## Streams and events

- Use `Stream` for event subscriptions and subscriptions that can end.
- Convert async iterables with `Stream.fromAsyncIterable` and run them with a scoped `Stream.runForEach`.
- Do not leak a stream fiber; tie it to the plugin scope so plugin unload closes subscriptions.

## Errors

- Use typed failures (`Effect.fail`, `Schema.TaggedErrorClass`) at boundaries that the Rust side must distinguish.
- Convert unknown throws with `Effect.try`/`Effect.tryPromise` and preserve the message.
- Never swallow a plugin error silently; log it with context and return a structured failure to Rust.

## Testing

- Test pure transformations directly.
- There is no shared Effect test runtime in this repository. If you add Deno tests, keep them independent and avoid sleeping to wait for a fiber.
- Keep a small in-memory service or stub instead of a real network or filesystem.

## Common pitfalls

- Calling `Effect.runPromise` inside a service loses the caller's context and clock.
- Forgetting to `yield*` an effect returns the effect object instead of its value.
- Mutating an object after handing it to another fiber can break Schema assumptions; clone boundary data.
- A Promise from a non-Effect library must be wrapped, not awaited in the middle of an `Effect.gen` without `yield* Effect.tryPromise`.
