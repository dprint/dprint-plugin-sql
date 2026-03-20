use super::configuration::Configuration;

use anyhow::Result;
use dprint_core::configuration::resolve_new_line_kind;
use sqlformat::FormatOptions;
use sqlformat::Indent;
use sqlformat::QueryParams;
use std::path::Path;

pub fn format_text(_file_path: &Path, text: &str, config: &Configuration) -> Result<Option<String>> {
  let input_text = text;
  let text = sqlformat::format(
    text,
    &QueryParams::None,
    &FormatOptions {
      indent: if config.use_tabs {
        Indent::Tabs
      } else {
        Indent::Spaces(config.indent_width)
      },
      uppercase: Some(config.uppercase),
      lines_between_queries: config.lines_between_queries,
      ..Default::default()
    },
  );

  // ensure ends with newline
  let text = if !text.ends_with('\n') {
    let mut text = text;
    text.push('\n');
    text
  } else {
    text
  };

  // newline
  let text = if resolve_new_line_kind(&text, config.new_line_kind) == "\n" {
    text.replace("\r\n", "\n")
  } else {
    // lazy
    text.replace("\r\n", "\n").replace("\n", "\r\n")
  };

  if text == input_text {
    Ok(None)
  } else {
    Ok(Some(text))
  }
}
