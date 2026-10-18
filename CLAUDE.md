# dprint-plugin-sql

SQL formatting plugin for dprint, with its own lexer, parser and formatter that build dprint-core's IR.

## Commands

- `cargo test` — run all tests
- `cargo test --test specs` — run only the spec tests
- `cargo clippy --all-targets --all-features -- -D warnings` — lint
- `cargo build --target wasm32-unknown-unknown --features wasm --release` — build the wasm plugin
- `cd deployment/npm && node setup.js && npm test` — test the npm package (requires wasm build first)
- `deno run -A scripts/corpus.ts` — format the SQL files of real projects to find bugs (see [Corpus](#corpus))

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
- `scripts/corpus.ts` and `examples/corpus.rs` — the corpus of real projects' SQL files and its runner

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

## Corpus

`deno run -A scripts/corpus.ts` fetches the `.sql` files of the projects pinned in `scripts/corpus.ts` into `.corpus/`, and formats each in its own dialect. It reports any file that panics, is refused (formatting would have changed more than whitespace), is unstable or times out, and exits with an error if any do. It also lists slow files. Files that don't parse in their dialect aren't problems, and `--verbose` lists them.

- `--all-dialects` formats every file in every dialect, which finds the bugs only a dialect a file isn't written in shows
- `--line-width 30,120` formats at several widths, since a narrow width breaks far more
- `--compare main` also formats with the formatter at a git ref and writes the differences in output to `.corpus/compare.diff`, to review how a change formats real code
- project names (ex. `pgtap sakila`) limit it to those projects

It isn't run in CI. Run it after changing the lexer, parser or layout, and before a release. When it finds a bug, add a spec reproducing it along with the fix. To add a project, add it to the list with a pinned commit and the dialect of its files.

## Releases

- Manual: trigger the `release` workflow with patch/minor
- On tag push: CI creates a GitHub release with `plugin.wasm`, and `publish_npm` publishes to npm
