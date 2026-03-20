# dprint-plugin-sql

Wrapper around [sqlformat-rs](https://github.com/shssoichiro/sqlformat-rs) for use as a dprint formatting plugin.

## Commands

- `cargo test` — run all tests
- `cargo build --target wasm32-unknown-unknown --features wasm --release` — build the wasm plugin
- `cd deployment/npm && node setup.js && npm test` — test the npm package (requires wasm build first)

## Architecture

- `src/format_text.rs` — core formatting logic, delegates to `sqlformat::format()`
- `src/wasm_plugin.rs` — dprint wasm plugin interface (compiled only for wasm32 target with `wasm` feature)
- `src/configuration/` — config types, resolution, and builder
- `deployment/npm/` — npm package that ships `plugin.wasm`
- `deployment/schema.json` — JSON schema for plugin configuration
- `scripts/update.ts` — Deno script that checks crates.io for new sqlformat versions and auto-publishes
- `scripts/generateReleaseNotes.ts` — generates changelog for GitHub releases

## Adding a new config option

When sqlformat-rs adds new options:

1. Add the field to `Configuration` in `src/configuration/configuration.rs` (add enums with `generate_str_to_from!` if needed)
2. Add resolution in `src/configuration/resolve_config.rs` using `get_value()`
3. Add a builder method in `src/configuration/builder.rs` and update the `check_all_values_set` test
4. Wire it through in `src/format_text.rs` when constructing `FormatOptions`
5. Add the property to `deployment/schema.json`
6. Add a spec test file in `tests/specs/Config/`

## Test specs

Spec files live in `tests/specs/` and use the dprint-development format:

- `== message ==` starts a test case
- `[expect]` separates input from expected output
- `~~ key: value, key: value ~~` at the top of a file sets config for all specs in that file
- Each config variation needs its own file since config is file-level

## Releases

- Manual: trigger the `release` workflow with patch/minor
- Automatic: `check_updates` workflow runs daily, detects new sqlformat versions, and auto-publishes
- On tag push: CI creates a GitHub release with `plugin.wasm`, and `publish_npm` publishes to npm
