use std::path::Path;
use std::path::PathBuf;

use dprint_plugin_sql::configuration::ConfigurationBuilder;
use dprint_plugin_sql::configuration::Dialect;
use dprint_plugin_sql::configuration::UppercaseKind;
use dprint_plugin_sql::*;

#[test]
fn should_handle_windows_newlines() {
  let config = ConfigurationBuilder::new().build();
  let file_text = format_text(&PathBuf::from("file.sql"), "SELECT * FROM  dbo.Test\r\n", &config).unwrap();

  assert_eq!(file_text.unwrap(), "SELECT *\nFROM dbo.Test\n");
}

#[test]
fn should_keep_windows_newlines_in_auto_mode() {
  let config = ConfigurationBuilder::new()
    .new_line_kind(dprint_core::configuration::NewLineKind::Auto)
    .build();
  let file_text = format_text(&PathBuf::from("file.sql"), "SELECT * FROM  dbo.Test\r\n", &config).unwrap();

  assert_eq!(file_text.unwrap(), "SELECT *\r\nFROM dbo.Test\r\n");
}

#[test]
fn should_format_empty_file() {
  let config = ConfigurationBuilder::new().build();
  assert_eq!(format_text(&PathBuf::from("file.sql"), "", &config).unwrap(), None);
  assert_eq!(
    format_text(&PathBuf::from("file.sql"), "\n\n", &config)
      .unwrap()
      .as_deref(),
    Some("")
  );
}

#[test]
fn should_leave_file_with_ignore_file_comment() {
  let config = ConfigurationBuilder::new().build();
  let text = "-- dprint-ignore-file\nSELECT   *   FROM t;\n";
  assert_eq!(format_text(&PathBuf::from("file.sql"), text, &config).unwrap(), None);
  let range = format_text_range(&PathBuf::from("file.sql"), text, 25..26, &config).unwrap();
  assert_eq!(range, None);
}

/// Nesting deeply enough to run out of stack aborts the process rather than raising an error, so
/// the parser refuses well before that.
#[test]
fn should_reject_deeply_nested_brackets() {
  let config = ConfigurationBuilder::new().build();
  let text = format!("SELECT {}1{}", "(".repeat(500), ")".repeat(500));
  let error = format_text(&PathBuf::from("file.sql"), &text, &config)
    .unwrap_err()
    .to_string();
  assert!(error.contains("nested too deeply"), "got {error}");
  // a depth a real query might plausibly reach is still accepted
  let nested = format!("SELECT {}1{}", "(".repeat(32), ")".repeat(32));
  assert!(format_text(&PathBuf::from("file.sql"), &nested, &config).is_ok());
}

#[test]
fn should_reject_deeply_nested_blocks() {
  let config = ConfigurationBuilder::new().dialect(Dialect::PostgreSql).build();
  let text = format!("DO $$ {} NULL; {} $$", "BEGIN ".repeat(300), "END; ".repeat(300));
  // the body of the `DO` is left as it is when it can't be formatted
  assert!(format_text(&PathBuf::from("file.sql"), &text, &config).is_ok());
  let text = format!("{} NULL; {}", "BEGIN ".repeat(300), "END; ".repeat(300));
  let error = format_text(&PathBuf::from("file.sql"), &text, &config)
    .unwrap_err()
    .to_string();
  assert!(error.contains("nested too deeply"), "got {error}");
}

#[test]
fn should_quickly_format_many_blocks_without_an_end() {
  // outside of PostgreSQL, `end$$` is one word, so none of these blocks are ended, which once took
  // time exponential in their number to find out
  let texts = [
    "do $$ declare a int[];
begin a := 1; raise notice 'a = %', a; end$$;
",
    "create function f(in i int, out j int) as $$
begin
  j := i;
  return;
end$$ language plpgsql;
select * from f(42);
",
    "create function f(a text) returns int as $$
declare x int;
begin
  if found then return x; end if;
  return 0;
end$$ language plpgsql stable;
",
  ];
  for dialect in [
    Dialect::Generic,
    Dialect::PostgreSql,
    Dialect::MySql,
    Dialect::SqlServer,
    Dialect::Sqlite,
    Dialect::Oracle,
    Dialect::BigQuery,
    Dialect::Snowflake,
  ] {
    let config = ConfigurationBuilder::new().dialect(dialect).build();
    for text in texts {
      let text = text.repeat(60);
      let start = std::time::Instant::now();
      let _ = format_text(&PathBuf::from("file.sql"), &text, &config);
      assert!(
        start.elapsed().as_secs() < 5,
        "took {:?} in {dialect:?} for:
{text}",
        start.elapsed()
      );
    }
  }
}

#[test]
fn should_report_parse_errors() {
  let cases = [
    (Dialect::Generic, "SELECT 'unterminated\n", "unterminated string"),
    (
      Dialect::Generic,
      "SELECT \"unterminated\n",
      "unterminated quoted identifier",
    ),
    (
      Dialect::SqlServer,
      "SELECT [unterminated\n",
      "unterminated quoted identifier",
    ),
    (
      Dialect::Generic,
      "SELECT 1 /* unterminated\n",
      "unterminated block comment",
    ),
    (
      Dialect::PostgreSql,
      "SELECT $$unterminated\n",
      "unterminated dollar-quoted string",
    ),
    (
      Dialect::Generic,
      "SELECT {{ unterminated\n",
      "unterminated template tag",
    ),
    (
      Dialect::Oracle,
      "SELECT q'[unterminated' FROM dual\n",
      "unterminated string",
    ),
    (Dialect::Generic, "SELECT (1\n", "unclosed '('"),
    (Dialect::Generic, "SELECT arr[1\n", "unclosed '['"),
    (Dialect::Generic, "SELECT (1]\n", "expected ')' to close the '('"),
    (Dialect::Generic, "SELECT 1)\n", "unexpected closing bracket"),
  ];
  for (dialect, input, expected) in cases {
    let config = ConfigurationBuilder::new().dialect(dialect).build();
    let error = format_text(&PathBuf::from("file.sql"), input, &config)
      .unwrap_err()
      .to_string();
    assert!(
      error.contains(expected),
      "for {input:?}\n  expected message containing {expected:?}\n  got {error}"
    );
  }
}

