use dprint_core::configuration::NewLineKind;
use dprint_core::configuration::ParseConfigurationError;
use dprint_core::generate_str_to_from;
use serde::Deserialize;
use serde::Serialize;

/// The casing of keywords.
#[derive(Clone, PartialEq, Eq, Debug, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UppercaseKind {
  /// Preserve original keyword casing.
  Preserve,
  /// Convert keywords to UPPERCASE.
  Upper,
  /// Convert keywords to lowercase.
  Lower,
}

generate_str_to_from![
  UppercaseKind,
  [Preserve, "preserve"],
  [Upper, "upper"],
  [Lower, "lower"]
];

/// The SQL dialect, which decides how the text is split into tokens: which characters start a
/// comment or quote an identifier, whether a backslash escapes a quote, and so on.
#[derive(Clone, PartialEq, Eq, Debug, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Dialect {
  /// Accepts the syntax of all of the dialects where they don't conflict.
  Generic,
  /// PostgreSQL, which also suits other databases that use its syntax (ex. CockroachDB, DuckDB,
  /// Redshift).
  PostgreSql,
  /// MySQL and MariaDB.
  MySql,
  /// Microsoft SQL Server and Azure SQL (T-SQL).
  SqlServer,
  /// SQLite.
  Sqlite,
  /// Oracle (PL/SQL).
  Oracle,
  /// Google BigQuery.
  BigQuery,
  /// Snowflake.
  Snowflake,
}

generate_str_to_from![
  Dialect,
  [Generic, "generic"],
  [PostgreSql, "postgresql"],
  [MySql, "mysql"],
  [SqlServer, "sqlserver"],
  [Sqlite, "sqlite"],
  [Oracle, "oracle"],
  [BigQuery, "bigquery"],
  [Snowflake, "snowflake"]
];

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Configuration {
  pub line_width: u32,
  pub use_tabs: bool,
  pub indent_width: u8,
  pub new_line_kind: NewLineKind,
  pub uppercase: UppercaseKind,
  pub lines_between_queries: u8,
  pub joins_as_top_level: bool,
  pub dialect: Dialect,
}
