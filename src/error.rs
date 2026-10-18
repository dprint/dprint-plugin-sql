/// The input could not be parsed as SQL.
///
/// The message is a pre-rendered diagnostic that includes the offending line
/// and a caret pointing at the position of the error.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ParseError {
  message: String,
}

impl ParseError {
  pub(crate) fn new(message: String) -> Self {
    ParseError { message }
  }
}

/// An error that can occur while formatting a SQL file.
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
  /// The input could not be parsed as SQL.
  #[error(transparent)]
  Parse(#[from] ParseError),
  /// Formatting would have changed more than the whitespace between the tokens of the input,
  /// which is a bug in the formatter. The input is left as it is rather than risking that.
  #[error(
    "Formatting would have changed more than the whitespace of the file, so it was left as it is. This is a bug in dprint-plugin-sql. Please report it at https://github.com/dprint/dprint-plugin-sql/issues"
  )]
  WouldChangeCode,
}
