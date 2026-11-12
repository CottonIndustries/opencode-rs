// OpenCode v2 plugin host. Runs under Deno, loads plugin modules and bridges
// their v2 (Promise/Effect) or legacy v1 hooks to the Rust process over
// newline-delimited JSON-RPC on stdout/stdin.
//
// This file is embedded into the opencode binary and written to the cache
// directory at runtime.

import { Effect, Exit, Schema, Scope, Stream } from "effect"
import { z } from "npm:zod@4.1.8"
import { toJSONSchema } from "npm:zod@4.1.8/v4/core"

type Json = any
type PluginMode = "promise" | "effect" | "legacy"

const encoder = new TextEncoder()

// Keep stdout exclusively for protocol frames. Anything a plugin logs goes to
// stderr so it cannot corrupt the bridge.
const outWrite = (line: string) => Deno.stdout.writeSync(encoder.encode(line + "\n"))
console.log = (...args: unknown[]) => console.error(...args)
console.info = (...args: unknown[]) => console.error(...args)
console.debug = (...args: unknown[]) => console.error(...args)
console.warn = (...args: unknown[]) => console.error(...args)

const configPath = Deno.args[0]
if (!configPath) {
  console.error("opencode plugin host: missing configuration path")
  Deno.exit(2)
}

const config: Json = JSON.parse(Deno.readTextFileSync(configPath))
const directory: string = config.directory
const worktree: string = config.worktree
const project: string = config.project ?? "global"
const version: string = config.version ?? "0.0.0"
const channel: string = config.channel ?? "latest"
const storageDir: string = config.storageDir ?? ""

// ---------------------------------------------------------------- transport

let nextID = 1
const pending = new Map<number, { resolve: (value: Json) => void; reject: (error: unknown) => void }>()

function send(message: Json) {
  outWrite(JSON.stringify(message))
}

function request(method: string, params?: Json): Promise<Json> {
  const id = nextID++
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject })
    send({ type: "request", id, method, params: params ?? {} })
  })
}

function notify(method: string, params?: Json) {
  send({ type: "notification", method, params: params ?? {} })
}

let eventQueue: Promise<void> = Promise.resolve()

function log(level: "debug" | "info" | "warn" | "error", message: string, extra?: Json) {
  console.error(`[plugin:${level}] ${message}${extra === undefined ? "" : " " + safe(extra)}`)
  try {
    notify("log", { level, message, extra })
  } catch {
    // stdout may be gone during shutdown
  }
}

function safe(value: unknown): string {
  try {
    return JSON.stringify(value)
  } catch {
    return String(value)
  }
}

async function dispatch(method: string, params: Json): Promise<Json> {
  switch (method) {
    case "initialize":
      return await ready
    case "hook.trigger":
      return await trigger(params.domain, params.name, params.event)
    case "tool.list":
      return toolList()
    case "tool.execute":
      return await executePluginTool(params)
    case "tool.abort":
      abortControllers.get(params.callID)?.abort()
      return {}
    case "command.execute":
      return await executePluginCommand(params)
    case "plugin.list":
      return pluginList()
    case "websearch.list":
      return {
        providers: [...registry.websearch.values()].map((item) => ({ id: item.id, name: item.name })),
        default: websearchDefault,
      }
    case "websearch.execute": {
      const providerID = params.providerID ?? websearchDefault
      const definition = typeof providerID === "string" ? registry.websearch.get(providerID) : undefined
      if (!definition || typeof definition.execute !== "function") {
        throw new Error(`web search provider not found: ${String(providerID)}`)
      }
      const controller = new AbortController()
      const results = await settle(definition.execute({ query: params.query }, { signal: controller.signal }))
      return { providerID, results }
    }
    case "flush":
      await eventQueue
      return {}
    case "state.reload":
      rebuild(params.domain)
      return domainSnapshot()
    case "shutdown":
      await shutdown()
      return {}
    default:
      throw new Error(`unknown host method: ${method}`)
  }
}

async function readLoop() {
  const reader = Deno.stdin.readable.pipeThrough(new TextDecoderStream()).getReader()
  let buffer = ""
  try {
    while (true) {
      const { value, done } = await reader.read()
      if (done) break
      buffer += value
      let index = buffer.indexOf("\n")
      while (index !== -1) {
        const line = buffer.slice(0, index).trim()
        buffer = buffer.slice(index + 1)
        if (line) {
          let message: Json
          try {
            message = JSON.parse(line)
          } catch (error) {
            log("error", `invalid bridge frame: ${String(error)}`)
            index = buffer.indexOf("\n")
            continue
          }
          void receive(message)
        }
        index = buffer.indexOf("\n")
      }
    }
  } catch (error) {
    if (!(error instanceof Deno.errors.BadResource)) log("error", `stdin closed: ${String(error)}`)
  }
  await shutdown()
  Deno.exit(0)
}

async function receive(message: Json) {
  if (message.type === "response") {
    const entry = pending.get(message.id)
    if (!entry) return
    pending.delete(message.id)
    if (message.ok) entry.resolve(message.result)
    else entry.reject(new Error(message.error?.message ?? String(message.error ?? "plugin host request failed")))
    return
  }
  if (message.type === "notification") {
    if (message.method === "event") {
      const event = message.params
      eventQueue = eventQueue
        .then(() => emitEvent(event))
        .catch((error) => log("error", `event dispatch failed: ${String(error)}`))
    }
    return
  }
  if (message.type === "request") {
    try {
      const result = await dispatch(message.method, message.params ?? {})
      send({ type: "response", id: message.id, ok: true, result })
    } catch (error) {
      send({
        type: "response",
        id: message.id,
        ok: false,
        error: { message: error instanceof Error ? error.message : String(error) },
      })
    }
  }
}

// ---------------------------------------------------------------- registries

type ToolDef = {
  id: string
  name: string
  description: string
  input: Json
  output?: Json
  options?: Json
  execute?: (input: Json, context: Json) => unknown
  plugin?: PluginRecord
  source: "builtin" | "custom"
}

type ProviderRecord = {
  provider: Json
  models: Map<string, Json>
  sourceConnection?: Json
}

type PluginRecord = {
  id: string
  spec: string
  source: Json
  options: Json
  mode: PluginMode
  features: Json
  state: Json
  module?: Json
  hooks?: Json
  cleanup?: (() => unknown) | undefined
  scope?: Scope.Closeable
  directTools: Map<string, ToolDef>
  registrations: Array<() => void>
}

const plugins: PluginRecord[] = []
const rpcs = new Map<string, { plugin: PluginRecord; definition: Json; handlers: Json }>()

type HookEntry = {
  plugin: PluginRecord
  callback: Function
  options?: { providerID?: string }
  legacy?: string
}
const hooks = new Map<string, HookEntry[]>()

type TransformEntry = { plugin: PluginRecord; callback: Function }
const transforms = new Map<string, TransformEntry[]>()

type Registry = {
  agents: Map<string, Json>
  commands: Map<string, Json>
  skills: Map<string, Json>
  tools: Map<string, ToolDef>
  providers: Map<string, ProviderRecord>
  mcp: Map<string, Json>
  websearch: Map<string, Json>
  references: Map<string, Json>
  integrations: Map<string, Json>
  artifacts: Map<string, Json>
  worktrees: Map<string, Json>
  vcs: Map<string, Json>
  defaultAgent?: string
  defaultModel?: Json
}

let websearchDefault: string | false | undefined

const registry: Registry = {
  agents: new Map(),
  commands: new Map(),
  skills: new Map(),
  tools: new Map(),
  providers: new Map(),
  mcp: new Map(),
  websearch: new Map(),
  references: new Map(),
  integrations: new Map(),
  artifacts: new Map(),
  worktrees: new Map(),
  vcs: new Map(),
}

const initialState: Json = config.state ?? {}
const abortControllers = new Map<string, AbortController>()
let initializing = true

function clone<T>(value: T): T {
  return structuredClone(value)
}

