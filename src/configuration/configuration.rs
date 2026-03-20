use dprint_core::configuration::NewLineKind;
use dprint_core::configuration::ParseConfigurationError;
use dprint_core::generate_str_to_from;
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UppercaseKind {
  /// Preserve original keyword casing.
  Preserve,
  /// Convert reserved words to UPPERCASE.
  Upper,
  /// Convert reserved words to lowercase.
  Lower,
}

generate_str_to_from![
  UppercaseKind,
  [Preserve, "preserve"],
  [Upper, "upper"],
  [Lower, "lower"]
];

#[derive(Clone, PartialEq, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Dialect {
  /// Generic SQL syntax.
  Generic,
  /// PostgreSQL syntax.
  PostgreSql,
  /// SQL Server syntax.
  SqlServer,
}

generate_str_to_from![
  Dialect,
  [Generic, "generic"],
  [PostgreSql, "postgresql"],
  [SqlServer, "sqlserver"]
];

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Configuration {
  pub use_tabs: bool,
  pub indent_width: u8,
  pub new_line_kind: NewLineKind,
  pub uppercase: UppercaseKind,
  pub lines_between_queries: u8,
  pub joins_as_top_level: bool,
  pub dialect: Dialect,
}
