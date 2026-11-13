// The small @opentui/core surface plugin JSX reads at runtime: color values and
// text attribute flags. Renderables are declarative JSX here, so the classes
// only need to be constructible stubs.
type Json = any

export const TextAttributes = {
  NONE: 0,
  BOLD: 1,
  DIM: 2,
  ITALIC: 4,
  UNDERLINE: 8,
  BLINK: 16,
  INVERSE: 32,
  HIDDEN: 64,
  STRIKETHROUGH: 128,
}

function byte(value: number) {
  return Math.max(0, Math.min(255, Math.round(value)))
}

export class RGBA {
  r: number
  g: number
  b: number
  a: number

  constructor(r = 0, g = 0, b = 0, a = 1) {
    this.r = r
    this.g = g
    this.b = b
    this.a = a
  }

  static fromInts(r: number, g: number, b: number, a = 255) {
    return new RGBA(byte(r) / 255, byte(g) / 255, byte(b) / 255, byte(a) / 255)
  }

  static fromValues(r: number, g: number, b: number, a = 1) {
    return new RGBA(r, g, b, a)
  }

  static fromArray(values: number[]) {
    return new RGBA(values[0] ?? 0, values[1] ?? 0, values[2] ?? 0, values[3] ?? 1)
  }

  static fromHex(value: string) {
    const text = value.startsWith("#") ? value.slice(1) : value
    const pair = (index: number) => parseInt(text.slice(index, index + 2), 16) || 0
    const digit = (index: number) => (parseInt(text.slice(index, index + 1), 16) || 0) * 17
    if (text.length === 3) return RGBA.fromInts(digit(0), digit(1), digit(2), 255)
    if (text.length === 4) return RGBA.fromInts(digit(0), digit(1), digit(2), digit(3))
    if (text.length === 6) return RGBA.fromInts(pair(0), pair(2), pair(4), 255)
    if (text.length === 8) return RGBA.fromInts(pair(0), pair(2), pair(4), pair(6))
    return new RGBA()
  }

  get hex() {
    const to = (value: number) => byte(value * 255).toString(16).padStart(2, "0")
    return `#${to(this.r)}${to(this.g)}${to(this.b)}${this.a >= 1 ? "" : to(this.a)}`
  }

  toInts() {
    return [byte(this.r * 255), byte(this.g * 255), byte(this.b * 255), byte(this.a * 255)]
  }

  toArray() {
    return [this.r, this.g, this.b, this.a]
  }

  toString() {
    return this.hex
  }

  equals(other: RGBA) {
    return this.r === other?.r && this.g === other?.g && this.b === other?.b && this.a === other?.a
  }
}

const NAMED: Record<string, string> = {
  black: "#000000",
  white: "#ffffff",
  red: "#ff0000",
  green: "#00ff00",
  blue: "#0000ff",
  yellow: "#ffff00",
  magenta: "#ff00ff",
  cyan: "#00ffff",
  gray: "#808080",
  grey: "#808080",
  transparent: "#00000000",
}

export function parseColor(value: Json) {
  if (value instanceof RGBA) return value
  if (typeof value === "string") {
    const named = NAMED[value.toLowerCase()]
    return RGBA.fromHex(named ?? value)
  }
  if (Array.isArray(value)) return RGBA.fromArray(value)
  if (value && typeof value === "object") {
    if (typeof value.r === "number") return new RGBA(value.r, value.g, value.b, value.a ?? 1)
    if (typeof value.hex === "string") return RGBA.fromHex(value.hex)
  }
  return new RGBA()
}

export function fg(color: Json) {
  return { fg: color }
}

export function bg(color: Json) {
  return { bg: color }
}

export function bold(value: Json) {
  return { ...value, bold: true }
}

export function dim(value: Json) {
  return { ...value, dim: true }
}

export function italic(value: Json) {
  return { ...value, italic: true }
}

export function underline(value: Json) {
  return { ...value, underline: true }
}

export function strikethrough(value: Json) {
  return { ...value, strikethrough: true }
}

export class BoxRenderable {}
export class TextRenderable {}
export class ScrollBoxRenderable {}
export class CodeRenderable {}
export class MarkdownRenderable {}
export class InputRenderable {}
export class TextareaRenderable {}
export class SelectRenderable {}
