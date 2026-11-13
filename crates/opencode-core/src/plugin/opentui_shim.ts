// A compact @opentui/solid surface for plugin UI. It reuses the Solid-style
// element runtime below; renderer hooks return inert state because the Rust TUI
// owns focus, sizing and painting.
export * from "./solid_shim.ts"

type Json = any

export function useRenderer() {
  const dimensions = () => (globalThis as Json).__opencodeDimensions ?? { width: 80, height: 24 }
  return {
    requestRender: () => (globalThis as Json).__opencodeRequestRender?.(),
    get width() {
      return Number(dimensions().width ?? 80)
    },
    get height() {
      return Number(dimensions().height ?? 24)
    },
    get terminalWidth() {
      return Number(dimensions().width ?? 80)
    },
    get terminalHeight() {
      return Number(dimensions().height ?? 24)
    },
    setTerminalTitle: () => {},
    copyToClipboard: async () => {},
    getSelection: () => undefined,
    requestAnimationFrame: (callback: Function) => setTimeout(callback, 0),
    cancelAnimationFrame: (handle: number) => clearTimeout(handle),
  }
}

export function useTerminalDimensions() {
  const dimensions = (globalThis as Json).__opencodeDimensions ?? { width: 80, height: 24 }
  return { width: Number(dimensions.width ?? 80), height: Number(dimensions.height ?? 24) }
}

export function useStdout() {
  return { write: () => {} }
}

export function useStderr() {
  return { write: () => {} }
}

export function useStdin() {
  return {}
}

export function useKeyboard(handler?: Function) {
  if (typeof handler !== "function") return
  const key = "__opencodeKeyboard"
  const target = ((globalThis as Json)[key] ??= [] as Function[])
  target.push(handler)
}

export function usePaste(handler?: Function) {
  if (typeof handler !== "function") return
  const key = "__opencodePaste"
  const target = ((globalThis as Json)[key] ??= [] as Function[])
  target.push(handler)
}

export function useFocus() {
  const root = ((globalThis as Json).__opencodeFocus ??= { next: 0, current: 0 })
  const id = root.next++
  const render = () => (globalThis as Json).__opencodeRequestRender?.()
  return {
    focused: () => root.current === id,
    focus: () => {
      root.current = id
      render()
    },
    blur: () => {
      if (root.current === id) root.current = -1
      render()
    },
    isFocus: (other: Json) => root.current === (typeof other === "object" ? other?.id : other),
    id,
  }
}

export function useApp() {
  return { version: "2.2.7", exit: () => {} }
}

export function useArgs() {
  return []
}

export function useExit() {
  return () => {}
}

export function Suspense(props: Json) {
  return props?.children
}

export function SuspenseList(props: Json) {
  return props?.children
}

export function Dynamic(props: Json) {
  const component = props?.component
  const rest = { ...props }
  delete rest.component
  return typeof component === "function" ? component(rest) : rest?.children
}

export function ErrorBoundary(props: Json) {
  return props?.children
}

export function Portal(props: Json) {
  return props?.children
}

export function extend(...values: Json[]) {
  return Object.assign({}, ...values)
}

export function ensureRuntimePluginSupport() {}

export function render(node: Json, _options?: Json) {
  return node
}
