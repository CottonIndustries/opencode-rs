// Minimal Solid-style runtime used by compiled TUI plugins. The host renders a
// single snapshot of the element tree, so signals, memos and effects resolve
// eagerly; the shapes match what the Solid JSX transform emits.
type Json = any

function resolveValue(value: Json, depth = 0): Json {
  let current = value
  let steps = 0
  while (typeof current === "function" && steps < 32) {
    current = current()
    steps += 1
  }
  return current
}

function append(parent: Json, value: Json, depth = 0, marker: Json = null) {
  if (depth > 64) return
  const place = (node: Json) => {
    if (!parent.children) parent.children = []
    const index = marker ? parent.children.indexOf(marker) : -1
    if (index >= 0) parent.children.splice(index, 0, node)
    else parent.children.push(node)
  }
  // Keep accessors lazy so a signal written after the first paint can still
  // contribute text; the host resolves them when it snapshots the tree.
  if (typeof value === "function") {
    place({ type: "#lazy", get: value })
    return
  }
  const current = resolveValue(value)
  if (current === null || current === undefined || current === false || current === true) return
  if (Array.isArray(current)) {
    for (const item of current) append(parent, item, depth + 1, marker)
    return
  }
  place(current)
}

export function createElement(type: Json, props: Json = {}) {
  return { type, props: props ?? {} }
}

export function createTextNode(text: Json) {
  return { type: "#text", text: String(resolveValue(text) ?? "") }
}

export function use(_value?: Json) {}

export function createComponent(component: Json, props: Json = {}) {
  if (typeof component === "function") return component(props)
  return { type: component, props: props ?? {} }
}

export function setProp(element: Json, name: string, value: Json) {
  if (!element) return
  if (!element.props) element.props = {}
  element.props[name] = value
}

export function spread(element: Json, props: Json) {
  if (!element || !props) return
  if (!element.props) element.props = {}
  Object.assign(element.props, props)
}

export function insert(parent: Json, value: Json, marker: Json = null) {
  append(parent, value, 0, marker)
}

export function insertNode(parent: Json, value: Json) {
  append(parent, value)
}

export function effect(fn: Function, value?: Json) {
  return fn(value)
}

export function memo(fn: Function) {
  return fn
}

export function createMemo(fn: Function) {
  return () => resolveValue(fn)
}

export function createRef() {
  return { current: undefined as Json }
}

export function createSignal(initial: Json) {
  let value = resolveValue(initial)
  const read = () => value
  const write = (next: Json) => {
    value = typeof next === "function" ? next(value) : next
    return value
  }
  return [read, write]
}

export function createEffect(fn: Function, value?: Json) {
  try {
    return fn(value)
  } catch {
    return undefined
  }
}

export function createComputed(fn: Function) {
  return createEffect(fn)
}

export function createRenderEffect(fn: Function) {
  return createEffect(fn)
}

export function on(deps: Json, fn: Function, _options?: Json) {
  resolveValue(deps)
  return fn()
}

export function onCleanup(_fn: Function) {}

export function onMount(fn: Function) {
  return createEffect(fn)
}

export function batch(fn: Function) {
  return fn()
}

export function untrack(fn: Function) {
  return fn()
}

export function startTransition(fn: Function) {
  return fn()
}

export function createRoot(fn: Function) {
  return fn(() => {})
}

export function createContext(defaultValue: Json) {
  return { defaultValue }
}

export function useContext(context: Json) {
  return context?.defaultValue
}

export function mergeProps(...values: Json[]) {
  return Object.assign({}, ...values.map((value) => resolveValue(value) ?? {}))
}

export function splitProps(props: Json, keys: string[]) {
  const picked: Json = {}
  const rest: Json = {}
  for (const [key, value] of Object.entries(props ?? {})) {
    if (keys.includes(key)) picked[key] = value
    else rest[key] = value
  }
  return [picked, rest]
}

export function For(props: Json) {
  return { type: "#for", when: props?.each, children: props?.children }
}

export function Index(props: Json) {
  const each = resolveValue(props?.each)
  const items = Array.isArray(each) ? each : []
  const render = props?.children
  if (typeof render !== "function") return items
  return items.map((item, index) => resolveValue(render(() => item, index)))
}

export function Show(props: Json) {
  return { type: "#show", when: props?.when, children: props?.children, fallback: props?.fallback }
}

export function Switch(props: Json) {
  const children = resolveValue(props?.children)
  const items = Array.isArray(children) ? children : [children]
  return items.find((item) => {
    const value = resolveValue(item)
    return value !== null && value !== undefined && value !== false
  })
}

export function Match(props: Json) {
  const when = resolveValue(props?.when)
  if (!when) return undefined
  const render = props?.children
  return typeof render === "function" ? render(when) : render
}

export function createMutable(initial: Json) {
  return initial ?? {}
}

export function modifyMutable(target: Json, mutation: Function) {
  if (typeof mutation === "function") mutation(target)
  return target
}

export function getOwner() {
  return undefined
}

export function runWithOwner(_owner: Json, fn: Function) {
  return fn()
}

export function createReaction(fn: Function) {
  return [fn, () => {}]
}

export function createStore(initial: Json) {
  const state = initial ?? {}
  const setState = (mutation: Function) => {
    if (typeof mutation === "function") mutation(state)
  }
  return [state, setState]
}

export function produce(mutation: Function) {
  return mutation
}

export function createResource(source: Json, fetcher?: Function) {
  let value: Json = undefined
  const read = () => value
  const refetch = async () => {
    if (typeof fetcher !== "function") return value
    value = await fetcher(resolveValue(source))
    return value
  }
  return [read, { refetch, mutate: (next: Json) => (value = next) }]
}

export function createSelector(_source: Json, fn?: Function) {
  return fn ?? (() => false)
}

export function createDeferred(value: Json) {
  return resolveValue(value)
}

export function reconcile(_value: Json) {
  return _value
}

export function createUniqueId() {
  return "id-" + Math.random().toString(36).slice(2)
}

export function mapArray(list: Json, fn: Function) {
  const items = Array.isArray(list) ? list : []
  return items.map((item, index) => fn(() => item, index))
}

export function indexArray(list: Json, fn: Function) {
  const items = Array.isArray(list) ? list : []
  return items.map((item, index) => fn(() => item, index))
}

export const isServer = false
export const DEV = false
export const hydrate = () => {}
