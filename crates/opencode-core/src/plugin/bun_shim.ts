// Optional Bun-only probes (bun:ffi, bun:jsc). Deno has no equivalent native
// FFI sampler, and the plugins that import these treat a missing API as
// "feature disabled", so an empty module is the correct fallback.
export {}
