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
const shellProgram: string = config.shell ?? ""
let bunShellCwd = directory
let bunShellEnv: Record<string, string> = {}

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
    case "tui.command.execute":
      return await executeTuiCommand(params)
    case "tui.slots":
      return await renderTuiSlots(params.input ?? {})
    case "tui.page": {
      const page = tuiPages.get(String(params.id))
      if (!page) throw new Error(`TUI page not found: ${String(params.id)}`)
      const rendered = await settle(page.render({ data: params.data }))
      return { title: page.name, text: jsxText(rendered), layout: jsxLayout(rendered) }
    }
    case "integration.list":
      return projectIntegrations()
    case "integration.get": {
      const id = String(params.integrationID ?? params.id)
      if (!registry.integrations.has(id) && !integrationMethods.has(id)) throw new Error(`Integration not found: ${id}`)
      return projectIntegration(id)
    }
    case "integration.connect.key":
      return integrationConnectKey(params)
    case "integration.connect.external":
      return integrationConnectExternal(params)
    case "integration.oauth.connect":
      return await integrationOauthConnect(params)
    case "integration.oauth.status":
      return attemptResult(findAttempt(params))
    case "integration.oauth.complete":
      return await integrationOauthComplete(params)
    case "integration.oauth.cancel":
      return integrationOauthCancel(params)
    case "integration.command.connect":
      return integrationCommandConnect(params)
    case "integration.command.status":
      return attemptResult(findAttempt(params))
    case "integration.command.cancel":
      return integrationCommandCancel(params)
    case "credential.list":
      return [...credentials.values()]
    case "credential.create":
      return createStoredCredential(params)
    case "credential.update": {
      const credential = credentials.get(String(params.credentialID ?? params.id))
      if (!credential) throw new Error(`Credential not found: ${String(params.credentialID ?? params.id)}`)
      credential.label = String(params.label ?? credential.label)
      credential.time = { ...(credential.time ?? {}), updated: Date.now() }
      saveStoredCredentials()
      void emitEvent({
        id: `credential_updated_${Date.now()}`,
        type: "credential.updated",
        properties: { integrationID: credential.integrationID, credentialID: credential.id },
      })
      return {}
    }
    case "credential.activate": {
      const id = String(params.credentialID ?? params.id)
      if (!credentials.has(id)) throw new Error(`Credential not found: ${id}`)
      return activateStoredCredential(params)
    }
    case "credential.remove": {
      const id = String(params.credentialID ?? params.id)
      const credential = credentials.get(id)
      if (!credential) throw new Error(`Credential not found: ${id}`)
      credentials.delete(id)
      saveStoredCredentials()
      void emitEvent({
        id: `credential_removed_${Date.now()}`,
        type: "credential.updated",
        properties: { integrationID: credential.integrationID, credentialID: id },
      })
      return {}
    }
    case "reference.list":
      return [...registry.references.entries()].map(([name, source]) => ({
        name,
        path: source.path ?? source.repository ?? "",
        ...(source.description === undefined ? {} : { description: source.description }),
        ...(source.hidden === undefined ? {} : { hidden: source.hidden }),
        source,
      }))
    case "websearch.list":
      return {
        providers: [...registry.websearch.values()].map((item) => ({ id: item.id, name: item.name })),
        default: websearchDefault,
      }
    case "websearch.execute": {
      const providerID = params.providerID ?? websearchDefault
      const definition = typeof providerID === "string" ? registry.websearch.get(providerID) : undefined
      if (!definition || typeof definition.execute !== "function") {
        return await request("websearch.builtin", {
          providerID: typeof providerID === "string" ? providerID : undefined,
          query: params.query,
        })
      }
      const controller = new AbortController()
      const results = await settle(definition.execute({ query: params.query }, { signal: controller.signal }))
      return { providerID, results }
    }
    case "flush":
      await eventQueue
      return {}
    case "state.reload":
      await reloadDomain(params.domain)
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
    } else if (message.method === "tool.abort") {
      abortControllers.get(message.params?.callID)?.abort()
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
const tuiPlugins: PluginRecord[] = []
const tuiCommands = new Map<string, Json>()
const tuiAliases = new Map<string, string>()
const tuiSlots: Array<{ placement: string; render: Function; plugin: string }> = []
const tuiPages = new Map<string, { name: string; render: Function; plugin: string }>()
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
let vcsDefault: string | undefined
let artifactDefault: string | false | undefined
const namespaces = new Map<string, Json>()
const integrationMethods = new Map<string, Map<string, Json>>()
const credentials = new Map<string, Json>()
const integrationAttempts = new Map<string, Json>()
const connectionStatuses = new Map<string, Json>()

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

let initialState: Json = config.state ?? {}
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
    for (const type of [
      "agent.updated",
      "model.updated",
      "provider.updated",
      "skill.updated",
      "command.updated",
      "reference.updated",
      "integration.updated",
      "websearch.updated",
      "plugin.updated",
    ]) {
      void emitEvent({ id: `${type}_${Date.now()}`, type, properties: {} })
    }
  })
}

