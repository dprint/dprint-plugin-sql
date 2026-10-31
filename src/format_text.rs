use std::path::Path;

use dprint_core::configuration::NewLineKind;
use dprint_core::configuration::resolve_new_line_kind;
use dprint_core::formatting::Arena;
use dprint_core::formatting::ArenaPool;
use dprint_core::formatting::PrintOptions;
use dprint_core::formatting::bumpalo::Bump;

use crate::ast::Item;
use crate::ast::StatementList;
use crate::configuration::Configuration;
use crate::configuration::Dialect;
use crate::configuration::UppercaseKind;
use crate::error::FormatError;
use crate::error::ParseError;
use crate::generation::Context;
use crate::generation::generate;
use crate::generation::generate_items;
use crate::lexer;
use crate::lexer::Token;
use crate::lexer::TokenKind;
use crate::parser;

pub fn format_text(file_path: &Path, text: &str, config: &Configuration) -> Result<Option<String>, FormatError> {
  format_text_with_arena_pool(file_path, text, config, None)
}

/// Formats the text, creating the arena from the pool when one is provided so its memory is
/// reused between formats.
pub(crate) fn format_text_with_arena_pool(
  _file_path: &Path,
  text: &str,
  config: &Configuration,
  arena_pool: Option<&ArenaPool>,
) -> Result<Option<String>, FormatError> {
  let result = format_text_inner(text, config, arena_pool)?;
  match result {
    Some(result) if result != text => Ok(Some(result)),
    _ => Ok(None),
  }
}

/// The formatted text, or `None` when a `dprint-ignore-file` comment says to leave it alone.
fn format_text_inner(
  text: &str,
  config: &Configuration,
  arena_pool: Option<&ArenaPool>,
) -> Result<Option<String>, FormatError> {
  let text = strip_bom(text);
  let arena = create_arena(arena_pool);
  let tokens = lex(text, config.dialect, &arena)?;
  if has_ignore_file_comment(tokens) {
    return Ok(None);
  }
  let list = parse(tokens, text, config.dialect, &arena)?;
  let result = print(&list, text, config, &arena);
  verify(tokens, &result, config, &arena)?;
  Ok(Some(result))
}

/// Formats SQL embedded in another file's SQL, such as the body of a PL/pgSQL function, with line
/// feeds for its line breaks. Returns `None` when it can't be formatted, in which case it should
/// be left as it is.
pub(crate) fn format_embedded(text: &str, config: &Configuration) -> Option<String> {
  let mut config = config.clone();
  config.new_line_kind = NewLineKind::LineFeed;
  let arena = Arena::new();
  let tokens = lexer::lex(text, config.dialect, arena.bump()).ok()?;
  if has_ignore_file_comment(tokens) {
    return None;
  }
  let list = parser::parse(tokens, config.dialect, arena.bump()).ok()?;
  let result = print(&list, text, &config, &arena);
  verify(tokens, &result, &config, &arena).ok()?;
  Some(result)
}

pub(crate) fn print<'a>(
  list: &StatementList<'a>,
  text: &'a str,
  config: &'a Configuration,
  arena: &'a Arena,
) -> String {
  let mut context = Context::new(arena, config, text);
  let print_items = generate(list, &mut context);
  dprint_core::formatting::print(print_items, config_to_print_options(text, config))
}

/// Prints the items of the top level statement list, as they're printed when formatting the whole
/// file, but without a newline at the end.
pub(crate) fn print_items<'a>(
  items: &[Item<'a>],
  text: &'a str,
  config: &'a Configuration,
  arena: &'a Arena,
) -> String {
  let mut context = Context::new(arena, config, text);
  let print_items = generate_items(items, &mut context);
  dprint_core::formatting::print(print_items, config_to_print_options(text, config))
}

