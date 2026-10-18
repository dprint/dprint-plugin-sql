# dprint-plugin-sql

SQL formatting plugin for dprint, with its own lexer, parser and formatter that build dprint-core's IR.

## Commands

- `cargo test` — run all tests
- `cargo test --test specs` — run only the spec tests
- `cargo clippy --all-targets --all-features -- -D warnings` — lint
- `cargo build --target wasm32-unknown-unknown --features wasm --release` — build the wasm plugin
- `cd deployment/npm && node setup.js && npm test` — test the npm package (requires wasm build first)

## Architecture

The pipeline is lex → parse → generate IR → print → verify.

- `src/lexer.rs` — splits the text into tokens, losslessly. This is where the dialects differ (`#`, `[..]`, backslash escapes, dollar quotes, `GO`, `DELIMITER`, client commands)
- `src/parser.rs` — builds statements, procedural blocks (`BEGIN .. END`, `IF`, loops, `CASE` statements, declaration sections), brackets and `CASE` expressions. Also splits T-SQL statements written without semicolons
- `src/ast.rs` — the tree the parser builds
- `src/generation/generate.rs` — the layout: splits statements into clauses, lists and conditions, and generates the IR
- `src/keywords.rs` — sorted tables of keywords
- `src/format_text.rs` — formatting a file, and the verification that formatting changed nothing but whitespace (keyword casing aside), which errors rather than return changed code
- `src/format_range.rs` — formatting the top level statements a range touches
- `src/wasm_plugin.rs` — dprint wasm plugin interface (compiled only for wasm32 target with `wasm` feature)
- `src/configuration/` — config types, resolution, and builder
- `deployment/npm/` — npm package that ships `plugin.wasm`
- `deployment/schema.json` — JSON schema for plugin configuration
- `scripts/generateReleaseNotes.ts` — generates changelog for GitHub releases

## Principles

- Formatting only ever changes the whitespace between tokens. Anything the parser doesn't understand is written as a run of tokens rather than reported, since the dialect it's in may use those words differently
- When the dialects disagree on what some text is, the configured dialect decides. The generic dialect must pick what's least likely to be wrong, since a wrong guess there is the most likely kind of bug
- Formatting must be stable: formatting the output again gives the same output. Every spec is formatted twice to check this, and `tests/test.rs` also formats every spec's input in every dialect at two widths and checks it's stable and changes nothing but whitespace

## Adding a new config option

1. Add the field to `Configuration` in `src/configuration/configuration.rs` (add enums with `generate_str_to_from!` if needed)
2. Add resolution in `src/configuration/resolve_config.rs` using `get_value()`
3. Add a builder method in `src/configuration/builder.rs` and update the `check_all_values_set` test
4. Use it in `src/generation/generate.rs`
5. Add the property to `deployment/schema.json` and the table in `README.md`
6. Add a spec test file in `tests/specs/Config/`

## Test specs

Spec files live in `tests/specs/` and use the dprint-development format:

- `== message ==` starts a test case
- `[expect]` separates input from expected output
- `~~ key: value, key: value ~~` at the top of a file sets config for all specs in that file
- Each config variation needs its own file since config is file-level, which is why each dialect has its own directory
- `[|` and `|]` mark the range to format in a spec's text
- A spec file can't start with a SQL comment, since a first line starting with `--` names the spec's file

## Releases

- Manual: trigger the `release` workflow with patch/minor
- On tag push: CI creates a GitHub release with `plugin.wasm`, and `publish_npm` publishes to npm