function buildDomain(domain: string) {
  switch (domain) {
    case "agents": {
      const map = new Map<string, Json>()
      for (const item of initialState.agents ?? []) map.set(item.id ?? item.name, clone(item))
      return { value: map, defaultAgent: initialState.defaultAgent }
    }
    case "commands": {
      const map = new Map<string, Json>()
      for (const item of initialState.commands ?? []) map.set(item.name, clone(item))
      return { value: map }
    }
    case "skills": {
      const map = new Map<string, Json>()
      for (const item of initialState.skills ?? []) map.set(item.id, clone(item))
      return { value: map }
    }
    case "tools": {
      const map = new Map<string, ToolDef>()
      for (const item of initialState.tools ?? []) {
        const id = item.id ?? item.name
        map.set(id, {
          ...clone(item),
          id,
          name: item.name ?? id,
          source: "builtin",
        })
      }
      return { value: map }
    }
    case "providers": {
      const map = new Map<string, ProviderRecord>()
      for (const item of initialState.providers ?? []) {
        const provider = clone(item.info ?? item.provider)
        const models = new Map<string, Json>()
        for (const model of item.models ?? []) models.set(model.id, clone(model))
        map.set(provider.id, { provider, models, sourceConnection: item.sourceConnection })
      }
      return { value: map, defaultModel: initialState.defaultModel }
    }
    case "mcp": {
      const map = new Map<string, Json>()
      for (const entry of initialState.mcp ?? []) {
        if (Array.isArray(entry)) map.set(entry[0], clone(entry[1]))
      }
      return { value: map }
    }
    case "websearch": {
      const map = new Map<string, Json>()
      for (const item of initialState.websearch ?? []) map.set(item.id, clone(item))
      return { value: map }
    }
    case "references": {
      const map = new Map<string, Json>()
      for (const entry of initialState.references ?? []) {
        if (Array.isArray(entry)) map.set(entry[0], clone(entry[1]))
      }
      return { value: map }
    }
    case "integrations": {
      const map = new Map<string, Json>()
      for (const item of initialState.integrations ?? []) map.set(item.id, clone(item))
      return { value: map }
    }
    case "artifacts": {
      const map = new Map<string, Json>()
      for (const item of initialState.artifacts ?? []) map.set(item.id, clone(item))
      return { value: map }
    }
    case "worktrees": {
      const map = new Map<string, Json>()
      for (const item of initialState.worktrees ?? []) map.set(item.id, clone(item))
      return { value: map }
    }
    case "vcs": {
      const map = new Map<string, Json>()
      for (const item of initialState.vcs ?? []) map.set(item.id, clone(item))
      return { value: map }
    }
    default: {
      const map = new Map<string, Json>()
      return { value: map }
    }
  }
}

function installDomain(domain: string) {
  const built = buildDomain(domain) as Json
  ;(registry as Json)[domain] = built.value
  if ("defaultAgent" in built) registry.defaultAgent = built.defaultAgent
  if ("defaultModel" in built) registry.defaultModel = built.defaultModel
}

let stateChangeScheduled = false

function scheduleStateChanged() {
  if (initializing || stateChangeScheduled) return
  stateChangeScheduled = true
  queueMicrotask(() => {
    stateChangeScheduled = false
    if (initializing) return
    notify("state.changed", { state: domainSnapshot() })
  })
}

function rebuild(domain?: string) {
  const domains = domain ? [domain] : Object.keys(registry).filter((key) => registry[key as keyof Registry] instanceof Map)
  for (const name of domains) {
    installDomain(name)
    for (const transform of transforms.get(name) ?? []) {
      try {
        transform.callback(editorFor(name))
      } catch (error) {
        log("error", `plugin ${transform.plugin.id} transform ${name} failed while reloading: ${String(error)}`)
      }
    }
    if (name === "tools") {
      for (const plugin of plugins) {
        for (const tool of plugin.directTools.values()) registry.tools.set(tool.id, tool)
      }
    }
  }
  scheduleStateChanged()
}

function registerTransform(plugin: PluginRecord, domain: string, callback: Function) {
  const list = transforms.get(domain) ?? []
  list.push({ plugin, callback })
  transforms.set(domain, list)
  try {
    callback(editorFor(domain))
  } catch (error) {
    log("error", `plugin ${plugin.id} transform ${domain} failed: ${String(error)}`)
  }
  scheduleStateChanged()
  const remove = () => {
    const current = transforms.get(domain) ?? []
    transforms.set(domain, current.filter((item) => item.callback !== callback))
  }
  plugin.registrations.push(remove)
  return remove
}

function registerHook(
  plugin: PluginRecord,
  domain: string,
  name: string,
  callback: Function,
  options?: { providerID?: string },
) {
  const key = `${domain}.${name}`
  const list = hooks.get(key) ?? []
  const entry = { plugin, callback, options }
  list.push(entry)
  hooks.set(key, list)
  const remove = () => {
    const current = hooks.get(key) ?? []
    const next = current.filter((item) => item !== entry)
    if (next.length) hooks.set(key, next)
    else hooks.delete(key)
  }
  plugin.registrations.push(remove)
  return { remove }
}

function isEffect(value: unknown): value is Effect.Effect<unknown, unknown, unknown> {
  return Effect.isEffect(value)
}

async function settle(value: unknown): Promise<unknown> {
  if (isEffect(value)) return await Effect.runPromise(value as Effect.Effect<unknown, never, never>)
  return await value
}

async function runCallback(callback: Function, event: Json): Promise<void> {
  await settle(callback(event))
}

// ---------------------------------------------------------------- editors

