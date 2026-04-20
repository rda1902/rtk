# Elixir

> Part of [`src/cmds/`](../README.md) — see also [docs/contributing/TECHNICAL.md](../../../docs/contributing/TECHNICAL.md)

## Specifics

- `mix_test_cmd.rs` filters ExUnit output via `mix test`; state machine text parser, failures only (60%+ reduction)
- TOML filters `mix-compile.toml` and `mix-format.toml` handle `mix compile` and `mix format` (simple line-based)
