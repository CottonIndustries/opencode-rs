export function createElement(type: unknown, props?: Record<string, unknown>) {
  return { type, props: props ?? {} };
}

export function spread(
  element: Record<string, unknown>,
  props: Record<string, unknown>,
) {
  element.props = Object.assign({}, element.props ?? {}, props);
}

export function createTextNode(text: string) {
  return { type: "text", props: { children: text } };
}

export function createComponent(component: unknown, props: unknown) {
  return typeof component === "function"
    ? (component as Function)(props)
    : { type: "component", props };
}

export function createMemo(fn: Function) {
  return fn();
}

export function mergeProps(...values: unknown[]) {
  return Object.assign({}, ...values);
}

export function memo(fn: Function) {
  return fn;
}

export function effect(fn: Function) {
  return fn();
}

export function insert() {}
export function insertNode() {}
export function setProp() {}
export function use() {}

export function jsx(type: unknown, props?: Record<string, unknown>) {
  return { type, props: props ?? {} };
}

export const jsxs = jsx;
export const Fragment = "fragment";