function editorFor(domain: string): Json {
  const target = registry[domain as keyof Registry] as unknown as Map<string, Json>
  switch (domain) {
    case "agents":
      return {
        list: () => [...target.values()],
        get: (id: string) => target.get(id),
        default: (id: string | undefined) => {
          registry.defaultAgent = id
        },
        update: (id: string, update: (value: Json) => void) => {
          let value = target.get(id)
          if (!value) {
            value = {
              id,
              name: id,
              request: { settings: {}, headers: {}, body: {} },
              mode: "primary",
              hidden: false,
              permissions: [
                { action: "*", resource: "*", effect: "allow" },
                { action: "read", resource: "*.env", effect: "ask" },
                { action: "read", resource: "*.env.*", effect: "ask" },
                { action: "read", resource: "*.env.example", effect: "allow" },
              ],
            }
            target.set(id, value)
          }
          update(value)
          value.id = id
        },
        remove: (id: string) => {
          target.delete(id)
        },
      }
    case "commands":
      return {
        add: (definition: Json) => {
          target.set(definition.name, definition)
        },
      }
    case "skills":
      return {
        list: () => [...target.values()],
        get: (id: string) => target.get(id),
        add: (skill: Json) => {
          target.set(skill.id, skill)
        },
        update: (id: string, update: (value: Json) => void) => {
          const value = target.get(id)
          if (value) update(value)
        },
        remove: (id: string) => {
          target.delete(id)
        },
      }
    case "tools": {
      const tools = target as unknown as Map<string, ToolDef>
      return {
        list: () => [...tools.values()],
        get: (id: string) => tools.get(id),
        namespace: (_namespace: Json) => {},
        add: (tool: Json) => {
          const id = tool.id ?? tool.name
          tools.set(id, { ...tool, id, name: tool.name ?? id, source: "custom" })
        },
        update: (id: string, update: (value: Json) => void) => {
          const value = tools.get(id)
          if (value) update(value)
        },
        remove: (id: string) => {
          tools.delete(id)
        },
      }
    }
    case "providers": {
      const providers = target as unknown as Map<string, ProviderRecord>
      return {
        list: () => [...providers.values()].map((item) => ({
          provider: item.provider,
          models: item.models,
          sourceConnection: item.sourceConnection,
        })),
        get: (id: string) => {
          const item = providers.get(id)
          return item ? { provider: item.provider, models: item.models, sourceConnection: item.sourceConnection } : undefined
        },
        add: (input: Json) => {
          const models = new Map<string, Json>()
          for (const model of input.models ?? []) models.set(model.id, model)
          providers.set(input.info.id, {
            provider: input.info,
            models,
            sourceConnection: input.sourceConnection,
          })
        },
        update: (id: string, update: (value: Json) => void) => {
          const item = providers.get(id)
          if (item) update(item.provider)
        },
        remove: (id: string) => {
          providers.delete(id)
        },
        models: {
          set: (providerID: string, models: Json[]) => {
            const item = providers.get(providerID)
            if (!item) return
            item.models = new Map(models.map((model) => [model.id, model]))
          },
          update: (providerID: string, modelID: string, update: (value: Json) => void) => {
            const item = providers.get(providerID)
            const model = item?.models.get(modelID)
            if (model) update(model)
          },
          remove: (providerID: string, modelID: string) => {
            providers.get(providerID)?.models.delete(modelID)
          },
        },
      }
    }
    case "model": {
      const providers = registry.providers
      return {
        list: (providerID?: string) => {
          const out: Json[] = []
          for (const [id, record] of providers) {
            if (providerID && id !== providerID) continue
            out.push(...record.models.values())
          }
          return out
        },
        get: (providerID: string, modelID: string) => providers.get(providerID)?.models.get(modelID),
        update: (providerID: string, modelID: string, update: (value: Json) => void) => {
          const model = providers.get(providerID)?.models.get(modelID)
          if (model) update(model)
        },
        remove: (providerID: string, modelID: string) => {
          providers.get(providerID)?.models.delete(modelID)
        },
        default: {
          get: () => registry.defaultModel,
          set: (providerID: string, modelID: string) => {
            registry.defaultModel = { providerID, id: modelID, modelID }
          },
        },
        provider: {
          list: () => [...providers.values()].map((item) => ({ provider: item.provider, models: item.models })),
          get: (id: string) => {
            const item = providers.get(id)
            return item ? { provider: item.provider, models: item.models } : undefined
          },
        },
      }
    }
    case "mcp":
      return {
        list: () => [...target.entries()],
        get: (name: string) => target.get(name),
        set: (name: string, value: Json) => {
          target.set(name, value)
        },
        update: (name: string, update: (value: Json) => void) => {
          const value = target.get(name)
          if (value) update(value)
        },
        remove: (name: string) => {
          target.delete(name)
        },
      }
    case "references":
      return {
        add: (name: string, source: Json) => {
          target.set(name, source)
        },
        remove: (name: string) => {
          target.delete(name)
        },
        list: () => [...target.entries()],
        get: (name: string) => target.get(name),
      }
    case "integrations":
      return {
        list: () => [...target.values()],
        get: (id: string) => target.get(id),
        update: (id: string, update: (value: Json) => void) => {
          const value = target.get(id)
          if (value) update(value)
        },
        remove: (id: string) => {
          target.delete(id)
        },
        method: {
          list: () => [],
          update: (_input: Json) => {},
          remove: (_id: string, _method: Json) => {},
        },
      }
    case "websearch":
      return {
        list: () => [...target.values()],
        get: (id: string) => target.get(id),
        add: (definition: Json) => {
          const id = definition.id ?? definition.name
          target.set(id, { ...definition, id })
        },
        remove: (id: string) => {
          target.delete(id)
        },
        default: {
          get: () => websearchDefault,
          set: (value: string | false) => {
            websearchDefault = value
          },
        },
      }
    case "artifacts":
    case "worktrees":
    case "vcs":
    default:
      return {
        list: () => [...target.values()],
        get: (id: string) => target.get(id),
        add: (definition: Json) => {
          const id = definition.id ?? definition.name
          target.set(id, { ...definition, id })
        },
        remove: (id: string) => {
          target.delete(id)
        },
        update: (id: string, update: (value: Json) => void) => {
          const value = target.get(id)
          if (value) update(value)
        },
        default: {
          get: () => undefined,
          set: (_id: string | false | undefined) => {},
        },
      }
  }
}

for (const domain of ["agents", "commands", "skills", "tools", "providers", "mcp", "websearch", "references", "integrations", "artifacts", "worktrees", "vcs"]) {
  installDomain(domain)
}

// ---------------------------------------------------------------- hooks

function toV2Messages(messages: Json): Json[] {
  return (Array.isArray(messages) ? messages : []).map((message: Json) => {
    const role = message.info?.role ?? "user"
    const content: Json[] = []
    for (const part of message.parts ?? []) {
      if (part.type === "text") content.push({ type: "text", text: part.text })
      else if (part.type === "reasoning") content.push({ type: "reasoning", text: part.text })
      else if (part.type === "file") {
        content.push({
          type: "media",
          media: { source: { type: "base64", data: part.data, mediaType: part.mime } },
          filename: part.filename,
        })
      } else if (part.type === "tool") {
        content.push({
          type: "tool-call",
          id: part.callID,
          name: part.tool,
          input: part.state?.input ?? part.input ?? {},
        })
      }
    }
    return { role, content }
  })
}

function applyV2Messages(event: Json, messages: Json[]) {
  const original = Array.isArray(event.messages) ? event.messages : []
  for (let index = 0; index < original.length && index < messages.length; index++) {
    const parts = original[index]?.parts ?? []
    const content = Array.isArray(messages[index]?.content) ? messages[index].content : []
    const texts = content.filter((part: Json) => part?.type === "text")
    const reasoning = content.filter((part: Json) => part?.type === "reasoning")
    let textIndex = 0
    let reasoningIndex = 0
    for (const part of parts) {
      if (part.type === "text" && textIndex < texts.length) {
        part.text = texts[textIndex].text
        textIndex++
      } else if (part.type === "reasoning" && reasoningIndex < reasoning.length) {
        part.text = reasoning[reasoningIndex].text
        reasoningIndex++
      }
    }
  }
}

const V2_MESSAGE_HOOKS = new Set(["context", "title", "compaction", "generate"])

async function trigger(domain: string, name: string, event: Json): Promise<Json> {
  const key = `${domain}.${name}`
  const list = hooks.get(key) ?? []
  const messageHook = domain === "session" && V2_MESSAGE_HOOKS.has(name)
  for (const entry of list) {
    if (entry.options?.providerID && event?.model?.providerID !== entry.options.providerID) continue
    try {
      if (entry.plugin.mode === "legacy") {
        await legacyTrigger(entry, domain, name, event)
      } else if (messageHook) {
        const v2 = { ...event, messages: toV2Messages(event.messages) }
        await runCallback(entry.callback, v2)
        applyV2Messages(event, v2.messages)
        if (v2.system !== undefined) event.system = v2.system
        if (v2.options !== undefined) event.options = v2.options
        if (v2.tools !== undefined) event.tools = v2.tools
        if (v2.result !== undefined) event.result = v2.result
      } else {
        await runCallback(entry.callback, event)
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error)
      log("error", `plugin ${entry.plugin.id} hook ${key} failed: ${message}`)
      if (key === "tool.execute.before") {
        event.__error = message
        return event
      }
    }
  }
  return event
}

