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
    setTerminalTitle: (title: string) => (globalThis as Json).__opencodeTitle?.(String(title ?? "")),
    copyToClipboard: async (text: string) => (globalThis as Json).__opencodeCopy?.(String(text ?? "")),
    getSelection: () => (globalThis as Json).__opencodeSelection ?? "",
    requestAnimationFrame: (callback: Function) => setTimeout(callback, 0),
    cancelAnimationFrame: (handle: number) => clearTimeout(handle),
  }
}

export function useTerminalDimensions() {
  const dimensions = () => {
    const value = (globalThis as Json).__opencodeDimensions ?? { width: 80, height: 24 }
    return { width: Number(value.width ?? 80), height: Number(value.height ?? 24) }
  }
  const accessor = dimensions as Json
  Object.defineProperty(accessor, "width", {
    get: () => dimensions().width,
  })
  Object.defineProperty(accessor, "height", {
    get: () => dimensions().height,
  })
  return accessor
}

export function onResize(callback: Function) {
  if (typeof callback === "function") {
    const value = (globalThis as Json).__opencodeDimensions ?? { width: 80, height: 24 }
    callback(Number(value.width ?? 80), Number(value.height ?? 24))
  }
}

export function onAnimationTick(fps: Json, callback: Function, _options?: Json) {
  if (typeof callback !== "function") return
  const rate = typeof fps === "function" ? Number(fps()) : Number(fps ?? 60)
  if (!Number.isFinite(rate) || rate <= 0) return
  return setInterval(() => callback(1000 / rate), Math.max(1, Math.round(1000 / rate)))
}

export function onFocus(_callback: Function) {}
export function onBlur(_callback: Function) {}
export function useSelectionHandler(_callback: Function) {}
export function useKeyHandler(callback: Function, options?: Json) {
  return useKeyboard(callback, options)
}

export function useTimeline(options?: Json) {
  const duration = Math.max(0, Number(options?.duration ?? 0))
  let playing = false
  let elapsed = 0
  let started = 0
  let timer: Json = undefined
  let update: Function | undefined
  let complete: Function | undefined
  const render = () => (globalThis as Json).__opencodeRequestRender?.()
  const now = () => elapsed + (playing ? Date.now() - started : 0)
  const stopTimer = () => {
    if (timer !== undefined) clearInterval(timer)
    timer = undefined
  }
  const tick = () => {
    const time = now()
    if (time >= duration) {
      elapsed = duration
      playing = false
      stopTimer()
      complete?.(time)
      render()
      return
    }
    update?.(time)
    render()
  }
  const play = () => {
    if (playing || duration <= 0) return
    playing = true
    started = Date.now()
    stopTimer()
    timer = setInterval(tick, 16)
    render()
  }
  const pause = () => {
    if (!playing) return
    elapsed = now()
    playing = false
    stopTimer()
    render()
  }
  const seek = (value: number) => {
    elapsed = Math.max(0, Math.min(duration, Number(value ?? 0)))
    started = Date.now()
    render()
  }
  return {
    duration,
    play,
    pause,
    restart: () => {
      elapsed = 0
      started = Date.now()
      play()
    },
    seek,
    stop: () => {
      elapsed = 0
      playing = false
      stopTimer()
      render()
    },
    isPlaying: () => playing,
    currentTime: now,
    onComplete: (callback: Function) => {
      complete = callback
    },
    onUpdate: (callback: Function) => {
      update = callback
    },
  }
}

export function createDynamic(component: Json, props: Json) {
  const value = typeof component === "function" ? component() : component
  return typeof value === "function" ? value(props) : { type: value, props }
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

export function useKeyboard(handler?: Function, _options?: Json) {
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
