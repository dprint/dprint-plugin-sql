use std::borrow::Cow;
use std::ops::Range;
use std::ops::RangeInclusive;
use std::path::Path;

use dprint_core::configuration::NewLineKind;
use dprint_core::configuration::resolve_new_line_kind;

use super::configuration::Configuration;
use crate::ast::Item;
use crate::error::FormatError;
use crate::format_text::format_text;
use crate::format_text::has_ignore_file_comment;
use crate::format_text::lex;
use crate::format_text::parse;
use crate::format_text::print_items;
use crate::format_text::strip_bom;
use crate::format_text::verify;

/// Formats only the part of the text within the provided byte range.
///
/// The range is widened to the lines of the statements, comments and commands it touches at the
/// top level of the file, and the text outside of those is left as it was, including the blank
/// lines above and below them, the line endings and any byte order mark. An empty range is a
/// cursor, which touches whatever is on its line. Statements that share a line with a touched one
/// are touched too, since only whole lines are formatted.
///
/// A range that touches both the first and last of the file's statements formats the whole file,
/// and one that only touches blank lines formats nothing.
pub fn format_text_range(
  file_path: &Path,
  text: &str,
  range: Range<usize>,
  config: &Configuration,
) -> Result<Option<String>, FormatError> {
  let body = strip_bom(text);
  let bom_len = text.len() - body.len();
  let end = range.end.saturating_sub(bom_len).min(body.len());
  let range = range.start.saturating_sub(bom_len).min(end)..end;
  if range.start == 0 && range.end == body.len() {
    return format_text(file_path, text, config);
  }

  let tokens = lex(body, config.dialect)?;
  if has_ignore_file_comment(&tokens) {
    return Ok(None);
  }
  let list = parse(&tokens, body, config.dialect)?;
  let items = &list.items;
  let spans: Vec<Range<usize>> = items.iter().map(item_span).collect();
  let Some(touched) = find_touched(body, items, &spans, &range) else {
    return Ok(None);
  };
  if touched == (0..=items.len() - 1) {
    return format_text(file_path, text, config);
  }

  let (first, last) = touched.into_inner();
  let original = lines_range(body, spans[first].start..spans[last].end);
  let formatted = print_items(&items[first..=last], body, config);
  let spliced = format!(
    "{}{}{}",
    &body[..original.start],
    with_file_new_lines(&formatted, body),
    &body[original.end..]
  );
  verify(&tokens, &spliced, config)?;
  let result = format!("{}{}", &text[..bom_len], spliced);
  Ok(if result == text { None } else { Some(result) })
}

/// Where the item starts and ends in the text.
fn item_span(item: &Item) -> Range<usize> {
  match (item.first_token(), item.last_token()) {
    (Some(first), Some(last)) => first.start..last.end(),
    _ => 0..0,
  }
}

/// The indexes of the items the range touches, if any, along with any that share a line with
/// them.
///
/// A cursor touches an item from anywhere on its lines, but a selection needs to hold some of the
/// item itself, so that one ending in the indentation of the next line doesn't format that line.
fn find_touched(
  text: &str,
  items: &[Item],
  spans: &[Range<usize>],
  range: &Range<usize>,
) -> Option<RangeInclusive<usize>> {
  let mut touched = (0..spans.len()).filter(|index| {
    let span = &spans[*index];
    if range.is_empty() {
      let lines = lines_range(text, span.clone());
      lines.start <= range.start && range.start <= lines.end
    } else {
      range.start < span.end && range.end > span.start
    }
  });
  let mut first = touched.next()?;
  let mut last = touched.next_back().unwrap_or(first);
  // only whole lines are formatted, so whatever shares a line with a touched item is formatted too
  while first > 0 && items[first].first_token().is_some_and(|t| t.newlines_before == 0) {
    first -= 1;
  }
  while last + 1 < items.len() && items[last + 1].first_token().is_some_and(|t| t.newlines_before == 0) {
    last += 1;
  }
  Some(first..=last)
}

/// Widens the range to the start of its first line, so that the indentation there is formatted,
/// and to the end of its last line without the line break.
fn lines_range(text: &str, range: Range<usize>) -> Range<usize> {
  let start = text[..range.start].rfind('\n').map(|index| index + 1).unwrap_or(0);
  let rest = &text[range.end..];
  let line = rest.find('\n').map(|index| &rest[..index]).unwrap_or(rest);
  start..range.end + line.trim_end_matches('\r').len()
}