function toV2Event(event: Json): Json {
  const properties = event.properties ?? event.data ?? {}
  let type = event.type
  let data = properties
  if (event.type === "message.part.updated") {
    const part = properties.part ?? {}
    const assistantMessageID = properties.assistantMessageID ?? properties.messageID ?? "msg_current"
    if (part.type === "text") {
      type = "session.text.delta"
      data = {
        sessionID: properties.sessionID,
        assistantMessageID,
        ordinal: properties.ordinal ?? 0,
        delta: properties.delta ?? part.text ?? "",
      }
    } else if (part.type === "reasoning") {
      type = "session.reasoning.delta"
      data = {
        sessionID: properties.sessionID,
        assistantMessageID,
        ordinal: properties.ordinal ?? 0,
        delta: properties.delta ?? part.text ?? "",
      }
    } else if (part.type === "tool") {
      const status = part.state?.status
      const base = {
        sessionID: properties.sessionID,
        assistantMessageID,
        id: properties.callID ?? part.callID,
        name: part.tool ?? part.name,
        executed: true,
      }
      if (status === "completed") {
        type = "session.tool.success"
        data = {
          ...base,
          content: [{ type: "text", text: part.state.output ?? "" }],
          metadata: part.state.metadata ?? {},
        }
      } else if (status === "error") {
        type = "session.tool.failed"
        data = {
          ...base,
          error: { type: "unknown", message: String(part.state.error ?? "") },
        }
      } else {
        type = "session.tool.called"
        data = { ...base, input: part.state?.input ?? {} }
      }
    }
  } else if (event.type === "session.idle") {
    data = { sessionID: properties.sessionID }
  } else if (event.type === "session.status") {
    data = { sessionID: properties.sessionID, status: properties.status ?? { type: "busy" } }
  } else if (event.type === "session.updated") {
    const info = properties.info ?? {}
    if (typeof info.title === "string") {
      type = "session.renamed"
      data = { sessionID: properties.sessionID ?? info.id, title: info.title }
    } else {
      type = "session.metadata.updated"
      data = { sessionID: properties.sessionID ?? info.id, metadata: info.metadata ?? {} }
    }
  } else if (event.type === "session.created") {
    const info = properties.info ?? {}
    data = {
      sessionID: info.id ?? properties.sessionID,
      projectID: info.projectID,
      location: info.location ?? { directory: directory },
      parentID: info.parentID ?? undefined,
      slug: info.id ?? properties.sessionID,
      title: info.title,
      agent: info.agent,
      model: info.model,
      version,
    }
  } else if (event.type === "message.updated") {
    type = "session.message.content.updated"
  } else if (event.type === "session.retry") {
    type = "session.retry.scheduled"
    data = {
      sessionID: properties.sessionID,
      assistantMessageID: properties.assistantMessageID ?? "msg_current",
      attempt: properties.attempt ?? 1,
      at: properties.at ?? Date.now(),
      error: properties.error ?? { type: "unknown", message: properties.message ?? "" },
    }
  } else if (event.type === "session.step") {
    type = "session.step.ended"
    data = {
      sessionID: properties.sessionID,
      assistantMessageID: properties.assistantMessageID ?? "msg_current",
      finish: "stop",
      cost: 0,
      tokens: { input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } },
    }
  } else if (event.type === "permission.replied") {
    data = {
      sessionID: properties.sessionID,
      requestID: properties.requestID,
      reply: properties.reply ?? "once",
    }
  }
  return { id: event.id ?? `evt_${Date.now()}`, created: event.created ?? properties.created ?? Date.now(), type, data }
}

async function emitEvent(event: Json) {
  for (const plugin of plugins) {
    const handler = plugin.hooks?.event
    if (typeof handler !== "function") continue
    try {
      // Legacy V1 hooks receive the V1 envelope.
      await settle(handler({ event }))
    } catch (error) {
      log("error", `plugin ${plugin.id} event hook failed: ${String(error)}`)
    }
  }
  const v2 = toV2Event(event)
  for (const subscriber of subscribers) {
    if (subscriber.done) continue
    if (subscriber.types && !subscriber.types.has(v2.type)) continue
    subscriber.queue.push(v2)
    subscriber.wake?.()
  }
}

type Subscriber = {
  types?: Set<string>
  queue: Json[]
  wake?: () => void
  done: boolean
}
const subscribers = new Set<Subscriber>()

function subscribeEvents(options?: { types?: string[]; signal?: AbortSignal }): AsyncIterable<Json> {
  const subscriber: Subscriber = {
    types: options?.types?.length ? new Set(options.types) : undefined,
    queue: [],
    done: false,
  }
  subscribers.add(subscriber)
  const finish = () => {
    subscriber.done = true
    subscribers.delete(subscriber)
    subscriber.wake?.()
  }
  options?.signal?.addEventListener("abort", finish, { once: true })
  return {
    async *[Symbol.asyncIterator]() {
      try {
        yield { id: "evt_connected", created: Date.now(), type: "server.connected", data: {} }
        while (!subscriber.done) {
          if (!subscriber.queue.length) {
            await new Promise<void>((resolve) => {
              subscriber.wake = resolve
            })
            subscriber.wake = undefined
            continue
          }
          const event = subscriber.queue.shift()!
          if (subscriber.types && !subscriber.types.has(event.type)) continue
          yield event
        }
      } finally {
        finish()
      }
    },
  }
}

// ---------------------------------------------------------------- tool helpers

function schemaToJson(schema: Json): Json {
  if (!schema) return { type: "object", properties: {} }
  try {
    if (Schema.isSchema(schema)) return Schema.toJsonSchemaDocument(schema).schema
  } catch {
    // not an Effect schema
  }
  if (typeof schema === "object" && schema["~standard"]?.jsonSchema) {
    try {
      return schema["~standard"].jsonSchema.input({ target: "draft-2020-12" })
    } catch {
      // fall through
    }
  }
  if (typeof schema === "object" && "_zod" in schema) {
    try {
      return toJSONSchema(schema, { target: "draft-2020-12" })
    } catch {
      // fall through
    }
  }
  return schema
}

function toolList(): Json {
  return [...registry.tools.values()].map((tool) => ({
    id: tool.id,
    name: tool.name,
    description: tool.description ?? "",
    input: schemaToJson(tool.input),
    output: tool.output === undefined ? undefined : schemaToJson(tool.output),
    options: tool.options ?? {},
    source: tool.source,
    plugin: tool.plugin?.id,
  }))
}

function zodJSONSchema(args: Json): Json {
  if (!args || typeof args !== "object") return { type: "object", properties: {} }
  const values = Object.values(args)
  const allZod = values.length > 0 && values.every((value: Json) => value && typeof value === "object" && "_zod" in value)
  if (allZod) {
    try {
      return toJSONSchema(z.object(args), { target: "draft-2020-12" })
    } catch {
      // fall through to the plain JSON-schema interpretation
    }
  }
  return { type: "object", properties: args, required: Object.keys(args) }
}

function normalizeToolResult(result: Json): Json {
  if (typeof result === "string") return { output: result, title: "", metadata: {}, content: result }
  if (!result || typeof result !== "object") return { output: String(result ?? ""), title: "", metadata: {} }
  let content = result.content
  if (Array.isArray(content)) {
    content = content
      .map((part: Json) => (part?.type === "text" ? part.text ?? "" : part?.type === "file" ? `[file ${part.name ?? part.uri}]` : ""))
      .filter(Boolean)
      .join("\n")
  }
  let output = result.output
  if (output === undefined) output = typeof content === "string" ? content : ""
  if (typeof output !== "string") output = JSON.stringify(output)
  return {
    output,
    title: result.title ?? "",
    metadata: result.metadata ?? {},
    content: typeof content === "string" ? content : undefined,
    attachments: Array.isArray(result.attachments)
      ? result.attachments.filter((part: Json) => part?.type === "file")
      : Array.isArray(result.content)
        ? result.content
            .filter((part: Json) => part?.type === "file")
            .map((part: Json) => ({ mime: part.mime ?? "application/octet-stream", url: part.uri, filename: part.name }))
        : undefined,
  }
}

async function executePluginTool(params: Json): Promise<Json> {
  const tool = registry.tools.get(params.name)
  if (!tool) throw new Error(`plugin tool not found: ${params.name}`)
  if (!tool.execute) throw new Error(`plugin tool ${params.name} has no executor`)
  const controller = new AbortController()
  if (params.callID) abortControllers.set(params.callID, controller)
  try {
    const legacy = tool.plugin?.mode === "legacy"
    const context: Json = {
      ...params.context,
      signal: controller.signal,
      abort: controller.signal,
      metadata: (input: Json) => {
        notify("tool.metadata", { callID: params.callID, ...input })
      },
      progress: (update: Json) => {
        notify("tool.metadata", { callID: params.callID, metadata: update })
        return tool.plugin?.mode === "effect" ? Effect.sync(() => {}) : Promise.resolve()
      },
      ask: (input: Json) => {
        const translated = legacy
          ? {
              action: input?.permission ?? input?.action,
              resources: input?.patterns ?? input?.resources,
              save: input?.always ?? input?.save,
              metadata: input?.metadata ?? {},
              sessionID: params.context?.sessionID,
              agent: params.context?.agent,
            }
          : input
        return request("permission.assert", translated)
      },
    }
    const result = await settle(tool.execute(params.input ?? {}, context))
    return normalizeToolResult(result)
  } finally {
    if (params.callID) abortControllers.delete(params.callID)
  }
}

