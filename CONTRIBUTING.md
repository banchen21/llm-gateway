# Contributing

Thanks for looking. This is a small, single-purpose project; the notes below are the
whole process.

## Before you start

- **Protocol changes**: say which of the four combinations you exercised — Anthropic or
  OpenAI in, Anthropic or OpenAI upstream. They are independent by design, and a change
  that only works for one is usually a bug.
- **New dependencies**: the point of the gateway is to be a small thing you can leave
  running. Prefer the standard library and what is already in `Cargo.toml`.
- **Secrets**: never commit a real API key, and redact them in issues and screenshots.

## Layout

```
crates/gateway-core/     protocol logic and configuration; no HTTP server
  ir/                    the one representation every converter shares
  convert/               protocol JSON and SSE <-> the IR
  upstream/              the IR -> an upstream request, and back
crates/gateway-server/   the axum service: endpoints, admin API, logging, GUI assets
apps/desktop/            the Tauri shell and the Vue 3 GUI (Element Plus)
```

A request always flows `client protocol -> IR -> upstream protocol`; a response always
flows `upstream SSE -> ModelEvent -> client SSE`. Converters never know which protocol is
on the other side, which is what keeps the four combinations from needing four code paths.

## Build and test

```bash
cargo test --workspace                 # protocol and server tests
cargo clippy --workspace --all-targets
cargo fmt --check

cd apps/desktop
pnpm install
pnpm build                             # vue-tsc type check, then vite build
```

The desktop binary embeds `apps/desktop/dist` at compile time, so build the GUI before
`cargo build -p llm-gateway-desktop`, or the window will have nothing to load.

The protocol tests replay recorded SSE against a stand-in upstream, so they need neither
credentials nor network access. Add a recorded stream to `crates/gateway-server/tests/`
rather than mocking an HTTP client.

## Commits

- One logical change per commit.
- Present tense, imperative: `add …`, `fix …`, `move …` — not `added` or `fixes`.
- A body that explains *why* when the diff does not make it obvious.

## Reporting bugs

Include the request shape (which client, which protocol), what happened, and what you
expected. The gateway's request log at `<config dir>/logs/requests.jsonl` records model,
status, token counts and prompt shape — never message bodies — so it is safe to attach.
