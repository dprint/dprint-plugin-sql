use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use dprint_core::configuration::*;
use dprint_development::*;
use dprint_plugin_sql::configuration::resolve_config;
use dprint_plugin_sql::*;

fn main() {
  let global_config = GlobalConfiguration::default();

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
      Arc::new(move |file_path, file_text, range, spec_config| {
        let spec_config: ConfigKeyMap = serde_json::from_value(spec_config.clone().into()).unwrap();
        let config_result = resolve_config(spec_config, &global_config);
        ensure_no_diagnostics(&config_result.diagnostics);

        Ok(match range {
          Some(range) => format_text_range(file_path, file_text, range, &config_result.config)?,
          None => format_text(file_path, file_text, &config_result.config)?,
        })
      })
    },
    Arc::new(move |file_path, file_text, spec_config| trace(file_path, file_text, spec_config, &global_config)),
  )
}

#[cfg(feature = "tracing")]
fn trace(
  file_path: &Path,
  file_text: &str,
  spec_config: &SpecConfigMap,
  global_config: &GlobalConfiguration,
) -> String {
  let spec_config: ConfigKeyMap = serde_json::from_value(spec_config.clone().into()).unwrap();
  let config_result = resolve_config(spec_config, global_config);
  ensure_no_diagnostics(&config_result.diagnostics);
  serde_json::to_string(&trace_file(file_path, file_text, &config_result.config)).unwrap()
}

#[cfg(not(feature = "tracing"))]
fn trace(
  _file_path: &Path,
  _file_text: &str,
  _spec_config: &SpecConfigMap,
  _global_config: &GlobalConfiguration,
) -> String {
  panic!(
    "
====
Please run with `cargo test --features tracing` to get trace output
====
"
  )
}