async function executePluginCommand(params: Json): Promise<Json> {
  const command = registry.commands.get(params.name)
  if (!command) throw new Error(`plugin command not found: ${params.name}`)
  if (typeof command.execute !== "function") return {}
  const invocation = {
    sessionID: params.sessionID,
    prompt: { text: params.arguments ?? "" },
    delivery: params.delivery ?? "steer",
  }
  const result = await settle(command.execute(invocation))
  return result ?? {}
}

// ---------------------------------------------------------------- v1 adapter

function legacyInput(): Json {
  return {
    client: makeClientProxy(),
    project: { id: project, directory, canonical: worktree },
    directory,
    worktree,
    serverUrl: new URL("http://localhost:4096"),
    experimental_workspace: { register: () => {} },
    $: makeBunShell(),
  }
}

function legacyTrigger(entry: HookEntry, domain: string, name: string, event: Json) {
  const cb = entry.callback
  const legacy = entry.legacy
  if (legacy === "tool.execute.before") {
    const output = { args: event.input }
    return Promise.resolve(cb({ tool: event.tool, sessionID: event.sessionID, callID: event.id }, output)).then(() => {
      if (output.args !== undefined) event.input = output.args
    })
  }
  if (legacy === "tool.execute.after") {
    const result = event.result ?? event.error ?? {}
    const output = {
      title: result.title ?? "",
      output: result.output ?? result.message ?? "",
      metadata: result.metadata ?? {},
    }
    return Promise.resolve(
      cb({ tool: event.tool, sessionID: event.sessionID, callID: event.id, args: event.input }, output),
    ).then(() => {
      if (event.status === "completed") {
        event.result = { ...(event.result ?? {}), title: output.title, output: output.output, metadata: output.metadata }
      }
    })
  }
  if (legacy === "chat.message") {
    const output = { message: event.message, parts: event.parts }
    return Promise.resolve(
      cb(
        {
          sessionID: event.sessionID,
          agent: event.agent,
          model: event.model,
          messageID: event.messageID,
          variant: event.variant,
        },
        output,
      ),
    ).then(() => {
      event.message = output.message
      event.parts = output.parts
    })
  }
  if (legacy === "chat.params") {
    const output = {
      temperature: event.options?.temperature,
      topP: event.options?.topP,
      topK: event.options?.topK,
      maxOutputTokens: event.options?.maxOutputTokens,
      options: event.options?.options ?? {},
    }
    return Promise.resolve(
      cb(
        {
          sessionID: event.sessionID,
          agent: event.agent,
          model: event.model,
          provider: event.provider,
          message: event.message,
        },
        output,
      ),
    ).then(() => {
      event.options = {
        ...(event.options ?? {}),
        temperature: output.temperature,
        topP: output.topP,
        topK: output.topK,
        maxOutputTokens: output.maxOutputTokens,
        options: output.options,
      }
    })
  }
  if (legacy === "chat.headers") {
    const output = { headers: event.headers ?? {} }
    return Promise.resolve(
      cb(
        {
          sessionID: event.sessionID,
          agent: event.agent,
          model: event.model,
          provider: event.provider,
          message: event.message,
        },
        output,
      ),
    ).then(() => {
      event.headers = output.headers
    })
  }
  if (legacy === "permission.ask") {
    const output = { status: event.effect ?? "ask" }
    return Promise.resolve(cb(event.request, output)).then(() => {
      if (output.status === "allow" || output.status === "deny" || output.status === "ask") event.effect = output.status
    })
  }
  if (legacy === "shell.env") {
    const output = { env: event.env ?? {} }
    return Promise.resolve(cb({ cwd: event.cwd, sessionID: event.sessionID, callID: event.callID }, output)).then(() => {
      event.env = output.env
    })
  }
  if (legacy === "experimental.chat.system.transform") {
    const output = { system: event.system }
    return Promise.resolve(cb({ sessionID: event.sessionID, model: event.model }, output)).then(() => {
      event.system = output.system
    })
  }
  if (legacy === "experimental.chat.messages.transform") {
    const output = { messages: event.messages }
    return Promise.resolve(cb({}, output)).then(() => {
      event.messages = output.messages
    })
  }
  if (legacy === "experimental.session.compacting") {
    const output = { context: [], prompt: undefined }
    return Promise.resolve(cb({ sessionID: event.sessionID }, output)).then(() => {
      event.context = output.context
      event.prompt = output.prompt
    })
  }
  if (legacy === "experimental.compaction.autocontinue") {
    const output = { enabled: true }
    return Promise.resolve(
      cb(
        {
          sessionID: event.sessionID,
          agent: event.agent,
          model: event.model,
          provider: event.provider,
          message: event.message,
          overflow: event.overflow,
        },
        output,
      ),
    ).then(() => {
      event.enabled = output.enabled
    })
  }
  if (legacy === "experimental.text.complete") {
    const output = { text: event.text }
    return Promise.resolve(
      cb({ sessionID: event.sessionID, messageID: event.messageID, partID: event.partID }, output),
    ).then(() => {
      event.text = output.text
    })
  }
  if (legacy === "tool.definition") {
    const output = { description: event.description, parameters: event.parameters }
    return Promise.resolve(cb({ toolID: event.toolID }, output)).then(() => {
      event.description = output.description
      event.parameters = output.parameters
    })
  }
  if (legacy === "command.execute.before") {
    const output = { parts: event.parts ?? [] }
    return Promise.resolve(
      cb({ command: event.command, sessionID: event.sessionID, arguments: event.arguments }, output),
    ).then(() => {
      event.parts = output.parts
    })
  }
  return Promise.resolve()
}

function registerLegacyHooks(plugin: PluginRecord, values: Json) {
  const map: Record<string, string> = {
    "tool.execute.before": "tool.execute.before",
    "tool.execute.after": "tool.execute.after",
    "chat.message": "session.prompt",
    "chat.params": "session.context",
    "chat.headers": "session.headers",
    "permission.ask": "permission.evaluate",
    "shell.env": "shell.create.before",
    "experimental.chat.system.transform": "session.system.transform",
    "experimental.chat.messages.transform": "session.messages.transform",
    "experimental.session.compacting": "session.compacting",
    "experimental.compaction.autocontinue": "session.autocontinue",
    "experimental.text.complete": "session.text.complete",
    "tool.definition": "tool.definition",
    "command.execute.before": "command.execute.before",
  }
  for (const [legacy, target] of Object.entries(map)) {
    const callback = values[legacy]
    if (typeof callback !== "function") continue
    const dot = target.indexOf(".")
    const domain = target.slice(0, dot)
    const name = target.slice(dot + 1)
    registerHook(plugin, domain, name, callback)
    const list = hooks.get(`${domain}.${name}`) ?? []
    const stored = list.find((item) => item.callback === callback)
    if (stored) stored.legacy = legacy
  }
  if (values.tool && typeof values.tool === "object") {
    for (const [name, definition] of Object.entries(values.tool)) {
      if (!definition || typeof definition !== "object") continue
      const def = definition as Json
      if (typeof def.execute !== "function") continue
      const id = name
      plugin.directTools.set(id, {
        id,
        name: id,
        description: def.description ?? "",
        input: def.args ? zodJSONSchema(def.args) : { type: "object", properties: {} },
        execute: (input: Json, context: Json) => def.execute(input, context),
        plugin,
        source: "custom",
      })
    }
  }
  if (values.dispose && typeof values.dispose === "function") {
    const previous = plugin.cleanup
    plugin.cleanup = async () => {
      if (previous) await previous()
      await values.dispose()
    }
  }
}

// ---------------------------------------------------------------- client shims

function apiResponse(data: Json): Json {
  return { location: { directory }, data }
}

function makeRegistration(mode: PluginMode, remove: () => void): Json {
  return mode === "effect"
    ? Effect.sync(() => ({ dispose: Effect.sync(remove) }))
    : Promise.resolve({ dispose: async () => remove() })
}

