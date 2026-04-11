# costae architecture specification

## Scripting language: JavaScript (JSX via rquickjs + OXC)

Layout files are `.jsx` files evaluated by **QuickJS** (via `rquickjs`) with JSX syntax transformed by **OXC** (`oxc_transformer`). No custom parser or preprocessor — two maintained Rust crates, both available on crates.io, both resolve cleanly with takumi. Requires a C compiler at build time (QuickJS is vendored via `rquickjs-sys`, no system libraries needed).

### How it works

1. **On layout file load or change**: OXC transforms the `.jsx` source into plain JS (`<text tw="...">x</text>` → `_jsx("text", { tw: "..." }, x)`). Result is compiled to QuickJS bytecode and cached in memory.
2. **On each data tick**: The `Runtime` and `Context` are kept alive between ticks. Updated globals are injected (`ctx`, stream values), then the cached bytecode is re-executed. No reparse, no recompile. Returns a JS object tree. Rust walks the tree to build takumi nodes.

OXC transform: **<0.1ms** (file-change only). QuickJS per-tick bytecode evaluation: **~100–500μs**. Dominant cost is always takumi + skia.

### Sandbox

rquickjs is deny-by-default. No filesystem, network, or process access unless explicitly registered from Rust. costae exposes only `useStringStream`, `useJSONStream`, and `ctx`.

### Example layout file

```jsx
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

function Card({ label, content }) {
  return (
    <container tw="flex flex-col gap-1 rounded-lg border px-3 py-2">
      <text tw="text-[10px] opacity-60">{label}</text>
      <text tw="text-[14px] text-white">{content}</text>
    </container>
  );
}

export default (
  <container tw="flex flex-col h-full w-full px-4 py-4">
    <Module bin="/home/kantord/.cargo/bin/costae-i3" id={ctx.output} tw="flex-1 w-full" />
    <TimeCard />
    <Card label="DATE" content={useStringStream("/usr/bin/bash", `
      while true; do date +"%b %-d"; sleep 1; done
    `)} />
  </container>
);
```

The layout file is a standard JSX module. The default export is the root node tree. Components are plain functions. No framework, no hooks protocol — `useStringStream` and `useJSONStream` are Rust-registered globals, not React hooks.


## Layout

The layout file is re-evaluated on every module value change. Full script re-evaluation is correct and cheap — QuickJS bytecode eval costs ~100–500μs; the dominant cost is always takumi + skia.

`config.yaml` stays for top-level settings (`width`, `outer_gap`). The `layout:` key is replaced by `layout_file:` pointing to the `.jsx` file.


## Nodes

Low-level nodes map directly to takumi nodes. The script produces a JS object tree that Rust walks to construct the takumi node tree. No intermediate representation.

`_jsx` is registered from Rust as a global. It receives `(tag, props, ...children)` and returns a plain JS object `{ tag, props, children }`. Rust reads this tree after evaluation.

Node types: `container`, `text`, `image`, `float-slot` (invisible box that reserves space and provides a bounding box for float stacks).


## Components

Components are plain JS functions that take props and return a node tree. JSX handles `<Card />` as a function call naturally — no registration needed.

### Global context

`ctx` is injected by Rust before script evaluation. Read-only.

| field | description |
|---|---|
| `ctx.output` | RANDR output name, e.g. `"DP-4"` |
| `ctx.dpi` | |
| `ctx.width` | |
| `ctx.outer_gap` | |
| `ctx.state` | map for ad-hoc state keyed by user-defined strings |

### State

No built-in local component state. Users can store ad-hoc state in `ctx.state["unique-key"]`. Not convenient but sufficient — defer until there is a concrete use case.


## Data layer

All external data flows through the data layer. Three calling conventions, same subprocess registry underneath.

### Subprocess identity

The identity key is `(bin, script, id)`. `id` is optional, defaults to empty string. The user supplies a unique `id` when multiple instances of the same `(bin, script)` must run in parallel (e.g. one per monitor).

On each re-evaluation, Rust diffs the old identity set against the new one. Unchanged identities reuse the running subprocess. Removed ones are killed. New ones are spawned.

### `useStringStream(bin, script?, opts?)`

Returns the latest stdout line as a string. Can be called at the top level or inside a component.

```jsx
const time = useStringStream("/usr/bin/bash", `
  while true; do date +"%H:%M"; sleep 1; done
`);
```

### `useJSONStream(bin, script?, opts?)`

Returns the latest stdout line parsed as a JS object.

```jsx
const data = useJSONStream("/usr/bin/myscript");
// data.temperature, data.humidity, etc.
```

### `<Module>`

An external process that outputs a node tree as JSON. costae inserts the subtree directly into the layout — like a component that runs in an external process. Language-agnostic. The subprocess prints JSON lines to stdout where each line is a complete node tree.

```jsx
<Module bin="/home/kantord/.cargo/bin/costae-i3" id={ctx.output} tw="flex-1 w-full" />
```


## Rendering

### Stacks

A window contains one base takumi stack plus zero or more float stacks. Each stack has its own render cache keyed by its serialized subtree — only stacks whose subtree changed are re-rendered by takumi. Compositing is a cheap tiny-skia blit.

### Float positioning

The base stack declares a `<float-slot name="foo" tw="..." />` — an invisible box that participates in layout normally. After `measure_layout`, Rust walks the `MeasuredNode` tree to extract each slot's bounding box `(x, y, w, h)`. The matching float stack renders into a viewport of exactly that size. Bounding boxes are cached and only re-queried when the base stack re-renders.

```jsx
// Base stack:
export default (
  <container tw="flex flex-col h-full w-full">
    <WorkspaceList />
    <float-slot name="clock" tw="w-full h-[60px]" />
  </container>
);

// Float stack:
export default (
  <float target="clock">
    <ClockWidget />
  </float>
);
```

### Compositing

Currently: opaque overdraw — float blits on top of base.

Future: RGBA pipeline + tiny-skia `SourceOver` blend mode for true float transparency. Self-contained change in the render output stage, no architectural impact.

### Caching

Each stack's cache key is its serialized subtree. If identical to the previous render, the cached pixel buffer is reused. This is the granularity mechanism — the user controls what re-renders independently by how they structure their stacks.


## Windows

Currently: single X11 window per costae instance.

Future: multiple windows declared in the layout file. Each window is an independent X11 window with its own stack set. All share the same scripting context, module registry, and subprocess pool.


## Implementation phases

1. **Scripting engine** — add `rquickjs` + `oxc_transformer`; on file load run OXC JSX transform and compile to QuickJS bytecode; keep `Runtime` and `Context` alive between ticks; on each tick inject updated globals and re-execute cached bytecode; register `_jsx` and `ctx`; walk returned JS object tree to construct takumi node tree
2. **Data layer** — register `useStringStream`, `useJSONStream`, `<Module>`; subprocess reconciliation
3. **Multi-stack** — float stacks, slot positioning, per-stack caching
4. **Multi-window** — multiple X11 windows declared in layout
5. **Float transparency** — RGBA compositing pipeline
