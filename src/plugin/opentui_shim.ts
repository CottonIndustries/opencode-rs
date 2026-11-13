// A compact @opentui/solid surface for plugin UI. It reuses the Solid-style
// element runtime below; renderer hooks return inert state because the Rust TUI
// owns focus, sizing and painting.
export * from "./solid_shim.ts"

type Json = any

export function useRenderer() {
  return {
    requestRender: () => {},
    width: 80,
    height: 24,
    terminalWidth: 80,
    terminalHeight: 24,
    setTerminalTitle: () => {},
    copyToClipboard: async () => {},
    getSelection: () => undefined,
  }
}

export function useTerminalDimensions() {
  return { width: 80, height: 24 }
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

export function useKeyboard(_handler?: Function) {}

export function usePaste(_handler?: Function) {}

export function useFocus() {
  return {
    focused: () => false,
    focus: () => {},
    blur: () => {},
    isFocus: () => false,
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