function makeClientProxy(path = ""): Json {
  const invoke = async (...args: Json[]) => {
    if (path === "config.get") return apiResponse(initialState.config ?? {})
    if (path === "session.list" && initializing) return apiResponse([])
    if (path === "app.log") {
      log("info", args[0]?.message ?? "", args[0])
      return apiResponse(undefined)
    }
    const result = await request("client.call", { path, args })
    if (result && Array.isArray(result.data) && (path === "session.list" || path === "session.messages")) {
      return apiResponse(result.data)
    }
    return apiResponse(result)
  }
  return new Proxy(invoke, {
    get(_target, prop) {
      if (prop === "then" || prop === "catch" || prop === "finally") return undefined
      if (typeof prop !== "string") return undefined
      return makeClientProxy(path ? `${path}.${prop}` : prop)
    },
  })
}

function makeBunShell(): Json {
  const shell = (strings: TemplateStringsArray | string, ...expressions: Json[]) => {
    let command: string
    if (typeof strings === "string") command = strings
    else {
      command = strings.raw[0]
      for (let i = 0; i < expressions.length; i++) {
        const value = expressions[i]
        const rendered = Array.isArray(value)
          ? value.map((item) => renderShellValue(item)).join(" ")
          : renderShellValue(value)
        command += rendered + (strings.raw[i + 1] ?? "")
      }
    }
    const state = { command, cwd: directory, env: {} as Record<string, string>, quiet: false, nothrow: false, started: false }
    let promise: Promise<Json> | undefined
    const exec = () => {
      if (!promise) {
        state.started = true
        promise = (async () => {
          const result = await request("shell.exec", { command: state.command, cwd: state.cwd, env: state.env })
          const output = {
            stdout: encoder.encode(result.stdout ?? ""),
            stderr: encoder.encode(result.stderr ?? ""),
            exitCode: result.exitCode ?? 0,
            text: () => result.stdout ?? "",
            json: () => JSON.parse(result.stdout ?? "null"),
            arrayBuffer: () => encoder.encode(result.stdout ?? "").buffer,
            bytes: () => encoder.encode(result.stdout ?? ""),
            blob: () => new Blob([result.stdout ?? ""]),
          }
          if (!state.nothrow && output.exitCode !== 0) {
            const error = new Error(`Command failed with exit code ${output.exitCode}: ${command}`) as Json
            error.exitCode = output.exitCode
            error.stdout = output.stdout
            error.stderr = output.stderr
            error.text = output.text
            throw error
          }
          return output
        })()
      }
      return promise
    }
    const chain: Json = {
      then: (onFulfilled?: Function, onRejected?: Function) => exec().then(onFulfilled as never, onRejected as never),
      catch: (onRejected?: Function) => exec().catch(onRejected as never),
      finally: (onFinally?: Function) => exec().finally(onFinally as never),
      cwd: (next: string) => {
        state.cwd = next
        return chain
      },
      env: (next: Record<string, string> | undefined) => {
        state.env = next ?? {}
        return chain
      },
      quiet: () => {
        state.quiet = true
        return chain
      },
      nothrow: () => {
        state.nothrow = true
        return chain
      },
      throws: (value: boolean) => {
        state.nothrow = !value
        return chain
      },
      text: async (encoding?: string) => (await exec()).text(encoding),
      json: async () => (await exec()).json(),
      lines: async function* () {
        const text = (await exec()).text()
        for (const line of text.split("\n")) yield line
      },
      arrayBuffer: async () => (await exec()).arrayBuffer(),
      blob: async () => (await exec()).blob(),
      stdin: new WritableStream(),
    }
    return chain
  }
  Object.assign(shell, {
    braces: (pattern: string) => [pattern],
    escape: (value: string) => "'" + value.replaceAll("'", "'\\''") + "'",
    env: (_next?: Record<string, string | undefined>) => shell,
    cwd: (_next?: string) => shell,
    nothrow: () => shell,
    throws: (_value: boolean) => shell,
  })
  return shell
}

function renderShellValue(value: Json): string {
  if (typeof value === "string") return value
  if (value && typeof value === "object" && "raw" in value) return String(value.raw)
  if (value && typeof value === "object" && typeof value.toString === "function" && value.toString !== Object.prototype.toString)
    return String(value)
  return String(value)
}

// ---------------------------------------------------------------- api builder

function callRPC(method: string, params?: Json): Promise<Json> {
  // The Rust App is built after plugin setup so registry transforms apply
  // before any session starts. Read-only client calls made during setup get
  // neutral answers instead of failing the plugin; they work normally once
  // initialization completes.
  if (initializing) {
    if (method === "session.list" || method === "permission.list") return Promise.resolve([])
    if (method.startsWith("session.") || method.startsWith("permission.")) return Promise.resolve(null)
    if (method === "generate.text") return Promise.resolve({ text: "" })
  }
  return request("client.call", { path: method, args: params === undefined ? [] : [params] })
}

function api(local: Json, mode: PluginMode, prefix: string, envelope = true): Json {
  const wrap = (fn: Function) => {
    if (mode !== "effect") return fn
    return (...args: Json[]) => Effect.tryPromise({ try: () => fn(...args), catch: (error: unknown) => error })
  }
  return new Proxy(local, {
    get(target, prop) {
      if (prop in target) return target[prop]
      if (prop === "then" || typeof prop !== "string") return undefined
      const fallback = async (...args: Json[]) => {
        const params = args.length === 0 ? undefined : args.length === 1 ? args[0] : args
        const result = await callRPC(prefix ? `${prefix}.${prop}` : prop, params)
        return envelope ? apiResponse(result) : result
      }
      return wrap(fallback)
    },
  })
}

function listProviderRecords(): Json[] {
  return [...registry.providers.values()].map((item) => ({
    provider: item.provider,
    models: item.models,
    sourceConnection: item.sourceConnection,
  }))
}

function providerRecord(id: string): Json | undefined {
  const item = registry.providers.get(id)
  return item ? { provider: item.provider, models: item.models, sourceConnection: item.sourceConnection } : undefined
}

