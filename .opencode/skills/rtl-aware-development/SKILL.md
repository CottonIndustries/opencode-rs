---
name: rtl-aware-development
description: Use for direction-aware layout in HTML surfaces such as shared session pages or any future web UI in opencode-rs. The terminal UI is a fixed LTR cell grid and does not use CSS direction; this skill records the rules for the places that do.
---

# Direction-aware development

Treat direction as independent from language. Test English in both directions as well as real RTL and mixed-script content.

## Scope in opencode-rs

- The TUI is a character grid rendered by ratatui. It has no CSS direction, logical box model or browser scroll model. Keep terminal output stable and LTR; do not assume terminal fonts will shape Arabic or Hebrew correctly.
- The HTML surfaces this repo can emit are share pages and any future web UI. Those follow the web rules below.
- Desktop and web apps are not ported; if one is added later, this skill applies to it unchanged.

## HTML guidelines

- Set `lang` and `dir` on the document and propagate direction through any provider used by portaled menus and popovers. Do not change the selected locale just to force RTL.
- Keep DOM and focus order semantic. Flexbox and Grid follow `dir`; do not add `row-reverse`, CSS `order` or reversed markup just to mirror a layout.
- Prefer logical CSS for layout. Reserve physical coordinates for pointer positions, canvas geometry and native window controls.

```css
/* Avoid */
padding-left: 12px;
right: 0;
border-right: 1px solid;
text-align: left;

/* Prefer */
padding-inline-start: 12px;
inset-inline-end: 0;
border-inline-end: 1px solid;
text-align: start;
```

- Isolate mixed-direction text. Use `dir="auto"` or `<bdi>` for unknown text; keep code, URLs, IDs and filesystem paths LTR without forcing the surrounding component LTR.

```html
<span class="file-row"><bdi dir="auto">README.md</bdi></span>
<bdi dir="ltr"><code>C:\src\app.ts</code></bdi>
```

- Mirror directional meaning, not every image. Back/forward, previous/next, disclosure, indentation and directional progress may need mirroring. Do not mirror brands, clocks, media controls, charts or text. Reverse physical gradients, `translateX`, SVG transforms and animation deltas explicitly.
- Map interactions through direction. `clientX` remains physical; resizing a logical edge needs an RTL-aware delta. Logical previous/next keyboard controls may swap ArrowLeft/ArrowRight. Follow the relevant WAI-ARIA pattern.
- Do not assume LTR scrolling. RTL `scrollLeft` can start at `0` and become negative. Prefer `scrollIntoView({ inline: "nearest" })` or a tested direction-normalizing helper.

## Share pages

Static transcripts and artifact views are read-only. Keep paths and code blocks in `<bdi dir="ltr">`, set the document language from configuration, and avoid layout rules that assume a left sidebar. Test a long unbroken path and an RTL title in the same page.

## Test matrix

- English + LTR
- English + forced RTL
- A real RTL locale + RTL
- Mixed RTL/LTR content, long labels, numbers, code and paths
- Keyboard, pointer resize, scrolling and menus/submenus in both directions

## References

- RTL Styling 101, Ahmad Shadeed: <https://rtlstyling.com/posts/rtl-styling/>
- W3C structural markup and right-to-left text: <https://www.w3.org/International/questions/qa-html-dir>
- W3C inline bidirectional markup: <https://www.w3.org/International/articles/inline-bidi-markup/>
- MDN CSS logical properties: <https://developer.mozilla.org/en-US/docs/Web/CSS/Guides/Logical_properties_and_values>
