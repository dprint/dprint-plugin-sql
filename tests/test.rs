extern crate dprint_development;
extern crate dprint_plugin_sql;

use std::path::PathBuf;

use dprint_core::configuration::*;
use dprint_development::*;
use dprint_plugin_sql::configuration::{resolve_config, ConfigurationBuilder};
use dprint_plugin_sql::*;

#[test]
fn test_specs() {
  let global_config = resolve_global_config(Default::default(), &Default::default()).config;

  run_specs(
    &PathBuf::from("./tests/specs"),
    &ParseSpecOptions {
      default_file_name: "file.sql",
    },
    &RunSpecsOptions {
      fix_failures: false,
      format_twice: true,
    },
    {
      let global_config = global_config.clone();
      move |file_path, file_text, spec_config| {
        let config_result = resolve_config(parse_config_key_map(spec_config), &global_config);
        ensure_no_diagnostics(&config_result.diagnostics);

        format_text(file_path, &file_text, &config_result.config)
      }
    },
    move |_file_path, _file_text, _spec_config| panic!("Plugin does not support dprint-core tracing."),
  )
}

#[test]
fn should_handle_windows_newlines() {
  let config = ConfigurationBuilder::new().build();
  let file_text = format_text(&PathBuf::from("file.sql"), "SELECT * FROM  dbo.Test\r\n", &config).unwrap();

  assert_eq!(file_text.unwrap(), "SELECT\n  *\nFROM\n  dbo.Test\n");
}