#[cfg(feature = "tracing")]
pub fn trace_file(_file_path: &Path, text: &str, config: &Configuration) -> dprint_core::formatting::TracingResult {
  let text = strip_bom(text);
  let arena = Arena::new();
  let tokens = lexer::lex(text, config.dialect, arena.bump()).unwrap();
  let list = parser::parse(tokens, config.dialect, arena.bump()).unwrap();
  let mut context = Context::new(&arena, config, text);
  let print_items = generate(&list, &mut context);
  dprint_core::formatting::trace_printing(print_items, config_to_print_options(text, config))
}

pub(crate) fn strip_bom(text: &str) -> &str {
  text.strip_prefix("\u{FEFF}").unwrap_or(text)
}

pub(crate) fn lex<'t, 'b>(text: &'t str, dialect: Dialect, arena: &'b Arena) -> Result<&'b [Token<'t>], ParseError> {
  lexer::lex(text, dialect, arena.bump()).map_err(|err| {
    let mut message = err.message;
    let is_quote_error = message.starts_with("unterminated string") || message.starts_with("unterminated quoted identifier");
    if is_quote_error && !matches!(dialect, Dialect::MySql | Dialect::BigQuery | Dialect::Snowflake) {
      message.push_str(" (if the file is written in a dialect that escapes quotes with a backslash, such as MySQL, set the \"dialect\" option)");
    }
    ParseError::new(render_diagnostic(text, err.span, &message))
  })
}

pub(crate) fn parse<'a>(
  tokens: &'a [Token<'a>],
  text: &str,
  dialect: Dialect,
  arena: &'a Arena,
) -> Result<StatementList<'a>, ParseError> {
  parser::parse(tokens, dialect, arena.bump())
    .map_err(|err| ParseError::new(render_diagnostic(text, err.span, &err.message)))
}

pub(crate) fn has_ignore_file_comment(tokens: &[Token]) -> bool {
  tokens
    .iter()
    .take_while(|token| token.is_comment())
    .any(|token| dprint_core::formatting::ir_helpers::text_has_dprint_ignore(token.text, "dprint-ignore-file"))
}

fn render_diagnostic(text: &str, span: std::ops::Range<usize>, message: &str) -> String {
  let (start, end) = highlight_range(text, span);
  dprint_core::formatting::utils::string_utils::format_diagnostic(Some((start, end)), message, text)
}

/// The range to underline beneath the offending line, which the diagnostic only draws when it is
/// within a single line and at least one character wide.
fn highlight_range(text: &str, span: std::ops::Range<usize>) -> (usize, usize) {
  let start = span.start.min(text.len());
  let line_end = text[start..].find('\n').map(|i| start + i).unwrap_or(text.len());
  let end = span.end.min(line_end);
  if end > start {
    (start, end)
  } else {
    match text[start..].chars().next() {
      Some(c) => (start, start + c.len_utf8()),
      None => (
        text[..start]
          .chars()
          .next_back()
          .map(|c| start - c.len_utf8())
          .unwrap_or(start),
        start,
      ),
    }
  }
}

pub(crate) fn create_arena(arena_pool: Option<&ArenaPool>) -> Arena {
  match arena_pool {
    Some(arena_pool) => arena_pool.create_arena(),
    None => Arena::new(),
  }
}

fn config_to_print_options(text: &str, config: &Configuration) -> PrintOptions {
  PrintOptions {
    indent_width: config.indent_width,
    max_width: config.line_width,
    use_tabs: config.use_tabs,
    new_line_text: resolve_new_line_kind(text, config.new_line_kind),
  }
}

// ---- verification ----

/// Checks that the formatted text holds the same tokens as the input, in the same order, so that
/// formatting only ever changes the whitespace between them. The exceptions are the casing of
/// keywords when the `casing` option changes it, the trailing whitespace of line comments, and the
/// code in a routine's body, which is compared the same way.
pub(crate) fn verify(
  original: &[Token],
  formatted: &str,
  config: &Configuration,
  arena: &Arena,
) -> Result<(), FormatError> {
  let bump = arena.bump();
  let formatted_tokens = lexer::lex(formatted, config.dialect, bump).map_err(|_| FormatError::WouldChangeCode)?;
  if tokens_equivalent(original, formatted_tokens, config, bump) {
    Ok(())
  } else {
    Err(FormatError::WouldChangeCode)
  }
}

