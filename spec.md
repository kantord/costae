# costae architecture specification

costae is a **status bar and widget system** for Linux desktops. It is not a general-purpose
GUI framework. The scope is desktop shell surfaces: bars, docks, notification areas, and
similar widgets. Layouts are declared in JSX, data comes from subprocess streams, and
rendering is done in software via takumi + tiny-skia.


## Scripting language: JavaScript (JSX via rquickjs + OXC)

Layout files are `.jsx` files evaluated by **QuickJS** (via `rquickjs`) with JSX syntax
transformed by **OXC** (`oxc_transformer`). No custom parser or preprocessor — two maintained
Rust crates, both available on crates.io. Requires a C compiler at build time (QuickJS is
vendored via `rquickjs-sys`, no system libraries needed).

### How it works

1. **On layout file load or change**: OXC parses the JSX source and locates the last
   top-level `ExpressionStatement` via the AST (not string heuristics). Everything before
   it becomes the body of `globalThis._render`; the final expression becomes its return
   value. OXC then transforms JSX syntax to `_jsx(...)` calls and emits plain JS.
2. **On each data tick**: The QuickJS `Runtime` and `Context` are kept alive between ticks
   (created once, reused forever). Stream values are updated in a shared map, then
   `_render()` is called. No reparse, no recompile. Returns a JS object tree. Rust walks
   the tree to extract panels and build takumi node trees.

OXC transform + wrap: **<1ms** (layout-file-change only). QuickJS `_render()` call:
**~100–200μs**. Dominant cost is always takumi + skia rasterization.

### Sandbox

rquickjs is deny-by-default. No filesystem, network, or process access unless explicitly
registered from Rust. costae exposes only `useStringStream`, `useJSONStream`, `Module`,
and `ctx`.

### Example layout file

```jsx
// The layout file IS the config — no separate config.yaml.
// The last expression must be a <root> node containing one or more <panel> nodes.
// No wrapping parens needed.

function TimeCard() {
  const time = useStringStream("/usr/bin/bash", `
    while true; do date +"%H:%M"; sleep 1; done
  `);
  return (
    <container tw="flex flex-col gap-1 rounded-lg border px-3 py-2">
      <text tw="text-[10px] opacity-60">TIME</text>
      <text tw="text-[14px] text-white">{time}</text>
    </container>
  );
}

<root>
  <panel anchor="left" width={250} height={ctx.screen_height}>
    <container tw="flex flex-col h-full w-full px-4 py-4">
      <Module bin="/home/kantord/.cargo/bin/costae-i3">
        {(data, events) => <WorkspaceList workspaces={data?.workspaces} events={events} />}
      </Module>
      <TimeCard />
    </container>
  </panel>
</root>
```

Components are plain JS functions. No framework, no hooks protocol — `useStringStream`,
`useJSONStream`, and `Module` are Rust-registered globals, not React hooks.


## Layout file

The layout file is the single source of configuration. There is no `config.yaml`. If
future top-level settings are needed they go as props on `<root>`.

The file is watched for changes and hot-reloaded. On reload all subprocesses are restarted
and stream values are cleared.

The file is re-evaluated on every data tick (stream value change). Re-evaluation is cheap
(~100–200μs) because `_render()` is pre-compiled and the QuickJS context is reused.


## Nodes

Low-level nodes map directly to takumi nodes. The script produces a JS object tree; Rust
walks it to construct the takumi node tree. No intermediate representation.

`_jsx` is registered from Rust as a global. It receives `(tag, props, ...children)` and
returns a plain JS object `{ type, ...props, children }`.

### Layout nodes (inside panels)

| node | description |
|---|---|
| `container` | flex container, maps to takumi container |
| `text` | text node |
| `image` | image node |

### Shell nodes (top-level structure)

| node | description |
|---|---|
| `root` | mandatory top-level node, contains one or more `panel` nodes |
| `panel` | declares one desktop surface (X11 window / Wayland layer surface) |

### `<panel>` props

| prop | type | description |
|---|---|---|
| `anchor` | `"left" \| "right" \| "top" \| "bottom"` | stick to this screen edge and reserve strut space. omit for a free-floating panel |
| `width` | number | panel width in logical pixels |
| `height` | number | panel height in logical pixels |
| `x` | number | x position (ignored when `anchor` is set) |
| `y` | number | y position (ignored when `anchor` is set) |

