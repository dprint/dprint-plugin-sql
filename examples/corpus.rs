//! Formats SQL files from real projects to find the formatter's bugs: a panic, a refusal to
//! format (output that would change more than whitespace), output that changes when it's
//! formatted again, or a file that takes too long. `scripts/corpus.ts` fetches a pinned set of
//! projects and runs this over them.
//!
//! ```shellsession
//! cargo run --release --example corpus -- [options] [path...]
//! ```
//!
//! - `--manifest <file>` reads the files to format from lines of `<dialect>\t<path>`
//! - `--dialect <name>` is the dialect of the paths given directly (default `generic`)
//! - `--all-dialects` formats every file in every dialect rather than only its own
//! - `--line-width <n>` is a line width to format at, which may be given more than once (default
//!   120)
//! - `--out <dir>` writes what each file is formatted to beneath `<dir>`, so that two versions of
//!   the formatter can be compared
//! - `--verbose` also lists the files that don't parse, which is expected of a file in a dialect
//!   it isn't written in
//!
//! A panic is only caught when the release profile unwinds, which the script sets up.

use std::collections::BTreeMap;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

use dprint_core::configuration::ConfigKeyMap;
use dprint_core::configuration::GlobalConfiguration;
use dprint_plugin_sql::FormatError;
use dprint_plugin_sql::configuration::Configuration;
use dprint_plugin_sql::configuration::resolve_config;

const DIALECTS: &[&str] = &[
  "generic",
  "postgresql",
  "mysql",
  "sqlserver",
  "sqlite",
  "oracle",
  "bigquery",
  "snowflake",
];
/// How long a file may take before it's reported as slow.
const SLOW: Duration = Duration::from_secs(1);
/// How long a file may take before it's given up on.
const TIMEOUT: Duration = Duration::from_secs(30);

fn main() {
  let options = Options::parse();
  let mut jobs = Vec::new();
  for (dialect, path) in &options.files {
    let dialects: Vec<&str> = if options.all_dialects {
      DIALECTS.to_vec()
    } else {
      vec![dialect.as_str()]
    };
    for dialect in dialects {
      for &line_width in &options.line_widths {
        jobs.push(Job {
          path: path.clone(),
          dialect: dialect.to_string(),
          line_width,
        });
      }
    }
  }

  let outcomes = run_jobs(jobs, options.out.as_deref());

  let mut totals: BTreeMap<String, Totals> = BTreeMap::new();
  let mut problems = 0;
  for (job, outcome) in &outcomes {
    let totals = totals.entry(job.config_name()).or_default();
    totals.files += 1;
    let label = match outcome {
      Outcome::Ok => {
        totals.ok += 1;
        None
      }
      Outcome::NotUtf8 => {
        totals.skipped += 1;
        options.verbose.then(|| "SKIPPED as it isn't UTF-8".to_string())
      }
      Outcome::Slow(elapsed) => {
        totals.ok += 1;
        totals.slow += 1;
        Some(format!("SLOW {}ms", elapsed.as_millis()))
      }
      Outcome::ParseError(message) => {
        totals.parse_errors += 1;
        options.verbose.then(|| format!("PARSE {message}"))
      }
      Outcome::Refused => {
        totals.refused += 1;
        Some("REFUSED".to_string())
      }
      Outcome::Unstable(first_difference) => {
        totals.unstable += 1;
        Some(format!("UNSTABLE {first_difference}"))
      }
      Outcome::Panic(message) => {
        totals.panics += 1;
        Some(format!("PANIC {message}"))
      }
      Outcome::Timeout => {
        totals.timeouts += 1;
        Some(format!("TIMEOUT after {}s", TIMEOUT.as_secs()))
      }
    };
    if let Some(label) = label {
      if !matches!(outcome, Outcome::ParseError(_) | Outcome::Slow(_) | Outcome::NotUtf8) {
        problems += 1;
      }
      println!("[{}] {} {}", job.config_name(), job.path.display(), label);
    }
  }
  for (config, totals) in &totals {
    println!(
      "[{config}] files: {}, ok: {}, skipped: {}, parse errors: {}, refused: {}, unstable: {}, panics: {}, timeouts: {}, slow: {}",
      totals.files,
      totals.ok,
      totals.skipped,
      totals.parse_errors,
      totals.refused,
      totals.unstable,
      totals.panics,
      totals.timeouts,
      totals.slow
    );
  }
  if problems > 0 {
    println!("{problems} problems");
    std::process::exit(1);
  }
}