fn tokens_equivalent(a: &[Token], b: &[Token], config: &Configuration, bump: &Bump) -> bool {
  // a template tag on a line of its own is like a comment to the formatter, but it may hold code
  // (ex. `{% if x %}`), so it's compared in its place among the code
  let is_comment = |t: &&Token| matches!(t.kind, TokenKind::LineComment | TokenKind::BlockComment);
  let mut a_code = a.iter().filter(|t| !is_comment(t));
  let mut b_code = b.iter().filter(|t| !is_comment(t));
  let mut previous_is_string = false;
  loop {
    match (a_code.next(), b_code.next()) {
      (None, None) => break,
      (Some(a), Some(b)) if token_equivalent(a, b, config, bump) => {
        // PostgreSQL only joins strings that are separated by a line break
        if previous_is_string && a.kind == TokenKind::String && a.starts_line() != b.starts_line() {
          return false;
        }
        previous_is_string = a.kind == TokenKind::String;
      }
      _ => return false,
    }
  }
  let a_comments = a.iter().filter(is_comment).map(comment_text);
  let b_comments = b.iter().filter(is_comment).map(comment_text);
  a_comments.eq(b_comments)
}

fn comment_text<'a>(token: &'a Token) -> std::borrow::Cow<'a, str> {
  let text = match token.kind {
    TokenKind::LineComment | TokenKind::TemplateLine | TokenKind::Template => token.text.trim_end_matches([' ', '\t']),
    // a SQLite comment may run to the end of the file, which the formatter ends with a newline
    TokenKind::BlockComment if !token.text.ends_with("*/") => token.text.trim_end_matches(lexer::is_space),
    _ => token.text,
  };
  normalize_newlines(text)
}

fn token_equivalent(a: &Token, b: &Token, config: &Configuration, bump: &Bump) -> bool {
  // whether a template tag is on a line of its own only changes how it's formatted
  let is_template = |t: &Token| matches!(t.kind, TokenKind::Template | TokenKind::TemplateLine);
  if is_template(a) && is_template(b) {
    return comment_text(a) == comment_text(b);
  }
  if a.kind != b.kind {
    return false;
  }
  if normalize_newlines(a.text) == normalize_newlines(b.text) {
    return true;
  }
  match a.kind {
    TokenKind::Word => match config.uppercase {
      UppercaseKind::Preserve => false,
      UppercaseKind::Upper => b.text == a.text.to_ascii_uppercase(),
      UppercaseKind::Lower => b.text == a.text.to_ascii_lowercase(),
    },
    TokenKind::Directive | TokenKind::BatchSeparator => {
      a.text.trim_end_matches(lexer::is_space) == b.text.trim_end_matches(lexer::is_space)
    }
    TokenKind::DollarString => dollar_string_bodies_equivalent(a.text, b.text, config, bump),
    _ => false,
  }
}

fn dollar_string_bodies_equivalent(a: &str, b: &str, config: &Configuration, bump: &Bump) -> bool {
  let tag_len = |text: &str| text[1..].find('$').map(|i| i + 2).unwrap_or(text.len());
  let (a_tag_len, b_tag_len) = (tag_len(a), tag_len(b));
  if a[..a_tag_len] != b[..b_tag_len] || a.len() < a_tag_len * 2 || b.len() < b_tag_len * 2 {
    return false;
  }
  let a_body = &a[a_tag_len..a.len() - a_tag_len];
  let b_body = &b[b_tag_len..b.len() - b_tag_len];
  match (
    lexer::lex(a_body, config.dialect, bump),
    lexer::lex(b_body, config.dialect, bump),
  ) {
    (Ok(a_tokens), Ok(b_tokens)) => tokens_equivalent(a_tokens, b_tokens, config, bump),
    _ => false,
  }
}