`anchor` implies both the screen-edge attachment and the EWMH strut reservation. No
implicit behavior — everything must be declared explicitly.


## Components

Components are plain JS functions that take props and return a node tree. JSX handles
`<Card />` as a function call naturally — no registration needed.

### Global context

`ctx` is injected by Rust before script evaluation. Read-only. Currently minimal —
app-specific config values belong in the layout file itself, not in ctx.

| field | description |
|---|---|
| `ctx.output` | RANDR output name, e.g. `"DP-4"` |
| `ctx.dpi` | display DPI |
| `ctx.screen_width` | monitor width in logical pixels |
| `ctx.screen_height` | monitor height in logical pixels |

### State

No built-in local component state. Defer until there is a concrete use case.


## Data layer

All external data flows through the data layer. Same subprocess registry underneath all
three calling conventions.

### Subprocess identity

The identity key is `(bin, script)`. On each re-evaluation, Rust diffs the old set against
the new one: unchanged identities reuse the running subprocess, removed ones are killed,
new ones are spawned.

### `useStringStream(bin, script?)`

Returns the latest stdout line as a string.

```jsx
const time = useStringStream("/usr/bin/bash", `
  while true; do date +"%H:%M"; sleep 1; done
`);
```

### `useJSONStream(bin, script?)`

Returns the latest stdout line parsed as a JS object.

```jsx
const data = useJSONStream("/usr/bin/myscript");
```

### `<Module bin="...">{(data, events) => ...}</Module>`

A bidirectional subprocess. Sends an init event on startup, receives JSON data on stdout,
routes click events back to stdin. Exposes `data` (latest parsed JSON output) and `events`
(a Proxy that generates serializable click descriptors routed back to the subprocess).

```jsx
<Module bin="/home/kantord/.cargo/bin/costae-i3">
  {(data, events) => (
    <WorkspaceList workspaces={data?.workspaces} events={events} />
  )}
</Module>
```


## Display backend

### Abstraction boundary

All display-server-specific code lives behind a clear boundary. The main loop never calls
x11rb directly — it only calls through the panel abstraction. This ensures that adding
Wayland support later requires only a new implementation of that abstraction, with zero
changes to the core loop, JSX evaluation, render pipeline, or data layer.

### X11 panel

Current implementation. One `XPanel` struct per `<panel>` node. Responsibilities:
- Create and configure the X11 window (override-redirect, strut properties)
- Accept BGRX pixel buffers and put them to the window via `XPutImage`
- Report button-press events up to the main loop
- Expose monitor geometry so panels can size themselves via `ctx.screen_width` /
  `ctx.screen_height`

### Wayland panel (future)

Will implement the same interface using the wlr-layer-shell protocol
(`zwlr_layer_surface_v1`). `anchor` maps directly to layer-shell anchor edges. Strut
reservation is handled automatically by the compositor. No other changes needed.


## Rendering

Each panel has its own `RenderCache` keyed by the serialized JSON of its subtree. Only
panels whose content changed are re-rendered. Rasterization is software-only (takumi +
tiny-skia). The dominant cost is the full-panel rasterization pass (~40–90ms at 365×2160);
all upstream steps (JSX eval, layout parse, cache key check) are <1ms.

### Caching

Cache key = canonical JSON of the panel's subtree (`json_canon`). On hit, the cached BGRX
buffer is reused and blitted directly. On miss, takumi measures and rasterizes the full
panel, then the result is cached.


## Implementation phases

1. ✅ **Scripting engine** — rquickjs + OXC, persistent `JsxEvaluator`, `_render()` pattern
2. ✅ **Data layer** — `useStringStream`, `useJSONStream`, `Module`, subprocess reconciliation,
   stream batching
3. **OXC AST split** — replace `rfind("\n(")` heuristic in `wrap_source_as_render_fn` with
   proper OXC AST traversal to find the last `ExpressionStatement`; remove need for
   wrapping parens in layout files
4. **Multi-panel** — `<root>` and `<panel>` node types; JSX evaluator returns a root node;
   Rust extracts panels and manages one X11 window + render cache per panel; `anchor` prop
   drives strut reservation; `config.yaml` removed, layout file is the sole config;
   `ctx` updated with `screen_width` / `screen_height`
5. **Float transparency** — RGBA compositing pipeline