async function reloadDomain(domain?: string) {
  const fresh = await request("client.call", { path: "state.snapshot", args: [] }).catch(() => undefined)
  if (fresh) initialState = fresh
  rebuild(domain)
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
        namespace: (definition: Json) => {
          const name = definition?.name
          if (typeof name === "string") namespaces.set(name, definition)
        },
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
          integrationMethods.delete(id)
        },
        method: {
          list: (id: string) => [...(integrationMethods.get(id)?.values() ?? [])].map((item) => item.method),
          update: (input: Json) => {
            const id = input?.integrationID
            const method = input?.method
            if (typeof id !== "string" || !method || typeof method !== "object") return
            if (!target.has(id)) target.set(id, { id, name: id })
            const methods = integrationMethods.get(id) ?? new Map<string, Json>()
            methods.set(method.id ?? method.type, input)
            integrationMethods.set(id, methods)
          },
          remove: (id: string, method: Json) => {
            const methods = integrationMethods.get(id)
            methods?.delete(method?.id ?? method?.type)
          },
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
    case "vcs":
      return {
        list: () => [...target.values()],
        get: (id: string) => target.get(id),
        add: (definition: Json) => {
          const id = definition.id ?? definition.name
          target.set(id, { ...definition, id })
          vcsDefault = id
        },
        remove: (id: string) => {
          target.delete(id)
          if (vcsDefault === id) vcsDefault = undefined
        },
        update: (id: string, update: (value: Json) => void) => {
          const value = target.get(id)
          if (value) update(value)
        },
        default: {
          get: () => vcsDefault,
          set: (id: string) => {
            vcsDefault = id
          },
        },
      }
    case "artifacts":
      return {
        list: () => [...target.values()],
        get: (id: string) => target.get(id),
        add: (definition: Json) => {
          const id = definition.id ?? definition.name
          target.set(id, { ...definition, id })
          artifactDefault = id
        },
        remove: (id: string) => {
          target.delete(id)
          if (artifactDefault === id) artifactDefault = undefined
        },
        update: (id: string, update: (value: Json) => void) => {
          const value = target.get(id)
          if (value) update(value)
        },
        default: {
          get: () => artifactDefault,
          set: (id: string | false | undefined) => {
            artifactDefault = id
          },
        },
      }
    case "worktrees":
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

function fileData(part: Json): string {
  if (typeof part.data === "string") return part.data
  const url = typeof part.url === "string" ? part.url : ""
  return url.split("base64,")[1] ?? ""
}

function toV2Messages(messages: Json): Json[] {
  return (Array.isArray(messages) ? messages : []).map((message: Json) => {
    const info = message.info ?? {}
    const role = info.role ?? "user"
    const content: Json[] = []
    for (const part of message.parts ?? []) {
      if (part.type === "text") content.push({ type: "text", text: part.text })
      else if (part.type === "reasoning") content.push({ type: "reasoning", text: part.text })
      else if (part.type === "file") {
        content.push({
          type: "media",
          media: {
            source: { type: "base64", data: fileData(part), mediaType: part.mime },
          },
          filename: part.filename,
        })
      } else if (part.type === "tool") {
        content.push({
          type: "tool-call",
          id: part.callID,
          name: part.tool,
          input: part.state?.input ?? part.input ?? {},
        })
        if (part.state?.status === "completed") {
          content.push({
            type: "tool-result",
            id: part.callID,
            name: part.tool,
            result: { type: "text", value: part.state.output ?? "" },
          })
        } else if (part.state?.status === "error") {
          content.push({
            type: "tool-result",
            id: part.callID,
            name: part.tool,
            result: { type: "error", value: part.state.error ?? "" },
          })
        }
      } else if (part.type === "compaction") {
        content.push({ type: "compaction", summary: part.summary ?? "" })
      }
    }
    return { id: info.id, role, content }
  })
}

/**
 * Project a plugin's V2 `{ role, content }` messages back onto the host's
 * `{ info, parts }` shape. Plugins may add, remove, reorder or replace whole
 * messages, so this rebuilds the list instead of editing entries in place:
 * matching is by message id where the plugin kept one, otherwise by position.
 */
function applyV2Messages(event: Json, messages: Json[]) {
  const original = Array.isArray(event.messages) ? event.messages : []
  const byID = new Map<string, Json>()
  for (const message of original) {
    const id = message?.info?.id
    if (typeof id === "string") byID.set(id, message)
  }
  const result: Json[] = []
  const tools = new Map<string, Json>()
  for (let index = 0; index < messages.length; index++) {
    const message = messages[index] ?? {}
    const content = Array.isArray(message.content) ? message.content : []
    if (message.role === "tool") {
      // A host tool-result carrier: attach it to the call it answers.
      for (const part of content) {
        if (part?.type !== "tool-result") continue
        const target = tools.get(String(part.id))
        if (!target) continue
        const state = target.state ?? (target.state = {})
        if (part.result?.type === "error") {
          state.status = "error"
          state.error = String(part.result.value ?? "")
        } else {
          state.status = "completed"
          state.output = String(part.result?.value ?? "")
        }
      }
      continue
    }
    const found = typeof message.id === "string" ? byID.get(message.id) : undefined
    // An id the host did not send belongs to a message the plugin added; only
    // fall back to position when the plugin omitted the id entirely.
    const source = found ?? (message.id ? undefined : original[index])
    const info = source?.info ? { ...source.info } : { id: message.id, role: message.role }
    if (message.role) info.role = message.role
    if (!info.id && message.id) info.id = message.id
    const parts: Json[] = []
    for (const part of content) {
      if (part?.type === "text") {
        parts.push({ type: "text", text: part.text ?? "", synthetic: part.synthetic === true })
      } else if (part?.type === "reasoning") {
        const previous = (source?.parts ?? []).find((item: Json) => item.type === "reasoning" && item.text === part.text)
        parts.push({ ...(previous ?? {}), type: "reasoning", text: part.text ?? "" })
      } else if (part?.type === "tool-call") {
        const previous = (source?.parts ?? []).find((item: Json) => item.type === "tool" && item.callID === part.id)
        const tool: Json = {
          ...(previous ?? {}),
          type: "tool",
          callID: part.id,
          tool: part.name,
          state: {
            ...(previous?.state ?? {}),
            status: "pending",
            input: part.input ?? previous?.state?.input ?? {},
          },
        }
        tools.set(String(part.id), tool)
        parts.push(tool)
      } else if (part?.type === "tool-result") {
        const target = tools.get(String(part.id))
        if (!target) continue
        const state = target.state ?? (target.state = {})
        if (part.result?.type === "error") {
          state.status = "error"
          state.error = String(part.result.value ?? "")
        } else {
          state.status = "completed"
          state.output = String(part.result?.value ?? "")
        }
      } else if (part?.type === "media") {
        const media = part.media ?? {}
        const source = media.source ?? {}
        parts.push({
          type: "file",
          mime: source.mediaType ?? media.mediaType ?? "application/octet-stream",
          data: source.data ?? "",
          filename: part.filename ?? media.filename ?? "",
        })
      } else if (part?.type === "compaction") {
        parts.push({ type: "compaction", summary: part.summary ?? "" })
      }
    }
    result.push({ info, parts })
  }
  event.messages = result
}

const V2_MESSAGE_HOOKS = new Set(["context", "title", "compaction", "generate"])

function wrapHttpEvent(domain: string, name: string, event: Json): boolean {
  if (domain !== "session") return false
  if (name === "http.request") {
    const request = event.request ?? {}
    const body =
      request.body === undefined
        ? undefined
        : typeof request.body === "string"
          ? request.body
          : JSON.stringify(request.body)
    event.request = new Request(request.url ?? "http://localhost/", {
      method: request.method ?? "POST",
      headers: request.headers ?? {},
      body,
    })
    return true
  }
  if (name === "http.response") {
    const response = event.response ?? {}
    const body =
      response.body === undefined || response.body === null
        ? null
        : typeof response.body === "string"
          ? response.body
          : JSON.stringify(response.body)
    event.response = new Response(body, {
      status: response.status ?? 200,
      headers: response.headers ?? {},
    })
    return true
  }
  return false
}

async function unwrapHttpEvent(domain: string, name: string, event: Json): Promise<void> {
  if (domain !== "session") return
  if (name === "http.request" && event.request instanceof Request) {
    const request = event.request
    const text = await request.clone().text()
    let body: Json = undefined
    if (text) {
      try {
        body = JSON.parse(text)
      } catch {
        body = text
      }
    }
    event.request = {
      url: request.url,
      method: request.method,
      headers: Object.fromEntries(request.headers),
      ...(body === undefined ? {} : { body }),
    }
  }
  if (name === "http.response" && event.response instanceof Response) {
    const response = event.response
    const text = await response.clone().text()
    let body: Json = undefined
    if (text) {
      try {
        body = JSON.parse(text)
      } catch {
        body = text
      }
    }
    event.response = {
      status: response.status,
      headers: Object.fromEntries(response.headers),
      ...(body === undefined ? {} : { body }),
    }
  }
}

async function trigger(domain: string, name: string, event: Json): Promise<Json> {
  const key = `${domain}.${name}`
  const list = hooks.get(key) ?? []
  const messageHook = domain === "session" && V2_MESSAGE_HOOKS.has(name)
  const httpEvent = list.length > 0 && wrapHttpEvent(domain, name, event)
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
  if (httpEvent) await unwrapHttpEvent(domain, name, event)
  return event
}

const sessionTitles = new Map<string, string>()
const sessionMetadata = new Map<string, Json>()

function toV2Event(event: Json): Json | null {
  if (event.type === "session.usage.recorded") return null
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
    } else {
      return null
    }
  } else if (event.type === "session.idle") {
    data = { sessionID: properties.sessionID }
  } else if (event.type === "session.status") {
    data = { sessionID: properties.sessionID, status: properties.status ?? { type: "busy" } }
  } else if (event.type === "session.updated") {
    const info = properties.info ?? {}
    const sessionID = properties.sessionID ?? info.id
    if (typeof info.title === "string" && sessionTitles.get(sessionID) !== info.title) {
      sessionTitles.set(sessionID, info.title)
      type = "session.renamed"
      data = { sessionID, title: info.title }
    } else if (info.metadata !== undefined && JSON.stringify(sessionMetadata.get(sessionID)) !== JSON.stringify(info.metadata)) {
      sessionMetadata.set(sessionID, info.metadata)
      type = "session.metadata.updated"
      data = { sessionID, metadata: info.metadata }
    } else {
      return null
    }
  } else if (event.type === "session.renamed") {
    sessionTitles.set(properties.sessionID, properties.title)
    data = { sessionID: properties.sessionID, title: properties.title }
  } else if (event.type === "session.metadata.updated") {
    sessionMetadata.set(properties.sessionID, properties.metadata)
    data = { sessionID: properties.sessionID, metadata: properties.metadata }
  } else if (event.type === "session.created") {
    const info = properties.info ?? {}
    const sessionID = info.id ?? properties.sessionID
    if (typeof info.title === "string") sessionTitles.set(sessionID, info.title)
    if (info.metadata !== undefined) sessionMetadata.set(sessionID, info.metadata)
    data = {
      sessionID,
      projectID: info.projectID,
      location: info.location ?? { directory: directory },
      parentID: info.parentID ?? undefined,
      slug: info.id ?? properties.sessionID,
      title: info.title,
      agent: info.agent,
      model: info.model,
      metadata: info.metadata,
      permissions: info.permissions,
      subpath: info.subpath,
      version,
    }
  } else if (event.type === "session.error") {
    type = "session.execution.failed"
    data = {
      sessionID: properties.sessionID,
      error: properties.error ?? { type: "unknown", message: properties.message ?? "" },
    }
  } else if (event.type === "message.updated") {
    const info = properties.info ?? {}
    const content = toV2Messages([{ info, parts: properties.parts ?? [] }])[0]?.content ?? []
    type = "session.message.content.updated"
    data = {
      sessionID: properties.sessionID ?? info.sessionID,
      messageID: info.id,
      content,
    }
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
    return null
  } else if (event.type === "permission.replied") {
    data = {
      sessionID: properties.sessionID,
      requestID: properties.requestID,
      reply: properties.reply ?? "once",
    }
  } else if (event.type === "server.notice") {
    return null
  }
  return {
    id: event.id ?? `evt_${Date.now()}`,
    created: event.created ?? properties.created ?? Date.now(),
    type,
    data,
    ...(event.durable === undefined ? {} : { durable: event.durable }),
    ...(event.location === undefined ? {} : { location: event.location }),
    ...(event.metadata === undefined ? {} : { metadata: event.metadata }),
  }
}