struct Options {
  files: Vec<(String, PathBuf)>,
  all_dialects: bool,
  line_widths: Vec<u32>,
  out: Option<PathBuf>,
  verbose: bool,
}

impl Options {
  fn parse() -> Self {
    let mut args = std::env::args().skip(1);
    let mut manifest = None;
    let mut dialect = "generic".to_string();
    let mut paths = Vec::new();
    let mut options = Options {
      files: Vec::new(),
      all_dialects: false,
      line_widths: Vec::new(),
      out: None,
      verbose: false,
    };
    while let Some(arg) = args.next() {
      let mut value = || args.next().unwrap_or_else(|| panic!("expected a value after {arg}"));
      match arg.as_str() {
        "--manifest" => manifest = Some(PathBuf::from(value())),
        "--dialect" => dialect = value(),
        "--all-dialects" => options.all_dialects = true,
        "--line-width" => options.line_widths.push(value().parse().expect("a line width")),
        "--out" => options.out = Some(PathBuf::from(value())),
        "--verbose" => options.verbose = true,
        _ if arg.starts_with("--") => panic!("unknown option {arg}"),
        _ => paths.push(PathBuf::from(arg)),
      }
    }
    if options.line_widths.is_empty() {
      options.line_widths.push(120);
    }
    if let Some(manifest) = manifest {
      let text = std::fs::read_to_string(&manifest).expect("the manifest to be readable");
      for line in text.lines().filter(|line| !line.is_empty()) {
        let (dialect, path) = line.split_once('\t').expect("a line of `<dialect>\\t<path>`");
        options.files.push((dialect.to_string(), PathBuf::from(path)));
      }
    }
    for path in paths {
      let mut files = Vec::new();
      collect_sql_files(&path, &mut files);
      options
        .files
        .extend(files.into_iter().map(|file| (dialect.clone(), file)));
    }
    options
  }
}

#[derive(Clone)]
struct Job {
  path: PathBuf,
  dialect: String,
  line_width: u32,
}

impl Job {
  fn config_name(&self) -> String {
    format!("{} {}", self.dialect, self.line_width)
  }

  fn config(&self) -> Configuration {
    let mut map = ConfigKeyMap::new();
    map.insert("dialect".to_string(), self.dialect.clone().into());
    map.insert("lineWidth".to_string(), (self.line_width as i32).into());
    let result = resolve_config(map, &GlobalConfiguration::default());
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    result.config
  }
}

enum Outcome {
  Ok,
  /// dprint only formats UTF-8 files (ex. not a UTF-16 one, which SQL Server tools often write).
  NotUtf8,
  Slow(Duration),
  ParseError(String),
  Refused,
  Unstable(String),
  Panic(String),
  Timeout,
}

#[derive(Default)]
struct Totals {
  files: usize,
  ok: usize,
  skipped: usize,
  parse_errors: usize,
  refused: usize,
  unstable: usize,
  panics: usize,
  timeouts: usize,
  slow: usize,
}

/// Runs the jobs on a thread per core, returning their outcomes in the order of the jobs.
fn run_jobs(jobs: Vec<Job>, out: Option<&Path>) -> Vec<(Job, Outcome)> {
  // a panic is an outcome to report rather than something to print
  std::panic::set_hook(Box::new(|_| {}));
  let queue = Arc::new(Mutex::new(jobs.into_iter().enumerate()));
  let results = Arc::new(Mutex::new(Vec::new()));
  let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
  let workers: Vec<_> = (0..threads)
    .map(|_| {
      let queue = queue.clone();
      let results = results.clone();
      let out = out.map(Path::to_path_buf);
      std::thread::spawn(move || {
        loop {
          let Some((index, job)) = queue.lock().unwrap().next() else {
            return;
          };
          let outcome = run_job(&job, out.as_deref());
          results.lock().unwrap().push((index, job, outcome));
        }
      })
    })
    .collect();
  for worker in workers {
    worker.join().unwrap();
  }
  let mut results = std::mem::take(&mut *results.lock().unwrap());
  results.sort_by_key(|(index, _, _)| *index);
  results.into_iter().map(|(_, job, outcome)| (job, outcome)).collect()
}