#[test]
fn should_suggest_dialect_for_backslash_escapes() {
  let config = ConfigurationBuilder::new().build();
  let error = format_text(&PathBuf::from("file.sql"), "SELECT 'it\\'s'\n", &config)
    .unwrap_err()
    .to_string();
  assert!(error.contains("set the \"dialect\" option"), "got {error}");
  // which formats in a dialect where a backslash escapes a quote
  let config = ConfigurationBuilder::new().dialect(Dialect::MySql).build();
  assert!(format_text(&PathBuf::from("file.sql"), "SELECT 'it\\'s'\n", &config).is_ok());
}

#[test]
fn should_point_at_error_position() {
  let config = ConfigurationBuilder::new().build();
  let error = format_text(&PathBuf::from("file.sql"), "SELECT 1;\nSELECT 'oops\n", &config)
    .unwrap_err()
    .to_string();
  assert!(error.contains("Line 2, column 8"), "got {error}");
}

/// A comment may legitimately end in whitespace that isn't a space or a tab, and `trim_end` would
/// take it.
#[test]
fn should_keep_unicode_whitespace_in_a_comment() {
  let config = ConfigurationBuilder::new().build();
  let text = format_text(&PathBuf::from("file.sql"), "SELECT 1; -- hi\u{a0}\n", &config).unwrap();
  assert_eq!(text, None, "the comment should have been left exactly as written");
}

#[test]
fn should_keep_multi_line_strings_as_they_are() {
  let config = ConfigurationBuilder::new().build();
  let text = "SELECT 'line one\n    line two\n  line three' AS s, \"col\nname\" FROM t;\n";
  let formatted = format_text(&PathBuf::from("file.sql"), text, &config).unwrap().unwrap();
  assert!(formatted.contains("'line one\n    line two\n  line three'"));
  assert!(formatted.contains("\"col\nname\""));
}

#[test]
fn should_only_change_casing_of_keywords() {
  let config = ConfigurationBuilder::new().casing(UppercaseKind::Upper).build();
  let text = "select name, \"select\", t.from, x.order, count(*) from t where name like 'select%';\n";
  let formatted = format_text(&PathBuf::from("file.sql"), text, &config).unwrap().unwrap();
  assert_eq!(
    formatted,
    "SELECT name, \"select\", t.from, x.order, count(*)\nFROM t\nWHERE name LIKE 'select%';\n"
  );
}

/// Formatting only ever changes the whitespace between tokens, which is checked after formatting
/// anything. This runs that check over every spec's input in every dialect, which catches the
/// cases where a dialect a spec isn't written for reads its text differently.
#[test]
fn should_only_change_whitespace_of_spec_inputs_in_every_dialect() {
  let dialects = [
    Dialect::Generic,
    Dialect::PostgreSql,
    Dialect::MySql,
    Dialect::SqlServer,
    Dialect::Sqlite,
    Dialect::Oracle,
    Dialect::BigQuery,
    Dialect::Snowflake,
  ];
  let mut checked = 0;
  for input in spec_inputs(Path::new("tests/specs")) {
    for dialect in dialects {
      for line_width in [40, 120] {
        let config = ConfigurationBuilder::new()
          .dialect(dialect)
          .line_width(line_width)
          .build();
        match format_text(Path::new("file.sql"), &input, &config) {
          Ok(Some(formatted)) => {
            let again = format_text(Path::new("file.sql"), &formatted, &config).unwrap();
            assert_eq!(
              again, None,
              "formatting again changed it in {dialect:?} at width {line_width}:\n{input}"
            );
            checked += 1;
          }
          Ok(None) => checked += 1,
          // the text might not be valid in this dialect (ex. a MySQL string escaped with a backslash)
          Err(FormatError::Parse(_)) => {}
          Err(err) => panic!("{err} in {dialect:?} at width {line_width} for:\n{input}"),
        }
      }
    }
  }
  assert!(checked > 1000, "only checked {checked}");
}

/// The input text of each spec in the spec files beneath `dir`.
fn spec_inputs(dir: &Path) -> Vec<String> {
  let mut inputs = Vec::new();
  for entry in std::fs::read_dir(dir).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      inputs.extend(spec_inputs(&path));
      continue;
    }
    let text = std::fs::read_to_string(&path).unwrap().replace("\r\n", "\n");
    let mut current: Option<Vec<&str>> = None;
    for line in text.lines() {
      if line.starts_with("== ") && line.ends_with(" ==") {
        current = Some(Vec::new());
      } else if line == "[expect]" {
        if let Some(lines) = current.take() {
          let input = lines.join("\n").replace("[|", "").replace("|]", "");
          inputs.push(input.trim_end().to_string() + "\n");
        }
      } else if let Some(lines) = current.as_mut() {
        lines.push(line);
      }
    }
  }
  inputs
}