const activeStreams = new Map<string, { text: boolean; reasoning: boolean }>()

function streamEvents(v2: Json): Json[] {
  const data = v2.data ?? {}
  if (v2.type === "session.text.delta" || v2.type === "session.reasoning.delta") {
    const key = `${data.sessionID}:${data.assistantMessageID}`
    const state = activeStreams.get(key) ?? { text: false, reasoning: false }
    const kind = v2.type === "session.text.delta" ? "text" : "reasoning"
    const out: Json[] = []
    if (!state[kind]) {
      state[kind] = true
      activeStreams.set(key, state)
      out.push({
        id: `${v2.id}_${kind}_started`,
        created: v2.created,
        type: `session.${kind}.started`,
        data: { sessionID: data.sessionID, assistantMessageID: data.assistantMessageID, ordinal: data.ordinal ?? 0 },
      })
    }
    out.push(v2)
    return out
  }
  if (v2.type === "session.message.content.updated") {
    const out = [v2]
    const prefix = `${data.sessionID}:`
    const text = (Array.isArray(data.content) ? data.content : [])
      .filter((part: Json) => part?.type === "text")
      .map((part: Json) => part.text ?? "")
      .join("")
    const reasoning = (Array.isArray(data.content) ? data.content : [])
      .filter((part: Json) => part?.type === "reasoning")
      .map((part: Json) => part.text ?? "")
      .join("")
    for (const [key, state] of activeStreams) {
      if (!key.startsWith(prefix)) continue
      if (state.text) {
        out.push({
          id: `${v2.id}_text_ended`,
          created: v2.created,
          type: "session.text.ended",
          data: { sessionID: data.sessionID, assistantMessageID: data.messageID, ordinal: 0, text },
        })
      }
      if (state.reasoning) {
        out.push({
          id: `${v2.id}_reasoning_ended`,
          created: v2.created,
          type: "session.reasoning.ended",
          data: { sessionID: data.sessionID, assistantMessageID: data.messageID, ordinal: 0, text: reasoning },
        })
      }
      activeStreams.delete(key)
    }
    return out
  }
  if (v2.type === "session.idle") {
    return [
      { id: `${v2.id}_status`, created: v2.created, type: "session.status", data: { sessionID: data.sessionID, status: { type: "idle" } } },
      v2,
    ]
  }
  return [v2]
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
  const converted = toV2Event(event)
  if (!converted) return
  for (const v2 of streamEvents(converted)) {
    for (const subscriber of subscribers) {
      if (subscriber.done) continue
      if (subscriber.types && !subscriber.types.has(v2.type)) continue
      subscriber.queue.push(v2)
      subscriber.wake?.()
    }
    notify("v2.event", { event: v2 })
  }
}

type Subscriber = {
  types?: Set<string>
  queue: Json[]
  wake?: () => void
  done: boolean
}
const subscribers = new Set<Subscriber>()
const KNOWN_SESSION_EVENTS = new Set([
  "session.created",
  "session.agent.selected",
  "session.model.selected",
  "session.moved",
  "session.renamed",
  "session.metadata.updated",
  "session.permissions",
  "session.viewed",
  "session.archived",
  "session.unarchived",
  "session.message.content.updated",
  "session.usage.recorded",
  "session.usage.updated",
  "session.deleted",
  "session.forked",
  "session.inbox.delivered",
  "session.inbox.enqueued",
  "session.inbox.cancelled",
  "session.inbox.delivery.changed",
  "session.execution.started",
  "session.execution.succeeded",
  "session.execution.failed",
  "session.execution.interrupted",
  "session.instructions.updated",
  "session.synthetic",
  "session.skill.activated",
  "session.shell.started",
  "session.shell.ended",
  "session.step.started",
  "session.step.streamed",
  "session.step.ended",
  "session.step.failed",
  "session.text.started",
  "session.text.delta",
  "session.text.ended",
  "session.reasoning.started",
  "session.reasoning.delta",
  "session.reasoning.ended",
  "session.tool.input.started",
  "session.tool.input.delta",
  "session.tool.input.ended",
  "session.tool.called",
  "session.tool.progress",
  "session.tool.success",
  "session.tool.failed",
  "session.retry.scheduled",
  "session.compaction.started",
  "session.compaction.delta",
  "session.compaction.ended",
  "session.compaction.failed",
  "session.revert.staged",
  "session.revert.cleared",
  "session.revert.committed",
  "session.status",
  "session.idle",
])

