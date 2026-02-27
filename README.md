# firrtl-source-locator

[![PyPI version](https://img.shields.io/badge/Zed-firrtl%20source%20locator-blue)](https://zed.dev/extensions/firrtl-source-locator)



![](screenshots/p1.webp)

![](screenshots/p2.webp)

A Zed companion LSP extension for Verilog/SystemVerilog that parses FIRRTL/Chisel source locator comments (`@[...]`) and jumps back to Scala source locations.

## Features

- `textDocument/definition`
  - Jump from anywhere inside one locator comment to all mapped Scala locations.
  - Always returns a multi-target list for one `@[...]` block (for picker-based selection in Zed).
  - Supports inherited-path tokens like `:108:21`.
  - Supports multi-column tokens like `:257:{27,31,48,72}`.
- `textDocument/hover`
  - On a locator token, shows a 3-line preview:
    1) mapped source code line
    2) `^` column indicator line
    3) expanded locator path (`path:line:col`)
  - On `// @[` (expanded trigger range), shows a summary of all mapped targets.
    - Each locator entry is rendered as 2 lines (source line + `^` line; multi-column entries share one `^` line).
  - Uses fenced Markdown code blocks with language tags (`scala` / `firrtl` / `verilog` / etc.) for syntax highlighting in hover.

Note: this extension intentionally prioritizes `Go to Definition` for locator blocks (instead of `DocumentLink`) so one click can always produce the multi-target picker.
It now returns `LocationLink` targets with explicit column ranges for each mapped source point.

## Server Resolution Strategy (PATH + GitHub Release)

This extension no longer builds the server on the host machine.

When Zed starts the language server, the extension resolves the server binary in this order:

1. Optional override: `FIRRTL_SOURCE_LOCATOR_SERVER=/absolute/path/to/firrtl-source-locator-server[.exe]`.
2. Search in `$PATH` via `Worktree::which("firrtl-source-locator-server")`.
3. If not found, download a prebuilt binary archive from GitHub release tag `v<extension-version>`.
4. Extract into the extension workdir:
   - `firrtl-source-locator-server-v<version>-<target>/`
5. Launch the resolved binary directly.

Supported release targets:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`

## Error Reporting and Troubleshooting

If startup fails, Zed's language server status will show the reason (missing release tag, missing asset, download error, unsupported platform, etc.).

Common fixes:

```bash
# install your own server binary and expose it in PATH
which firrtl-source-locator-server

# or point the extension to a custom server binary
export FIRRTL_SOURCE_LOCATOR_SERVER=/absolute/path/to/firrtl-source-locator-server
```

If GitHub download fails, verify network access and confirm the release tag `v<extension-version>` includes your platform asset.

## Development

```bash
# check extension wasm crate
cargo check

# run server parser tests
cargo test --manifest-path server/Cargo.toml
```

GitHub workflows:

- `.github/workflows/ci.yml`: extension check + server tests.
- `.github/workflows/release-server.yml`: build and publish release assets for supported targets.

## Use in Zed

1. Open Zed Extensions.
2. Click `Install Dev Extension`.
3. Select this project directory.
4. Open `.sv`/`.v` files and use Go to Definition on `@[...]` tokens.
5. Hover:
   - on one token for single-target details,
   - on `// @[` for all-target summary.