function makeDomains(plugin: PluginRecord): Json {
  const mode = plugin.mode
  const effect = (fn: Function) => (mode === "effect" ? Effect.sync(fn) : Promise.resolve().then(fn))
  const sync = (fn: Function) => (mode === "effect" ? (...args: Json[]) => Effect.sync(() => fn(...args)) : fn)
  const lazy = (fn: Function) =>
    mode === "effect"
      ? (...args: Json[]) => Effect.tryPromise({ try: () => fn(...args), catch: (error: unknown) => error })
      : fn
  const registration = (remove: () => void) => makeRegistration(mode, remove)
  const local = (domain: string, prefix: string, extras: Json = {}, envelope = true): Json => {
    const target = registry[domain as keyof Registry] as unknown as Map<string, Json> | undefined
    const base: Json = {
      ...extras,
      transform: (callback: Function) => registration(registerTransform(plugin, domain, callback)),
      reload: () => effect(() => rebuild(domain)),
    }
    if (target instanceof Map) {
      if (!("list" in extras)) {
        base.list = sync(() => {
          const value = [...target.values()]
          return envelope ? apiResponse(value) : value
        })
      }
      if (!("get" in extras)) {
        base.get = sync((input: Json) => {
          const id = typeof input === "string"
            ? input
            : input?.id ?? input?.name ?? input?.agentID ?? input?.providerID ?? input?.skillID ?? input?.commandID
          const value = target.get(id)
          return envelope ? apiResponse(value) : value
        })
      }
    }
    return api(base, mode, prefix, envelope)
  }
  const domain = (name: string, prefix: string, hookDomain = name, envelope = true): Json => ({
    ...local(name, prefix, {}, envelope),
    hook: (hookName: string, callback: Function, options?: Json) =>
      registration(registerHook(plugin, hookDomain, hookName, callback, options).remove),
  })

  const agent = local("agents", "agent", {
    list: sync(() => apiResponse([...registry.agents.values()])),
    get: sync((input: Json) => {
      const id = typeof input === "string" ? input : input?.agentID ?? input?.id
      return apiResponse(registry.agents.get(id))
    }),
  })
  const commandTarget = registry.commands
  const command = local("commands", "command", {
    add: (definition: Json) => commandTarget.set(definition.name, definition),
    get: sync((name: string) => commandTarget.get(name)),
    execute: lazy((input: Json) => request("command.execute", input)),
  })
  const skill = local("skills", "skill")
  const tool = domain("tools", "tool", "tool", false)
  const mcp = local("mcp", "mcp", {
    list: lazy(async () => {
      const result = await callRPC("mcp.list")
      if (Array.isArray(result)) return apiResponse(result)
      return apiResponse([...registry.mcp.keys()].map((name) => ({ name, status: { status: "pending" } })))
    }),
  })
  const provider = local("providers", "provider", {
    list: sync(() => apiResponse([...registry.providers.values()].map((record) => record.provider))),
    get: sync((input: Json) => {
      const id = typeof input === "string" ? input : input?.providerID ?? input?.id
      return apiResponse(registry.providers.get(id)?.provider)
    }),
  })
  const modelBase: Json = {
    transform: (callback: Function) => registration(registerTransform(plugin, "model", callback)),
    reload: () => effect(() => rebuild("model")),
    list: (providerID?: string) =>
      sync((providerID?: string) => {
        const out: Json[] = []
        for (const [id, record] of registry.providers) {
          if (providerID && id !== providerID) continue
          out.push(...record.models.values())
        }
        return apiResponse(out)
      })(providerID),
    get: (providerID: string, modelID: string) =>
      sync((providerID: string, modelID: string) => apiResponse(registry.providers.get(providerID)?.models.get(modelID)))(providerID, modelID),
    update: (providerID: string, modelID: string, update: (value: Json) => void) =>
      sync((providerID: string, modelID: string, update: (value: Json) => void) => {
        const value = registry.providers.get(providerID)?.models.get(modelID)
        if (value) update(value)
      })(providerID, modelID, update),
    remove: (providerID: string, modelID: string) =>
      sync((providerID: string, modelID: string) => {
        registry.providers.get(providerID)?.models.delete(modelID)
      })(providerID, modelID),
    default: {
      get: sync(() => apiResponse(registry.defaultModel)),
      set: (providerID: string, modelID: string) =>
        sync((providerID: string, modelID: string) => {
          registry.defaultModel = { providerID, id: modelID, modelID }
        })(providerID, modelID),
    },
    provider: {
      list: sync(() => listProviderRecords()),
      get: sync((id: string) => providerRecord(id)),
    },
  }
  const model = api(modelBase, mode, "model")
  const permissionBase: Json = {
    hook: (hookName: string, callback: Function, options?: Json) =>
      registration(registerHook(plugin, "permission", hookName, callback, options).remove),
    assert: lazy((input: Json) => request("permission.assert", input)),
    list: lazy(async () => callRPC("permission.list")),
    get: lazy((input: Json) => callRPC("permission.get", input)),
    reply: lazy(async (input: Json) => {
      await callRPC("permission.reply", input)
      return undefined
    }),
  }
  const shell: Json = {
    hook: (hookName: string, callback: Function) =>
      registration(registerHook(plugin, "shell", hookName, callback).remove),
  }
  const event: Json = {
    subscribe: (options?: Json) =>
      mode === "effect"
        ? Stream.fromAsyncIterable(subscribeEvents(options), (error) => error as never)
        : subscribeEvents(options),
  }
  const configApi: Json = {
    get: sync(() => [{ type: "document", info: initialState.config ?? {} }]),
    shells: sync(() => []),
    providers: sync(() => listProviderRecords()),
    provider: sync((id: string) => providerRecord(id)),
  }
  const storage: Json = {
    get: lazy((key: string) => storageGet(`${plugin.id}/${key}`)),
    set: lazy((key: string, value: Json) => storageSet(`${plugin.id}/${key}`, value)),
    remove: lazy((key: string) => storageRemove(`${plugin.id}/${key}`)),
    scan: lazy(async (options: Json) => {
      const prefix = `${plugin.id}/${options?.prefix ?? ""}`
      const result = await storageScan({ ...options, prefix })
      const cut = plugin.id.length + 1
      return {
        entries: (result.entries ?? []).map((entry: Json) => ({
          key: String(entry.key).slice(cut),
          value: entry.value,
        })),
        next: typeof result.next === "string" ? result.next.slice(cut) : undefined,
      }
    }),
  }
  const aisdk: Json = {
    hook: (hookName: string, callback: Function, options?: Json) =>
      registration(registerHook(plugin, "aisdk", hookName, callback, options).remove),
  }
  const websearch = local("websearch", "websearch", {
    providers: sync(() =>
      apiResponse([...registry.websearch.values()].map((item) => ({ id: item.id, name: item.name }))),
    ),
    query: lazy(async (input: Json) => {
      const result = await callRPC("websearch.execute", {
        providerID: input.providerID,
        query: input.query,
      })
      return apiResponse(result)
    }),
  })
  const worktree = local("worktrees", "worktree", {}, false)
  const vcs = local("vcs", "vcs")
  const reference = local("references", "reference")
  const integration = local("integrations", "integration")
  const artifact = local("artifacts", "artifact")
  const pluginApi: Json = { list: lazy(async () => apiResponse(pluginList())) }
  const rpc: Json = {
    register: (definition: Json, handlers: Json) => {
      const id = definition?.id
      if (typeof id !== "string") throw new Error("rpc.register requires a definition id")
      rpcs.set(id, { plugin, definition, handlers })
      const dispose = () => rpcs.delete(id)
      plugin.registrations.push(dispose)
      return effect(() => ({
        dispose: mode === "effect" ? Effect.sync(dispose) : async () => dispose(),
        events: { emit: (...args: Json[]) => notify("rpc.event", { definition: id, args }) },
        handlers,
      }))
    },
    call: lazy(async (input: Json) => {
      const entry = rpcs.get(input?.rpcID)
      if (!entry) throw new Error(`rpc not found: ${String(input?.rpcID)}`)
      const handler = entry.handlers?.[input.method]
      if (typeof handler !== "function") throw new Error(`rpc method not found: ${String(input.method)}`)
      const controller = new AbortController()
      const error = (value: Json) => Object.assign(new Error(value?.message ?? "rpc error"), { _tag: "Rpc.Error", ...value })
      return settle(handler(input.input, { signal: controller.signal, error }))
    }),
  }
  const generate: Json = { text: lazy((input: Json) => callRPC("generate.text", input)) }
  const experimental: Json = {
    terminal: { read: lazy((input: Json) => callRPC("experimental.terminal.read", input)) },
    generate,
    artifact,
  }

  return {
    app: { name: "opencode", version, channel },
    location: { directory, project: { id: project, directory, canonical: worktree } },
    options: plugin.options ?? {},
    agent,
    aisdk,
    command,
    config: api(configApi, mode, "config", false),
    credential: api({ activate: lazy((input: Json) => callRPC("credential.activate", input)) }, mode, "credential"),
    event,
    experimental,
    integration,
    mcp,
    model,
    generate,
    permission: api(permissionBase, mode, "permission", false),
    plugin: api(pluginApi, mode, "plugin"),
    provider,
    reference,
    rpc,
    session: makeSessionDomain(plugin),
    shell,
    skill,
    storage,
    tool,
    vcs,
    websearch,
    worktree,
  }
}

function makeSessionDomain(plugin: PluginRecord): Json {
  const mode = plugin.mode
  const lazy = (fn: Function) =>
    mode === "effect"
      ? (...args: Json[]) => Effect.tryPromise({ try: () => fn(...args), catch: (error: unknown) => error })
      : fn
  const call = (method: string) => lazy(async (input: Json) => callRPC(method, input))
  const voidCall = (method: string) =>
    lazy(async (input: Json) => {
      await callRPC(method, input)
      return undefined
    })
  const paged = (method: string) =>
    lazy(async (input: Json) => {
      const value = await callRPC(method, input)
      if (value && Array.isArray(value.data)) return value
      return { data: Array.isArray(value) ? value : [], cursor: {} }
    })
  const base: Json = {
    list: paged("session.list"),
    messages: paged("session.messages"),
    message: {
      list: paged("session.message.list"),
      get: call("session.message.get"),
    },
    create: call("session.create"),
    fork: call("session.fork"),
    get: call("session.get"),
    remove: voidCall("session.remove"),
    switchAgent: voidCall("session.switchAgent"),
    switchModel: voidCall("session.switchModel"),
    prompt: call("session.prompt"),
    generate: call("session.generate"),
    command: voidCall("session.command"),
    compact: call("session.compact"),
    synthetic: call("session.synthetic"),
    interrupt: lazy(async (input: Json) => {
      await callRPC("session.abort", input)
      return { interrupted: true }
    }),
    update: voidCall("session.update"),
    move: voidCall("session.move"),
    background: voidCall("session.background"),
    skill: voidCall("session.skill"),
    context: call("session.context"),
    instructions: {
      entry: {
        list: call("session.instructions.entry.list"),
        put: voidCall("session.instructions.entry.put"),
        remove: voidCall("session.instructions.entry.remove"),
      },
    },
    wait: voidCall("session.wait"),
    status: call("session.status"),
    hook: (name: string, callback: Function, options?: Json) =>
      makeRegistration(mode, registerHook(plugin, "session", name, callback, options).remove),
  }
  return api(base, mode, "session", false)
}