fn normalize_newlines(text: &str) -> std::borrow::Cow<'_, str> {
  if text.contains('\r') {
    std::borrow::Cow::Owned(text.replace("\r\n", "\n"))
  } else {
    std::borrow::Cow::Borrowed(text)
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::configuration::ConfigurationBuilder;

  #[test]
  fn strips_bom() {
    let config = ConfigurationBuilder::new().build();
    let file_text = format_text(Path::new("file.sql"), "\u{FEFF}SELECT 1;\n", &config).unwrap();
    assert_eq!(file_text.unwrap(), "SELECT 1;\n");
  }

  #[test]
  fn verification_rejects_changed_tokens() {
    let arena = Arena::new();
    let config = ConfigurationBuilder::new().build();
    let tokens = lexer::lex("SELECT a FROM t", Dialect::Generic, arena.bump()).unwrap();
    assert!(verify(tokens, "SELECT a\nFROM t\n", &config, &arena).is_ok());
    assert!(verify(tokens, "SELECT b FROM t", &config, &arena).is_err());
    assert!(verify(tokens, "SELECT a FROM", &config, &arena).is_err());
    assert!(verify(tokens, "select a from t", &config, &arena).is_err());
    let upper = ConfigurationBuilder::new().casing(UppercaseKind::Upper).build();
    let lower_tokens = lexer::lex("select a from t", Dialect::Generic, arena.bump()).unwrap();
    assert!(verify(lower_tokens, "SELECT a FROM t", &upper, &arena).is_ok());
    // a word's case only changes to the configured one
    assert!(verify(lower_tokens, "Select a from t", &upper, &arena).is_err());
    assert!(verify(tokens, "select a from t", &upper, &arena).is_err());
  }

  #[test]
  fn verification_compares_comments() {
    let arena = Arena::new();
    let config = ConfigurationBuilder::new().build();
    let tokens = lexer::lex("SELECT a -- x  \nFROM t", Dialect::Generic, arena.bump()).unwrap();
    assert!(verify(tokens, "SELECT a -- x\nFROM t", &config, &arena).is_ok());
    assert!(verify(tokens, "SELECT a -- y\nFROM t", &config, &arena).is_err());
    assert!(verify(tokens, "SELECT a\nFROM t", &config, &arena).is_err());
  }

  #[test]
  fn verification_compares_line_breaks_between_strings() {
    let arena = Arena::new();
    let config = ConfigurationBuilder::new().dialect(Dialect::PostgreSql).build();
    let tokens = lexer::lex("SELECT 'a'\n'b'", Dialect::PostgreSql, arena.bump()).unwrap();
    assert!(verify(tokens, "SELECT\n  'a'\n  'b'", &config, &arena).is_ok());
    assert!(verify(tokens, "SELECT 'a' 'b'", &config, &arena).is_err());
  }

  #[test]
  fn verification_compares_template_tags_in_place() {
    let arena = Arena::new();
    let config = ConfigurationBuilder::new().build();
    let tokens = lexer::lex("SELECT a\n{% if x %}\n, b\n{% endif %}", Dialect::Generic, arena.bump()).unwrap();
    // a tag may move onto or off a line of its own
    assert!(verify(tokens, "SELECT a {% if x %}, b\n{% endif %}", &config, &arena).is_ok());
    // but not across the code around it
    assert!(verify(tokens, "SELECT a,\n{% if x %}\nb\n{% endif %}", &config, &arena).is_err());
  }

  #[test]
  fn verification_compares_routine_bodies_as_code() {
    let arena = Arena::new();
    let config = ConfigurationBuilder::new().dialect(Dialect::PostgreSql).build();
    let tokens = lexer::lex("DO $$ BEGIN   NULL; END $$", Dialect::PostgreSql, arena.bump()).unwrap();
    assert!(verify(tokens, "DO $$\nBEGIN\n  NULL;\nEND\n$$", &config, &arena).is_ok());
    assert!(verify(tokens, "DO $$ BEGIN NULL END $$", &config, &arena).is_err());
    assert!(verify(tokens, "DO $x$ BEGIN NULL; END $x$", &config, &arena).is_err());
  }
}