fn run_job(job: &Job, out: Option<&Path>) -> Outcome {
  let Ok(text) = std::fs::read_to_string(&job.path) else {
    return Outcome::NotUtf8;
  };
  // on a thread of its own so that a file that never finishes can be given up on
  let (sender, receiver) = mpsc::channel();
  let config = job.config();
  let path = job.path.clone();
  let start = Instant::now();
  std::thread::spawn(move || {
    let first = std::panic::catch_unwind(|| dprint_plugin_sql::format_text(&path, &text, &config));
    let second = match &first {
      Ok(Ok(Some(formatted))) => Some(std::panic::catch_unwind(|| {
        dprint_plugin_sql::format_text(&path, formatted, &config)
      })),
      _ => None,
    };
    let _ = sender.send((text, first, second));
  });
  let Ok((text, first, second)) = receiver.recv_timeout(TIMEOUT) else {
    return Outcome::Timeout;
  };
  let elapsed = start.elapsed();
  let formatted = match first {
    Err(panic) => return Outcome::Panic(panic_message(panic)),
    Ok(Err(FormatError::Parse(err))) => {
      return Outcome::ParseError(err.to_string().lines().next().unwrap_or("").to_string());
    }
    Ok(Err(err)) => {
      write_output(out, job, &format!("{err}\n"), true);
      return Outcome::Refused;
    }
    Ok(Ok(formatted)) => formatted.unwrap_or(text),
  };
  write_output(out, job, &formatted, false);
  match second {
    None | Some(Ok(Ok(None))) => {}
    Some(Ok(Ok(Some(again)))) => {
      let difference = formatted
        .lines()
        .zip(again.lines())
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map(|(i, (a, b))| format!("at line {}: {a:?} became {b:?}", i + 1))
        .unwrap_or_default();
      return Outcome::Unstable(difference);
    }
    Some(Ok(Err(_))) => return Outcome::Unstable("formatting the output failed".to_string()),
    Some(Err(panic)) => return Outcome::Panic(format!("formatting the output: {}", panic_message(panic))),
  }
  if elapsed > SLOW {
    Outcome::Slow(elapsed)
  } else {
    Outcome::Ok
  }
}

/// Writes what a file was formatted to, or why it couldn't be, beneath `out`.
fn write_output(out: Option<&Path>, job: &Job, text: &str, is_error: bool) {
  let Some(out) = out else {
    return;
  };
  let mut path = out.join(job.config_name().replace(' ', "-"));
  // the components of the file's path that are names, whether it's relative or absolute
  for component in job.path.components() {
    if let Component::Normal(name) = component {
      path.push(name);
    }
  }
  if is_error {
    path.as_mut_os_string().push(".error");
  }
  std::fs::create_dir_all(path.parent().unwrap()).unwrap();
  std::fs::write(path, text).unwrap();
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
  panic
    .downcast_ref::<&str>()
    .map(|s| s.to_string())
    .or_else(|| panic.downcast_ref::<String>().cloned())
    .unwrap_or_default()
}

fn collect_sql_files(path: &Path, files: &mut Vec<PathBuf>) {
  if path.is_dir() {
    let mut entries: Vec<_> = std::fs::read_dir(path)
      .unwrap()
      .filter_map(|entry| entry.ok())
      .map(|entry| entry.path())
      .collect();
    entries.sort();
    for entry in entries {
      collect_sql_files(&entry, files);
    }
  } else if path.extension().is_some_and(|extension| extension == "sql") {
    files.push(path.to_path_buf());
  }
}