// ---------------------------------------------------------------- storage

function storageFile(key: string): string {
  return `${storageDir}/${encodeURIComponent(key)}.json`
}

async function storageGet(key: string): Promise<Json> {
  try {
    return JSON.parse(await Deno.readTextFile(storageFile(key)))
  } catch {
    return undefined
  }
}

async function storageSet(key: string, value: Json): Promise<void> {
  await Deno.mkdir(storageDir, { recursive: true })
  await Deno.writeTextFile(storageFile(key), JSON.stringify(value))
}

async function storageRemove(key: string): Promise<void> {
  try {
    await Deno.remove(storageFile(key))
  } catch {
    // absent
  }
}

async function storageScan(options: Json): Promise<Json> {
  const prefix = options?.prefix ?? ""
  const entries: Json[] = []
  try {
    for await (const entry of Deno.readDir(storageDir)) {
      if (!entry.isFile || !entry.name.endsWith(".json")) continue
      const key = decodeURIComponent(entry.name.slice(0, -5))
      if (!key.startsWith(prefix)) continue
      const value = await storageGet(key)
      if (value === undefined) continue
      entries.push({ key, value })
    }
  } catch {
    // no storage yet
  }
  entries.sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0))
  const after = options?.after
  const filtered = after ? entries.filter((entry) => entry.key > after) : entries
  const limit = options?.limit
  const page = limit ? filtered.slice(0, limit) : filtered
  return { entries: page, ...(limit && filtered.length > limit ? { next: page[page.length - 1]?.key } : {}) }
}

// ---------------------------------------------------------------- loading

function pluginInfo(plugin: PluginRecord): Json {
  return {
    id: plugin.id,
    source: plugin.source,
    features: plugin.features,
    state: plugin.state,
  }
}

function pluginList(): Json {
  return plugins.map(pluginInfo)
}

function domainSnapshot(): Json {
  return {
    agents: [...registry.agents.values()],
    commands: [...registry.commands.values()].map((item) => ({
      name: item.name,
      description: item.description,
      plugin: typeof item.execute === "function",
    })),
    skills: [...registry.skills.values()],
    tools: toolList(),
    providers: [...registry.providers.values()].map((item) => ({
      info: item.provider,
      models: [...item.models.values()],
      sourceConnection: item.sourceConnection,
    })),
    mcp: [...registry.mcp.entries()],
    websearch: [...registry.websearch.values()],
    websearchDefault,
    references: [...registry.references.entries()],
    integrations: [...registry.integrations.values()],
    artifacts: [...registry.artifacts.values()],
    worktrees: [...registry.worktrees.values()],
    vcs: [...registry.vcs.values()],
    defaultAgent: registry.defaultAgent,
    defaultModel: registry.defaultModel,
    config: initialState.config,
  }
}

async function importCandidate(candidates: string[]): Promise<Json> {
  const errors: string[] = []
  for (const specifier of candidates) {
    try {
      return await import(specifier)
    } catch (error) {
      errors.push(`${specifier}: ${error instanceof Error ? error.message : String(error)}`)
    }
  }
  throw new Error(`could not import plugin (${errors.join("; ")})`)
}

async function loadPlugin(entry: Json): Promise<PluginRecord> {
  const record: PluginRecord = {
    id: entry.id ?? entry.spec,
    spec: entry.spec,
    source: entry.source ?? { type: "package", target: entry.spec },
    options: entry.options ?? {},
    mode: "promise",
    features: { server: true },
    state: { status: "active" },
    directTools: new Map(),
    registrations: [],
  }
  try {
    const candidates: string[] = entry.candidates ?? []
    const module = await importCandidate(candidates)
    record.module = module
    const value = module.default
    if (value && typeof value === "object" && typeof value.id === "string" && value.id.trim()) {
      record.id = value.id.trim()
    }
    if (value && typeof value === "object" && typeof value.effect === "function") {
      record.mode = "effect"
      const context = makeDomains(record)
      // Keep the plugin's Scope alive for the life of the host so scoped forks
      // and registrations survive setup.
      const scope = Effect.runSync(Scope.make())
      record.scope = scope
      await Effect.runPromise(value.effect(context).pipe(Effect.provideService(Scope.Scope, scope)))
      record.hooks = {}
    } else if (value && typeof value === "object" && typeof value.setup === "function") {
      record.mode = "promise"
      const context = makeDomains(record)
      const cleanup = await value.setup(context)
      if (typeof cleanup === "function") record.cleanup = cleanup
      record.hooks = {}
    } else {
      record.mode = "legacy"
      let exported: Json[] = []
      if (typeof value === "function") exported = [value]
      else if (value && typeof value === "object" && typeof value.server === "function") exported = [value.server]
      else
        exported = Object.entries(module)
          .filter(([name]) => name !== "default")
          .map(([, item]) => (typeof item === "function" ? item : item && typeof item === "object" && typeof item.server === "function" ? item.server : undefined))
          .filter((item) => typeof item === "function")
      if (!exported.length) throw new Error("plugin exports no functions")
      record.hooks = {}
      const input = legacyInput()
      for (const fn of exported) {
        const hooks = await fn(input, record.options)
        if (!hooks || typeof hooks !== "object") continue
        record.hooks = { ...record.hooks, ...hooks }
        registerLegacyHooks(record, hooks)
      }
    }
    log("info", `loaded plugin ${record.id}`)
  } catch (error) {
    record.state = {
      status: "failed",
      error: error instanceof Error ? error.message : String(error),
    }
    for (const dispose of record.registrations.splice(0)) {
      try {
        dispose()
      } catch {
        // ignore partial registrations
      }
    }
    if (record.scope) {
      try {
        await Effect.runPromise(Scope.close(record.scope, Exit.void))
      } catch {
        // ignore
      }
      record.scope = undefined
    }
    log("error", `failed to load plugin ${record.spec}: ${record.state.error}`)
  }
  plugins.push(record)
  return record
}

for (const domain of ["agents", "commands", "skills", "tools", "providers", "mcp", "websearch", "references", "integrations", "artifacts", "worktrees", "vcs"]) {
  transforms.set(domain, [])
}

const ready = (async () => {
  for (const entry of config.plugins ?? []) {
    await loadPlugin(entry)
  }
  // Legacy config hooks run before transforms so they can adjust the raw
  // configuration used by later state consumers.
  for (const plugin of plugins) {
    const hook = plugin.hooks?.config
    if (typeof hook !== "function") continue
    try {
      await settle(hook(initialState.config ?? {}))
    } catch (error) {
      log("error", `plugin ${plugin.id} config hook failed: ${String(error)}`)
    }
  }
  rebuild()
  initializing = false
  return {
    plugins: pluginList(),
    state: domainSnapshot(),
    errors: plugins
      .filter((plugin) => plugin.state.status === "failed")
      .map((plugin) => ({ id: plugin.id, error: plugin.state.error })),
  }
})()

async function shutdown() {
  for (const plugin of plugins) {
    for (const dispose of plugin.registrations.splice(0)) {
      try {
        dispose()
      } catch {
        // ignore
      }
    }
    if (plugin.cleanup) {
      try {
        await settle(plugin.cleanup())
      } catch {
        // ignore
      }
    }
    if (plugin.scope) {
      try {
        await Effect.runPromise(Scope.close(plugin.scope, Exit.void))
      } catch {
        // ignore
      }
    }
  }
}

void readLoop()