function subscribeEvents(options?: { types?: string[]; signal?: AbortSignal }): AsyncIterable<Json> {
  for (const type of options?.types ?? []) {
    if (type.startsWith("session.") && !KNOWN_SESSION_EVENTS.has(type)) {
      log("warn", `unknown event type: ${type}`)
    }
  }
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
        void emitEvent({
          id: `tool_progress_${Date.now()}`,
          type: "session.tool.progress",
          properties: {
            sessionID: params.context?.sessionID,
            assistantMessageID: params.context?.messageID,
            id: params.callID,
            name: params.name,
            metadata: input?.metadata ?? input,
          },
        })
      },
      progress: (update: Json) => {
        notify("tool.metadata", { callID: params.callID, metadata: update })
        void emitEvent({
          id: `tool_progress_${Date.now()}`,
          type: "session.tool.progress",
          properties: {
            sessionID: params.context?.sessionID,
            assistantMessageID: params.context?.messageID,
            id: params.callID,
            name: params.name,
            metadata: update,
          },
        })
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

function slug(value: string): string {
  return value.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "command"
}

function tuiCommandList(): Json[] {
  return [...tuiCommands.values()].map((command) => ({
    id: command.id,
    title: command.title,
    description: command.description,
    group: command.group,
    bind: command.bind,
    slash: command.slash,
  }))
}

function tuiPageList(): Json[] {
  return [...tuiPages.entries()].map(([id, page]) => ({ id, name: page.name, plugin: page.plugin }))
}

function registerTuiCommand(plugin: PluginRecord, command: Json): () => void {
  const title = typeof command.title === "string" && command.title.trim() ? command.title.trim() : "Plugin command"
  const id = typeof command.id === "string" && command.id.trim() ? command.id.trim() : `tui.${plugin.id}.${slug(title)}`
  const entry = {
    id,
    title,
    description: command.description,
    group: command.group ?? "Plugin",
    bind: command.bind,
    slash: command.slash,
    run: command.run,
    plugin: plugin.id,
  }
  tuiCommands.set(id, entry)
  const slash = command.slash
  if (slash && typeof slash.name === "string") {
    tuiAliases.set(slash.name, id)
    for (const alias of slash.aliases ?? []) tuiAliases.set(String(alias), id)
  }
  const dispose = () => {
    tuiCommands.delete(id)
    for (const [name, target] of tuiAliases) if (target === id) tuiAliases.delete(name)
    scheduleStateChanged()
  }
  plugin.registrations.push(dispose)
  scheduleStateChanged()
  return dispose
}

function jsxText(value: Json): string {
  if (value === null || value === undefined || value === false || value === true) return ""
  if (typeof value === "string" || typeof value === "number" || typeof value === "bigint") return String(value)
  if (Array.isArray(value)) return value.map(jsxText).filter(Boolean).join("")
  if (typeof value === "function") {
    try {
      return jsxText(value())
    } catch {
      return ""
    }
  }
  if (typeof value === "object") {
    if (value.text !== undefined) return jsxText(value.text)
    if (value.children !== undefined) return jsxText(value.children)
    if (value.props) {
      const props = value.props
      if (typeof props === "string") return props
      if (props.children !== undefined) return jsxText(props.children)
      if (props.text !== undefined) return jsxText(props.text)
      if (props.content !== undefined) return jsxText(props.content)
    }
  }
  return ""
}

function jsxLayout(value: Json, depth = 0, budget = { nodes: 0 }): Json | null {
  if (depth > 8 || budget.nodes > 200) return null
  if (value === null || value === undefined || value === false || value === true) return null
  if (typeof value === "string" || typeof value === "number" || typeof value === "bigint") {
    const text = String(value)
    return text ? { type: "text", text } : null
  }
  if (Array.isArray(value)) {
    const children = value.map((item) => jsxLayout(item, depth + 1, budget)).filter(Boolean)
    return children.length ? { type: "box", direction: "column", children } : null
  }
  if (typeof value === "function") {
    try {
      return jsxLayout(value(), depth + 1, budget)
    } catch {
      return null
    }
  }
  if (typeof value !== "object") return null
  const props = value.props ?? {}
  const children = props.children !== undefined ? props.children : value.children
  const type = typeof value.type === "string" ? value.type.toLowerCase() : ""
  if (type.includes("text") || type === "span" || type === "b" || type === "i" || type === "strong" || type === "em") {
    const text = jsxText(value)
    if (!text.trim()) return null
    budget.nodes += 1
    return {
      type: "text",
      text,
      ...(type === "b" || type === "strong" ? { bold: true } : {}),
      ...(type === "i" || type === "em" ? { italic: true } : {}),
      ...(props.fg ? { color: String(props.fg) } : {}),
      ...(props.dim ? { dim: true } : {}),
    }
  }
  if (children === undefined) {
    const text = jsxText(value)
    if (!text.trim()) return null
    budget.nodes += 1
    return { type: "text", text }
  }
  budget.nodes += 1
  const direction = String(props.flexDirection ?? props.direction ?? "column").toLowerCase() === "row" ? "row" : "column"
  const nested = jsxLayout(children, depth + 1, budget)
  const items = nested ? (nested.type === "box" && nested.direction === direction ? nested.children : [nested]) : []
  return { type: "box", direction, children: items }
}

async function renderTuiSlots(input: Json): Promise<Json[]> {
  const out: Json[] = []
  for (const slot of [...tuiSlots]) {
    try {
      const rendered = await settle(slot.render(input))
      const text = jsxText(rendered).trim()
      if (text) out.push({ placement: slot.placement, text, layout: jsxLayout(rendered), plugin: slot.plugin })
    } catch (error) {
      log("error", `tui slot ${slot.placement} failed: ${String(error)}`)
    }
  }
  return out
}

function makeTuiContext(plugin: PluginRecord): Json {
  const prefix = `tui:${plugin.id}:`
  const memory = new Map<string, Json>()
  const storage = {
    store: (key: string, options: Json) => {
      let state: Json
      try {
        state = JSON.parse(Deno.readTextFileSync(storageFile(`${prefix}${key}`)))
      } catch {
        state = structuredClone(options?.initial ?? {})
      }
      return [
        state,
        async (mutation: Function) => {
          mutation(state)
          await storageSet(`${prefix}${key}`, state)
        },
      ]
    },
    memory: (key: string, options: Json) => {
      const state = memory.get(key) ?? structuredClone(options?.initial ?? {})
      memory.set(key, state)
      return [state, (mutation: Function) => mutation(state)]
    },
  }
  const noCleanup = () => () => {}
  const keymap = {
    layer: (factory: Function) => {
      try {
        const layer = factory()
        for (const command of layer?.commands ?? []) registerTuiCommand(plugin, command)
      } catch (error) {
        log("error", `tui plugin ${plugin.id} keymap layer failed: ${String(error)}`)
      }
    },
    dispatch: (id: string, input?: string) => {
      const command = tuiCommands.get(id)
      if (command?.run) void settle(command.run(input))
    },
    shortcuts: () => [],
    commands: () => [...tuiCommands.values()],
    pending: () => [],
    active: () => [],
    mode: { current: () => "base", push: () => () => {} },
  }
  const toast = {
    show: (options: Json) => {
      void request("tui.toast", {
        title: options?.title,
        message: options?.message ?? "",
        variant: options?.variant ?? "info",
      }).catch((error: unknown) => log("error", `tui toast failed: ${String(error)}`))
    },
  }
  const dialog = {
    show: () => {},
    set: () => {},
    close: () => {},
    alert: async (options: Json) => {
      await request("tui.dialog", { type: "alert", title: options?.title, message: options?.message })
      return undefined
    },
    confirm: async (options: Json) => {
      const result = await request("tui.dialog", {
        type: "confirm",
        title: options?.title,
        message: options?.message,
        value: options?.default ?? true,
      })
      return typeof result?.value === "boolean" ? result.value : undefined
    },
    prompt: async (options: Json) => {
      const result = await request("tui.dialog", {
        type: "prompt",
        title: options?.title,
        message: options?.message,
        value: options?.default ?? "",
        placeholder: options?.placeholder,
      })
      return typeof result?.value === "string" ? result.value : undefined
    },
    select: async (options: Json) => {
      const values = (options?.options ?? []).map((item: Json) => item?.value ?? item?.label)
      const result = await request("tui.dialog", {
        type: "select",
        title: options?.title,
        message: options?.message,
        value: options?.initial ?? values[0],
        options: values,
      })
      return result?.value
    },
  }
  const ui = {
    dialog,
    toast,
    format: { path: (value: string) => value },
    router: {
      register: (page: Json) => {
        if (typeof page?.name !== "string" || typeof page?.render !== "function") return () => {}
        const entry = { name: page.name, render: page.render as Function, plugin: plugin.id }
        tuiPages.set(page.name, entry)
        const dispose = () => tuiPages.delete(page.name)
        plugin.registrations.push(dispose)
        scheduleStateChanged()
        return dispose
      },
      navigate: (destination: Json) => {
        if (destination?.type !== "plugin") return
        const id = destination.id ?? destination.name
        if (typeof id !== "string") return
        void request("tui.navigate", { id, data: destination.data }).catch((error: unknown) =>
          log("error", `tui navigate failed: ${String(error)}`),
        )
      },
      current: () => ({ type: "home" }),
    },
    panel: {
      open: (name: string, options: Json) => {
        void request("tui.panel", {
          name,
          presentation: options?.presentation ?? "panel",
        }).catch((error: unknown) => log("error", `tui panel failed: ${String(error)}`))
        return true
      },
      close: () => {},
      current: () => undefined,
    },
    tabs: {
      enabled: () => false,
      list: () => [],
      open: () => false,
      focus: () => false,
      move: () => false,
      close: () => false,
    },
    model: {
      current: () => undefined,
      variant: { list: () => [], set: () => false },
    },
    slot: (claim: Json) => {
      const placement = claim?.prepend ?? claim?.append ?? claim?.before ?? claim?.after ?? claim?.replace
      if (typeof placement !== "string" || typeof claim?.render !== "function") return () => {}
      const entry = { placement, render: claim.render as Function, plugin: plugin.id }
      tuiSlots.push(entry)
      const dispose = () => {
        const index = tuiSlots.indexOf(entry)
        if (index >= 0) tuiSlots.splice(index, 1)
      }
      plugin.registrations.push(dispose)
      return dispose
    },
  }
  return {
    options: plugin.options,
    location: { directory, worktree, project },
    app: { version: config.version, channel: config.channel, platform: Deno.build.os },
    renderer: {},
    client: makeClientProxy(),
    data: {
      on: () => () => {},
      listen: () => () => {},
      session: {
        list: () => [],
        get: () => undefined,
        root: (id: string) => id,
        family: (id: string) => [id],
        cost: () => 0,
        status: () => "idle",
        pending: { list: () => [], sync: async () => {}, invalidate: () => {} },
        sync: async () => {},
        invalidate: () => {},
        message: { list: () => [], get: () => undefined, sync: async () => {}, invalidate: () => {} },
        permission: { list: () => [], sync: async () => {}, invalidate: () => {} },
        form: {
          list: () => [],
          sync: async () => {},
          invalidate: () => {},
          reply: async () => {},
          cancel: async () => {},
        },
      },
      project: {
        list: () => [],
        get: () => undefined,
        sync: async () => {},
        invalidate: () => {},
        permission: { list: () => [], sync: async () => {}, invalidate: () => {} },
      },
      shell: { list: () => [], get: () => undefined, sync: async () => {}, invalidate: () => {} },
      location: { default: () => ({ directory }) },
    },
    attention: {},
    theme: {},
    themeMode: "dark",
    markdown: { registerCodeBlockRenderer: noCleanup },
    keymap,
    storage,
    ui,
  }
}

async function loadTuiPlugin(entry: Json): Promise<void> {
  const record: PluginRecord = {
    id: entry.id ?? entry.spec,
    spec: entry.spec,
    source: entry.source ?? { type: "package", target: entry.spec },
    options: entry.options ?? {},
    mode: "promise",
    features: { server: false, tui: true },
    state: { status: "active" },
    directTools: new Map(),
    registrations: [],
  }
  try {
    const module = await importCandidate(entry.tuiCandidates ?? [])
    const value = module.default ?? module
    const setup = typeof value === "function" ? value : value?.setup
    if (typeof setup !== "function") throw new Error("TUI entry exports no setup")
    if (value && typeof value === "object" && typeof value.id === "string" && value.id.trim()) record.id = value.id.trim()
    const cleanup = await setup(makeTuiContext(record))
    if (typeof cleanup === "function") record.cleanup = cleanup
    log("info", `loaded tui plugin ${record.id}`)
  } catch (error) {
    record.state = { status: "failed", error: error instanceof Error ? error.message : String(error) }
    log("error", `failed to load tui plugin ${record.spec}: ${record.state.error}`)
  }
  tuiPlugins.push(record)
}

async function executeTuiCommand(params: Json): Promise<Json> {
  const name = String(params.name ?? "")
  const id = tuiCommands.has(name) ? name : tuiAliases.get(name)
  const command = id ? tuiCommands.get(id) : undefined
  if (!command) throw new Error(`TUI command not found: ${name}`)
  if (typeof command.run !== "function") return {}
  return (await settle(command.run(params.input, undefined))) ?? {}
}

async function executePluginCommand(params: Json): Promise<Json> {
  const command = registry.commands.get(params.name)
  if (!command) throw new Error(`plugin command not found: ${params.name}`)
  if (typeof command.execute !== "function") return {}
  const invocation = {
    sessionID: params.sessionID,
    prompt: params.prompt ?? { text: params.arguments ?? "" },
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
    if (path === "config.get") return initialState.config ?? {}
    if (path === "event.subscribe") return { stream: subscribeEvents() }
    if (path === "app.log") {
      log("info", args[0]?.message ?? "", args[0])
      return undefined
    }
    const result = await request("client.call", { path, args })
    // V1 SDK methods return their payload directly, not the V2 `{ data }` envelope.
    if (result && Array.isArray(result.data) && (path === "session.list" || path === "session.messages")) {
      return result.data
    }
    return result
  }
  return new Proxy(invoke, {
    get(_target, prop) {
      if (prop === "then" || prop === "catch" || prop === "finally") return undefined
      if (typeof prop !== "string") return undefined
      if (path === "event" && prop === "subscribe") return () => ({ stream: subscribeEvents() })
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
    const state = {
      command,
      cwd: bunShellCwd,
      env: { ...bunShellEnv } as Record<string, string>,
      quiet: false,
      nothrow: false,
      started: false,
    }
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
    env: (next?: Record<string, string | undefined>) => {
      for (const [key, value] of Object.entries(next ?? {})) {
        if (value === undefined) delete bunShellEnv[key]
        else bunShellEnv[key] = value
      }
      return shell
    },
    cwd: (next?: string) => {
      if (next) bunShellCwd = next
      return shell
    },
    nothrow: () => shell,
    throws: (_value: boolean) => shell,
  })
  return shell
}

function renderShellValue(value: Json): string {
  if (value && typeof value === "object" && "raw" in value) return String(value.raw)
  if (typeof value !== "string") return String(value)
  const shell = shellProgram.toLowerCase()
  if (shell.includes("cmd.exe")) return `"${value.replaceAll('"', '""')}"`
  if (shell.includes("powershell") || shell.includes("pwsh")) return `'${value.replaceAll("'", "''")}'`
  return `'${value.replaceAll("'", "'\\''")}'`
}

// ---------------------------------------------------------------- api builder

function abortError(): Error {
  const error = new Error("request aborted")
  error.name = "AbortError"
  return error
}

function aborted(signal: AbortSignal): Promise<never> {
  return new Promise((_, reject) => {
    if (signal.aborted) return reject(abortError())
    signal.addEventListener("abort", () => reject(abortError()), { once: true })
  })
}

function callRPC(method: string, params?: Json, options?: Json): Promise<Json> {
  // The provisional Rust App is attached before initialize, so setup can read
  // live registries and sessions while transform state is still being built.
  const cancelID = options?.signal ? crypto.randomUUID() : undefined
  const result = request("client.call", {
    path: method,
    args: params === undefined ? [] : [params],
    options: cancelID ? { ...options, cancelID } : options,
  })
  if (!options?.signal || !cancelID) return result
  const cancel = () => notify("client.cancel", { cancelID })
  options.signal.addEventListener("abort", cancel, { once: true })
  return Promise.race([result, aborted(options.signal)]).finally(() => {
    options.signal.removeEventListener("abort", cancel)
  })
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
  const sync = (fn: Function) =>
    mode === "effect"
      ? (...args: Json[]) => Effect.sync(() => fn(...args))
      : (...args: Json[]) => Promise.resolve().then(() => fn(...args))
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
      reload: () => effect(() => reloadDomain(domain)),
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
      const value = registry.agents.get(id)
      if (!value) throw new Error(`agent not found: ${String(id)}`)
      return apiResponse(value)
    }),
  })
  const commandTarget = registry.commands
  const command = local("commands", "command", {
    add: (definition: Json) => commandTarget.set(definition.name, definition),
    get: sync((name: string) => commandTarget.get(name)),
    execute: lazy((input: Json) => callRPC("session.command", input)),
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
      const value = registry.providers.get(id)?.provider
      if (!value) throw new Error(`provider not found: ${String(id)}`)
      return apiResponse(value)
    }),
  })
  const modelBase: Json = {
    transform: (callback: Function) => registration(registerTransform(plugin, "model", callback)),
    reload: () => effect(() => reloadDomain("model")),
    list: (input?: Json) =>
      sync((input?: Json) => {
        const providerID = typeof input === "string" ? input : input?.providerID
        const types = Array.isArray(input?.types) ? new Set<string>(input.types) : undefined
        const out: Json[] = []
        for (const [id, record] of registry.providers) {
          if (providerID && id !== providerID) continue
          for (const model of record.models.values()) {
            if (types && !types.has(model.type ?? "chat")) continue
            out.push(model)
          }
        }
        return apiResponse(out)
      })(input),
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
    default: Object.assign(
      sync(() => {
        const ref = registry.defaultModel
        if (!ref) return apiResponse(null)
        const modelID = ref.modelID ?? ref.id
        return apiResponse(registry.providers.get(ref.providerID)?.models.get(modelID) ?? null)
      }),
      {
        get: sync(() => {
          const ref = registry.defaultModel
          if (!ref) return apiResponse(null)
          const modelID = ref.modelID ?? ref.id
          return apiResponse(registry.providers.get(ref.providerID)?.models.get(modelID) ?? null)
        }),
        set: (providerID: string, modelID: string) =>
          sync((providerID: string, modelID: string) => {
            registry.defaultModel = { providerID, modelID }
          })(providerID, modelID),
      },
    ),
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
    list: lazy(async (input: Json) => callRPC("permission.list", input)),
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
      const result = await request("websearch.execute", {
        providerID: input.providerID,
        query: input.query,
      })
      return apiResponse(result)
    }),
  })
  const vcsBase = local("vcs", "vcs", {}, false)
  const vcsDefinition = () => (vcsDefault ? registry.vcs.get(vcsDefault) : [...registry.vcs.values()].at(-1))
  const vcsCall = (method: string, envelope: boolean) =>
    lazy(async (input: Json) => {
      const definition = vcsDefinition()
      const handler = definition?.[method]
      const scoped = { directory, worktree, canonical: worktree, ...(input ?? {}) }
      const result =
        typeof handler === "function"
          ? await settle(handler(scoped, { signal: new AbortController().signal }))
          : await callRPC(`vcs.${method}`, scoped)
      return envelope ? apiResponse(result) : result
    })
  const vcs: Json = {
    ...vcsBase,
    init: vcsCall("init", false),
    get: vcsCall("info", true),
    base: vcsCall("base", true),
    status: vcsCall("status", true),
    diff: vcsCall("diff", true),
    branch: { list: vcsCall("branches", true) },
  }
  const referenceInfo = (name: string, source: Json): Json => ({
    name,
    path: source.path ?? source.repository ?? "",
    ...(source.description === undefined ? {} : { description: source.description }),
    ...(source.hidden === undefined ? {} : { hidden: source.hidden }),
    source,
  })
  const reference = local("references", "reference", {
    list: sync(() =>
      apiResponse([...registry.references.entries()].map(([name, source]) => referenceInfo(name, source))),
    ),
    get: sync((input: Json) => {
      const name = typeof input === "string" ? input : input?.name ?? input?.id
      const source = registry.references.get(name)
      return apiResponse(source ? referenceInfo(name, source) : undefined)
    }),
  })
  const integration = local("integrations", "integration", {
    list: sync(() => apiResponse(projectIntegrations())),
    get: sync((input: Json) => {
      const id = typeof input === "string" ? input : input?.integrationID ?? input?.id
      if (!registry.integrations.has(id) && !integrationMethods.has(id)) {
        throw new Error(`Integration not found: ${String(id)}`)
      }
      return apiResponse(projectIntegration(id))
    }),
    connect: {
      key: lazy((input: Json) => integrationConnectKey(input)),
      external: lazy((input: Json) => integrationConnectExternal(input)),
    },
    oauth: {
      connect: lazy(async (input: Json) => apiResponse(await integrationOauthConnect(input))),
      status: lazy(async (input: Json) => apiResponse(attemptResult(findAttempt(input)))),
      complete: lazy((input: Json) => integrationOauthComplete(input)),
      cancel: lazy((input: Json) => integrationOauthCancel(input)),
    },
    command: {
      connect: lazy(async (input: Json) => apiResponse(integrationCommandConnect(input))),
      status: lazy(async (input: Json) => apiResponse(attemptResult(findAttempt(input)))),
      cancel: lazy((input: Json) => integrationCommandCancel(input)),
    },
    connection: {
      active: lazy(async (integrationID: string) => integrationConnectionList(integrationID)[0]),
      resolve: lazy((connection: Json) => integrationResolveConnection(connection)),
      status: lazy(async (input: Json) => integrationConnectionStatus(input)),
    },
  })
  const artifact = local("artifacts", "artifact")
  const worktreeBase = local("worktrees", "worktree", {}, false)
  const worktreeDefinition = () => [...registry.worktrees.values()].at(-1)
  const worktree: Json = {
    ...worktreeBase,
    list: lazy(async (input: Json) => {
      const definition = worktreeDefinition()
      const sourceDirectory = input?.sourceDirectory ?? input?.directory ?? directory
      const result =
        typeof definition?.list === "function"
          ? await settle(definition.list(sourceDirectory, { signal: new AbortController().signal }))
          : await callRPC("worktree.list", { ...(input ?? {}), sourceDirectory })
      return apiResponse(result)
    }),
    create: lazy(async (input: Json) => {
      const definition = worktreeDefinition()
      const from = input?.from ?? directory
      const result =
        typeof definition?.create === "function"
          ? await settle(
              definition.create(
                { sourceDirectory: from, directory: input?.directory ?? "", branch: input?.branch },
                { signal: new AbortController().signal },
              ),
            )
          : await callRPC("worktree.create", { ...(input ?? {}), sourceDirectory: from })
      return apiResponse(result)
    }),
    remove: lazy(async (input: Json) => {
      const definition = worktreeDefinition()
      if (typeof definition?.remove === "function") {
        await settle(
          definition.remove(
            { directory: input?.directory, force: input?.force ?? false },
            { signal: new AbortController().signal },
          ),
        )
      } else {
        await callRPC("worktree.remove", input)
      }
      return undefined
    }),
    refresh: lazy(async (input: Json) => {
      if (typeof worktreeDefinition()?.list !== "function") {
        await callRPC("worktree.refresh", input)
      }
      return undefined
    }),
  }
  const pluginApi: Json = { list: lazy(async () => apiResponse(pluginList())) }
  const rpcBase: Json = {
    register: (definition: Json, handlers: Json) => {
      const id = definition?.id
      if (typeof id !== "string") throw new Error("rpc.register requires a definition id")
      rpcs.set(id, { plugin, definition, handlers })
      const dispose = () => rpcs.delete(id)
      plugin.registrations.push(dispose)
      return effect(() => ({
        dispose: mode === "effect" ? Effect.sync(dispose) : async () => dispose(),
        events: {
          emit: async (...args: Json[]) => {
            const [name, data] = args
            const event = {
              id: `rpc_${Date.now()}`,
              type: `rpc.${id}.${String(name)}`,
              properties: data ?? {},
            }
            await emitEvent(event)
          },
        },
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
      const result: Json = await settle(handler(input.input, { signal: controller.signal, error }))
      if (result && result._tag === "Rpc.Error") throw Object.assign(new Error(result.message ?? "rpc error"), result)
      return result
    }),
  }
  const rpc: Json = Object.assign(
    (definition: Json) => {
      const methods: Json = {}
      for (const name of Object.keys(definition?.methods ?? {})) {
        methods[name] = lazy(async (input: Json) => {
          return await settle(rpcBase.call({ rpcID: definition.id, method: name, input }))
        })
      }
      return Object.assign(methods, {
        events: {
          subscribe: (name: string, options?: Json) => {
            if (!Object.hasOwn(definition?.events ?? {}, name)) {
              throw new Error(`Unknown RPC event: ${String(definition?.id)}.${name}`)
            }
            return subscribeEvents({
              types: [`rpc.${definition.id}.${name}`],
              signal: options?.signal,
            })
          },
          on: (name: string, handler: Function, options?: Json) => {
            const controller = new AbortController()
            const signal = options?.signal
              ? AbortSignal.any([controller.signal, options.signal])
              : controller.signal
            const source = subscribeEvents({ types: [`rpc.${definition.id}.${name}`], signal })
            void (async () => {
              for await (const event of source) await handler(event)
            })().catch((error) => log("error", `rpc event handler failed: ${String(error)}`))
            return () => controller.abort()
          },
        },
      })
    },
    rpcBase,
  )
  const generate: Json = {
    text: lazy((input: Json) => callRPC("generate.text", input)),
    image: lazy((input: Json) => callRPC("generate.image", input)),
    decisions: lazy((input: Json) => callRPC("generate.decisions", input)),
  }
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
    credential: api({ activate: lazy((input: Json) => activateStoredCredential(input)) }, mode, "credential"),
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
  const call = (method: string) => lazy(async (input: Json, options?: Json) => callRPC(method, input, options))
  const voidCall = (method: string) =>
    lazy(async (input: Json, options?: Json) => {
      await callRPC(method, input, options)
      return undefined
    })
  const paged = (method: string) =>
    lazy(async (input: Json, options?: Json) => {
      const value = await callRPC(method, input, options)
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
    active: call("session.active"),
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
    interrupt: lazy(async (input: Json, options?: Json) => {
      const result = await callRPC("session.interrupt", input, options)
      return { interrupted: result?.interrupted === true }
    }),
    update: voidCall("session.update"),
    move: voidCall("session.move"),
    background: voidCall("session.background"),
    share: call("session.share"),
    unshare: call("session.unshare"),
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
    shell: voidCall("session.shell"),
    environment: voidCall("session.environment"),
    view: voidCall("session.view"),
    diff: call("session.diff"),
    revert: {
      stage: call("session.revert.stage"),
      clear: voidCall("session.revert.clear"),
      commit: voidCall("session.revert.commit"),
    },
    inbox: {
      list: call("session.inbox.list"),
      cancel: voidCall("session.inbox.cancel"),
      update: voidCall("session.inbox.update"),
    },
    form: {
      list: call("session.form.list"),
      create: call("session.form.create"),
      get: call("session.form.get"),
      reply: voidCall("session.form.reply"),
      cancel: voidCall("session.form.cancel"),
    },
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
  const encoded = JSON.stringify(value)
  if (encoded === undefined) throw new Error(`storage value for ${key} is not JSON`)
  await Deno.mkdir(storageDir, { recursive: true })
  await Deno.writeTextFile(storageFile(key), encoded)
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

// ---------------------------------------------------------------- integrations

async function loadStoredCredentials() {
  const saved = await storageGet("__credentials__")
  if (!Array.isArray(saved)) return
  for (const credential of saved) {
    if (credential && typeof credential.id === "string") credentials.set(credential.id, credential)
  }
}

function saveStoredCredentials() {
  eventQueue = eventQueue
    .then(() => storageSet("__credentials__", [...credentials.values()]))
    .catch((error) => log("error", `saving credentials failed: ${String(error)}`))
}

function integrationName(id: string): string {
  return registry.integrations.get(id)?.name ?? id
}

function integrationConnectionList(id: string): Json[] {
  const saved = [...credentials.values()]
    .filter((credential) => credential.integrationID === id)
    .sort((a, b) => Number(Boolean(b.active)) - Number(Boolean(a.active)) || (b.time?.created ?? 0) - (a.time?.created ?? 0))
    .map((credential) => ({
      type: "credential",
      id: credential.id,
      label: credential.label,
      method: credential.value?.type,
    }))
  const methods = integrationMethods.get(id)?.values() ?? []
  const environment: Json[] = []
  for (const entry of methods) {
    if (entry.method?.type !== "env") continue
    for (const name of entry.method.names ?? []) {
      if (Deno.env.get(name)) environment.push({ type: "env", name })
    }
  }
  return [...saved, ...environment]
}

function projectIntegration(id: string): Json {
  const ref = registry.integrations.get(id) ?? { id, name: id }
  return {
    ...ref,
    methods: [...(integrationMethods.get(id)?.values() ?? [])].map((entry) => entry.method),
    connections: integrationConnectionList(id),
  }
}

function projectIntegrations(): Json[] {
  const ids = new Set([...registry.integrations.keys(), ...integrationMethods.keys()])
  return [...ids].map((id) => projectIntegration(id))
}

function credentialLabel(integrationID: string, requested?: string): string {
  if (requested) return requested
  const name = integrationName(integrationID)
  const labels = new Set(
    [...credentials.values()].filter((credential) => credential.integrationID === integrationID).map((credential) => credential.label),
  )
  return Array.from({ length: labels.size + 1 }, (_, index) => (index === 0 ? name : `${name} ${index + 1}`)).find(
    (candidate) => !labels.has(candidate),
  ) ?? name
}

function syncCredentialAuth(credential: Json) {
  const value = credential?.value
  if (!value) return
  if (value.type === "key") {
    void request("auth.set", {
      provider: credential.integrationID,
      key: value.key,
      metadata: value.metadata,
    })
    return
  }
  if (value.type === "oauth") {
    void request("auth.set", {
      provider: credential.integrationID,
      oauth: {
        refresh: value.refresh,
        access: value.access,
        expires: value.expires,
        accountId: value.metadata?.accountId,
        enterpriseUrl: value.metadata?.enterpriseUrl,
      },
    })
  }
}

function createStoredCredential(input: Json): Json {
  const integrationID = String(input.integrationID)
  const created = Date.now()
  const existing = [...credentials.values()].filter((credential) => credential.integrationID === integrationID)
  const active = input.activate !== false || !existing.some((credential) => credential.active)
  if (active) {
    for (const credential of existing) credential.active = false
  }
  const requestedID = typeof input.id === "string" ? input.id : undefined
  if (requestedID && credentials.has(requestedID)) throw new Error(`Credential already exists: ${requestedID}`)
  const entry = {
    id: requestedID ?? `cred_${crypto.randomUUID()}`,
    integrationID,
    label: credentialLabel(integrationID, input.label),
    active,
    value: input.value,
    time: { created, updated: created },
  }
  credentials.set(entry.id, entry)
  saveStoredCredentials()
  if (active) syncCredentialAuth(entry)
  void emitEvent({
    id: `credential_${created}`,
    type: "credential.updated",
    properties: { integrationID, credentialID: entry.id },
  })
  if (active) {
    void emitEvent({
      id: `credential_switch_${created}`,
      type: "credential.switched",
      properties: { integrationID, credentialID: entry.id },
    })
  }
  return entry
}

function activateStoredCredential(input: Json) {
  const id = input?.credentialID ?? input?.id
  const credential = typeof id === "string" ? credentials.get(id) : undefined
  if (!credential) return
  for (const item of credentials.values()) {
    if (item.integrationID === credential.integrationID) item.active = false
  }
  credential.active = true
  credential.time = { ...(credential.time ?? {}), updated: Date.now() }
  saveStoredCredentials()
  syncCredentialAuth(credential)
  void emitEvent({
    id: `credential_switch_${Date.now()}`,
    type: "credential.switched",
    properties: { integrationID: credential.integrationID, credentialID: credential.id },
  })
}

function findIntegrationMethod(integrationID: string, types: string[], methodID?: string): Json | undefined {
  return [...(integrationMethods.get(integrationID)?.values() ?? [])].find(
    (entry) => types.includes(entry.method?.type) && (methodID === undefined || entry.method?.id === methodID),
  )
}

function integrationConnectKey(input: Json) {
  const integrationID = String(input.integrationID)
  const known = integrationMethods.get(integrationID)
  if (known && !findIntegrationMethod(integrationID, ["key"])) throw new Error(`Key method not found: ${integrationID}`)
  const answer = input.answer && typeof input.answer === "object" ? input.answer : {}
  createStoredCredential({
    integrationID,
    label: input.label,
    value: {
      type: "key",
      key: String(input.key ?? ""),
      ...(Object.keys(answer).length ? { metadata: answer } : {}),
    },
  })
}

function integrationConnectExternal(input: Json) {
  const integrationID = String(input.integrationID)
  const method = findIntegrationMethod(integrationID, ["external"], input.methodID)
  if (!method) throw new Error(`External method not found: ${input.methodID}`)
  const answer = input.answer && typeof input.answer === "object" ? input.answer : {}
  createStoredCredential({
    integrationID,
    label: input.label,
    value: {
      type: "external",
      methodID: String(input.methodID),
      ...(Object.keys(answer).length ? { metadata: answer } : {}),
    },
  })
}

function findAttempt(input: Json): Json {
  const attempt = integrationAttempts.get(input?.attemptID)
  if (!attempt || attempt.integrationID !== input?.integrationID) throw new Error(`Attempt not found: ${String(input?.attemptID)}`)
  return attempt
}

function attemptResult(attempt: Json): Json {
  if (attempt.status === "failed") {
    return { status: "failed", message: attempt.message ?? "Authorization failed", time: attempt.time }
  }
  return { status: attempt.status, time: attempt.time }
}

async function integrationOauthConnect(input: Json): Promise<Json> {
  const integrationID = String(input.integrationID)
  const method = findIntegrationMethod(integrationID, ["oauth"], input.methodID)
  if (!method || typeof method.authorize !== "function") throw new Error(`OAuth method not found: ${input.methodID}`)
  const authorization = await settle(method.authorize(input.answer ?? {}))
  const created = Date.now()
  const expires = authorization.expiresAt ?? created + 15 * 60_000
  const attemptID = `int_${crypto.randomUUID()}`
  const attempt = {
    integrationID,
    status: "pending",
    time: { created, expires },
    authorization,
  }
  integrationAttempts.set(attemptID, attempt)
  if (authorization.mode === "auto") {
    void (async () => {
      try {
        const value = await settle(authorization.callback)
        createStoredCredential({ integrationID, label: input.label, value })
        attempt.status = "complete"
      } catch (error) {
        attempt.status = "failed"
        attempt.message = String((error as Json)?.message ?? error)
      }
    })()
  }
  return {
    attemptID,
    url: authorization.url,
    instructions: authorization.instructions,
    mode: authorization.mode,
    ...(authorization.paste ? { paste: true } : {}),
    time: { created, expires },
  }
}

async function integrationOauthComplete(input: Json) {
  const attempt = integrationAttempts.get(input.attemptID)
  if (!attempt || attempt.integrationID !== input.integrationID) throw new Error(`Attempt not found: ${input.attemptID}`)
  if (attempt.status !== "pending") return
  const authorization = attempt.authorization
  if (authorization.mode === "code") {
    try {
      const value = await settle(authorization.callback(input.code))
      createStoredCredential({ integrationID: attempt.integrationID, value })
      attempt.status = "complete"
    } catch (error) {
      attempt.status = "failed"
      attempt.message = String((error as Json)?.message ?? error)
    }
  }
}

function integrationOauthCancel(input: Json) {
  const attempt = integrationAttempts.get(input.attemptID)
  if (!attempt || attempt.integrationID !== input.integrationID) return
  attempt.status = "failed"
  attempt.message = "Authorization cancelled"
}

function integrationCommandConnect(input: Json): Json {
  const integrationID = String(input.integrationID)
  const method = findIntegrationMethod(integrationID, ["command"], input.methodID)
  if (!method) throw new Error(`Command method not found: ${input.methodID}`)
  const created = Date.now()
  const expires = created + 15 * 60_000
  const attemptID = `int_${crypto.randomUUID()}`
  const attempt = { integrationID, status: "pending", message: undefined, time: { created, expires }, cancelled: false }
  integrationAttempts.set(attemptID, attempt)
  void (async () => {
    try {
      const result = await request("shell.exec", {
        argv: method.method.command,
        cwd: directory,
        env: {},
      })
      if (attempt.cancelled) return
      if ((result.exitCode ?? 0) !== 0) throw new Error(String(result.stderr ?? "").trim() || `Authentication command exited ${result.exitCode}`)
      const credential = String(result.stdout ?? "").trim()
      if (!credential) throw new Error("Authentication command returned no credential")
      createStoredCredential({
        integrationID,
        label: input.label,
        value: { type: "key", key: credential, metadata: { methodID: input.methodID } },
      })
      attempt.status = "complete"
    } catch (error) {
      if (attempt.cancelled) return
      attempt.status = "failed"
      attempt.message = String((error as Json)?.message ?? error)
    }
  })()
  return { attemptID, time: { created, expires } }
}

function integrationCommandCancel(input: Json) {
  const attempt = integrationAttempts.get(input.attemptID)
  if (!attempt || attempt.integrationID !== input.integrationID) return
  attempt.cancelled = true
  attempt.status = "failed"
  attempt.message = "Authentication cancelled"
}

async function integrationResolveConnection(connection: Json): Promise<Json | undefined> {
  if (!connection) return undefined
  if (connection.type === "env") {
    const value = typeof connection.name === "string" ? Deno.env.get(connection.name) : undefined
    return value ? { type: "key", key: value } : undefined
  }
  const credential = credentials.get(connection.id)
  if (!credential) return undefined
  const value = credential.value
  if (value?.type !== "oauth") return value
  const expires = typeof value.expires === "number" ? value.expires : 0
  if (!expires || expires > Date.now() + 60_000) return value
  const registration = integrationMethods.get(credential.integrationID)?.get(value.methodID)
  if (typeof registration?.refresh !== "function") return value
  try {
    const refreshed = await settle(registration.refresh(value))
    credential.value = refreshed
    credential.time = { ...(credential.time ?? {}), updated: Date.now() }
    saveStoredCredentials()
    syncCredentialAuth(credential)
    void emitEvent({
      id: `credential_updated_${Date.now()}`,
      type: "credential.updated",
      properties: { integrationID: credential.integrationID, credentialID: credential.id },
    })
    return refreshed
  } catch (error) {
    log("error", `credential refresh failed: ${String(error)}`)
    return value
  }
}

function integrationConnectionStatus(input: Json) {
  const key = `${input.integrationID}:${input.connection?.id ?? input.connection?.name ?? "default"}`
  if (input.status) connectionStatuses.set(key, input.status)
  else connectionStatuses.delete(key)
  void emitEvent({
    id: `integration_status_${Date.now()}`,
    type: "integration.updated",
    properties: {},
  })
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
    namespaces: [...namespaces.values()],
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
    tui: { commands: tuiCommandList(), pages: tuiPageList() },
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
    if (!candidates.length) {
      record.features = { server: false, tui: true }
      record.state = { status: "active" }
      plugins.push(record)
      return record
    }
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
  await loadStoredCredentials()
  for (const entry of config.plugins ?? []) {
    await loadPlugin(entry)
  }
  if (config.tui === true) {
    for (const entry of config.plugins ?? []) {
      if (Array.isArray(entry.tuiCandidates) && entry.tuiCandidates.length > 0) await loadTuiPlugin(entry)
    }
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
    errors: [...plugins, ...tuiPlugins]
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
  for (const plugin of tuiPlugins) {
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
  }
}

void readLoop()