/// Keeps the line endings of the rest of the file since changing those is up to formatting the
/// whole file.
fn with_file_new_lines<'a>(formatted: &'a str, file_text: &str) -> Cow<'a, str> {
  if !file_text.contains('\n') {
    return Cow::Borrowed(formatted);
  }
  match resolve_new_line_kind(file_text, NewLineKind::Auto) {
    "\r\n" if formatted.contains('\n') && !formatted.contains("\r\n") => Cow::Owned(formatted.replace('\n', "\r\n")),
    "\n" if formatted.contains("\r\n") => Cow::Owned(formatted.replace("\r\n", "\n")),
    _ => Cow::Borrowed(formatted),
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::configuration::ConfigurationBuilder;

  fn format_b(config: &Configuration, text: &str) -> String {
    let start = text.find('b').unwrap();
    format_text_range(Path::new("file.sql"), text, start..start + 1, config)
      .unwrap()
      .unwrap()
  }

  #[test]
  fn keeps_bom_outside_range() {
    // the spec files can't express this since editors strip the bom
    let config = ConfigurationBuilder::new().build();
    let output = format_b(&config, "\u{FEFF}select   a;\nselect   b;\nselect   c;\n");
    assert_eq!(output, "\u{FEFF}select   a;\nselect b;\nselect   c;\n");
  }

  #[test]
  fn keeps_file_line_endings() {
    // the spec files can't express this since they normalize line endings
    let config = ConfigurationBuilder::new().build();
    let output = format_b(&config, "select   a;\r\nselect   b   from   t;\r\nselect   c;\r\n");
    assert_eq!(output, "select   a;\r\nselect b\r\nfrom t;\r\nselect   c;\r\n");
  }

  #[test]
  fn keeps_file_line_endings_over_configured_ones() {
    let config = ConfigurationBuilder::new()
      .new_line_kind(NewLineKind::CarriageReturnLineFeed)
      .build();
    let output = format_b(&config, "select   a;\nselect   b   from   t;\nselect   c;\n");
    assert_eq!(output, "select   a;\nselect b\nfrom t;\nselect   c;\n");
  }

  #[test]
  fn formats_statement_cursor_is_in_trailing_whitespace_of() {
    let config = ConfigurationBuilder::new().build();
    let text = "select   a;\nselect   b;   \nselect   c;\n";
    let cursor = text.find(";   \n").unwrap() + 3;
    let output = format_text_range(Path::new("file.sql"), text, cursor..cursor, &config).unwrap();
    assert_eq!(output.as_deref(), Some("select   a;\nselect b;\nselect   c;\n"));
  }

  #[test]
  fn formats_statements_sharing_a_line() {
    let config = ConfigurationBuilder::new().build();
    let output = format_b(&config, "select   a;\nselect   b;  select   x;\nselect   c;\n");
    assert_eq!(output, "select   a;\nselect b;\nselect x;\nselect   c;\n");
  }

  #[test]
  fn formats_last_statement_without_adding_final_newline() {
    let config = ConfigurationBuilder::new().build();
    let output = format_b(&config, "select   a;\nselect   b;");
    assert_eq!(output, "select   a;\nselect b;");
  }

  #[test]
  fn does_not_format_at_end_of_file() {
    let config = ConfigurationBuilder::new().build();
    let text = "select   a;\nselect   b;\n\n";
    let output = format_text_range(Path::new("file.sql"), text, text.len()..text.len(), &config).unwrap();
    assert_eq!(output, None);
  }

  #[test]
  fn does_not_format_when_already_formatted() {
    let config = ConfigurationBuilder::new().build();
    let text = "select   a;\nselect b;\nselect   c;\n";
    let start = text.find('b').unwrap();
    let output = format_text_range(Path::new("file.sql"), text, start..start + 1, &config).unwrap();
    assert_eq!(output, None);
  }

  #[test]
  fn formats_whole_file_when_touching_first_and_last_statements() {
    let config = ConfigurationBuilder::new().build();
    let output = format_text_range(Path::new("file.sql"), "\n\nselect   a;\nselect   b;", 3..20, &config).unwrap();
    assert_eq!(output.as_deref(), Some("select a;\nselect b;\n"));
  }

  #[test]
  fn clamps_range_past_end_of_file() {
    let config = ConfigurationBuilder::new().build();
    let text = "select   a;\nselect   b;\n\n";
    let output = format_text_range(Path::new("file.sql"), text, 14..500, &config).unwrap();
    assert_eq!(output.as_deref(), Some("select   a;\nselect b;\n\n"));
    let output = format_text_range(Path::new("file.sql"), text, 400..500, &config).unwrap();
    assert_eq!(output, None);
  }

  #[test]
  fn errors_for_syntax_error_outside_range() {
    let config = ConfigurationBuilder::new().build();
    let result = format_text_range(Path::new("file.sql"), "select   a;\nselect 'b\n", 0..1, &config);
    assert!(result.unwrap_err().to_string().contains("unterminated string"));
  }

  #[test]
  fn formats_procedure_touched_within_its_body() {
    let config = ConfigurationBuilder::new().build();
    let text = "select   a;\ncreate procedure p() begin\nselect   1;\nend;\nselect   c;\n";
    let cursor = text.find("1;").unwrap();
    let output = format_text_range(Path::new("file.sql"), text, cursor..cursor, &config).unwrap();
    assert_eq!(
      output.as_deref(),
      Some("select   a;\ncreate procedure p()\nbegin\n  select 1;\nend;\nselect   c;\n")
    );
  }
}
