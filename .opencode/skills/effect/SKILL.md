---
name: effect
description: Work with Effect v4 TypeScript code in the opencode-rs Deno plugin host
---

# Effect

The plugin host (`src/plugin/host.ts`) uses Effect v4 for typed, composable services, schemas and workflows. The Rust core is the source of truth for session and tool behavior; Effect is the bridge language for plugin code.

## Source of truth

Use the current Effect v4 source and the installed `effect@4.1.0` types, not memory or older Effect v2/v3 examples. Check the package's generated `.d.ts` files or the reference tree when an API is unfamiliar.

## Services and layers

- Define a service with `Context.Service` and provide it with `Layer.effect`.
- Read dependencies with `yield* Service` inside `Effect.gen`.
- Keep the layer graph explicit; do not reach for module-level mutable state to avoid a dependency.
- Run effects at the boundary (`Effect.runPromise`/`runPromiseExit`), not inside a service that should stay composable.

```ts
class Registry extends Context.Service<Registry, RegistryShape>()("opencode/Registry") {}

const layer = Layer.effect(
  Registry,
  Effect.gen(function* () {
    const storage = yield* Storage
    return Registry.of({ get: (id) => storage.read(id) })
  }),
)
```

## Schemas

Use `Schema` for data that crosses the host boundary: plugin options, hook payloads, stored state and RPC results. Encode/decode at the edge and keep domain types simple. `Schema.Struct`, `Schema.Array`, `Schema.optional` and `Schema.Literals` cover most cases.

## Streams and events

- Use `Stream` for event subscriptions and subscriptions that can end.
- Convert async iterables with `Stream.fromAsyncIterable` and run them with a scoped `Stream.runForEach`.
- Do not leak a stream fiber; tie it to the plugin scope so plugin unload closes subscriptions.

## Errors

- Use typed failures (`Effect.fail`, `Schema.TaggedError`) at boundaries that the Rust side must distinguish.
- Convert unknown throws with `Effect.try`/`Effect.tryPromise` and preserve the message.
- Never swallow a plugin error silently; log it with context and return a structured failure to Rust.

## Testing

- Test pure transformations directly.
- Use the Effect test runtime for time and concurrency; do not sleep to wait for a fiber.
- Keep a small in-memory layer for services instead of a real network or filesystem.

## Common pitfalls

- Calling `Effect.runPromise` inside a service loses the caller's context and clock.
- Forgetting to `yield*` an effect returns the effect object instead of its value.
- Mutating an object after handing it to another fiber can break Schema assumptions; clone boundary data.
- A Promise from a non-Effect library must be wrapped, not awaited in the middle of an `Effect.gen` without `yield* Effect.tryPromise`.
