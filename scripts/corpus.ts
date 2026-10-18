// Formats the SQL files of a pinned set of open source projects to find the formatter's bugs (see
// `examples/corpus.rs`), and optionally compares what they're formatted to with another version
// of the formatter.
//
// deno run -A scripts/corpus.ts [options] [project...]
//
//   --all-dialects        format every file in every dialect rather than only its own
//   --line-width <n,...>  the line widths to format at (default 120)
//   --compare <ref>       also format with the formatter at a git ref (ex. `main`), and write the
//                         differences to .corpus/compare.diff
//   --verbose             also list the files that don't parse
//
// The projects are fetched into .corpus/projects, only their .sql files, at the commits below.

interface Project {
  name: string;
  url: string;
  commit: string;
  /** The directories to take .sql files from, or the whole project when there are none. */
  paths?: string[];
  /** The dialect of a file, from its path within the project. */
  dialect: (path: string) => string;
}

const projects: Project[] = [
  {
    name: "sqlfluff",
    url: "https://github.com/sqlfluff/sqlfluff",
    commit: "cf59ad38d4d1435dbad9118bd791ef03795ace0a",
    paths: ["test/fixtures/dialects"],
    dialect: (path) => {
      const folder = path.split("/")[3];
      return ({
        ansi: "generic",
        bigquery: "bigquery",
        greenplum: "postgresql",
        mariadb: "mysql",
        mysql: "mysql",
        oracle: "oracle",
        postgres: "postgresql",
        redshift: "postgresql",
        snowflake: "snowflake",
        sqlite: "sqlite",
        tsql: "sqlserver",
      } as Record<string, string>)[folder] ?? "generic";
    },
  },
  {
    name: "postgres",
    url: "https://github.com/postgres/postgres",
    commit: "45fc51fcbf4534ea2f6f85e36f8155dbd5ba9851",
    paths: ["src/test/regress/sql", "src/pl/plpgsql/src/sql"],
    dialect: () => "postgresql",
  },
  {
    name: "pgtap",
    url: "https://github.com/theory/pgtap",
    commit: "21ceed193bf370892d530c5115680839ee322851",
    dialect: () => "postgresql",
  },
  {
    name: "sql-server-samples",
    url: "https://github.com/microsoft/sql-server-samples",
    commit: "e5d7bb90ca0b695ac14f61458047bfb5a498459e",
    dialect: () => "sqlserver",
  },
  {
    name: "first-responder-kit",
    url: "https://github.com/BrentOzarULTD/SQL-Server-First-Responder-Kit",
    commit: "53c380edf86e93fa07bcb2f2caf82c3e0d20fb68",
    dialect: () => "sqlserver",
  },
  {
    name: "sql-server-maintenance-solution",
    url: "https://github.com/olahallengren/sql-server-maintenance-solution",
    commit: "41617578643e722b790ea4d57ef80f8de38ae747",
    dialect: () => "sqlserver",
  },
  {
    name: "db-sample-schemas",
    url: "https://github.com/oracle-samples/db-sample-schemas",
    commit: "6660bad68c07bd143430ace58565b3f727e17263",
    dialect: () => "oracle",
  },
  {
    name: "test_db",
    url: "https://github.com/datacharmer/test_db",
    commit: "e324b56193ca506ab7cc1ab143a9153d8c4535d7",
    dialect: () => "mysql",
  },
  {
    name: "sakila",
    url: "https://github.com/jOOQ/sakila",
    commit: "e089a5b1ec9af0df7a9c6a5d47d49fa1736a4e84",
    dialect: dialectFromName,
  },
  {
    name: "chinook",
    url: "https://github.com/lerocha/chinook-database",
    commit: "7f67772503d71ba90f19283c38e93923addb43fa",
    dialect: dialectFromName,
  },
  {
    name: "bigquery-utils",
    url: "https://github.com/GoogleCloudPlatform/bigquery-utils",
    commit: "d5bde4fb0d2dbfa0477f1f2a6d87edda65bb7907",
    dialect: () => "bigquery",
  },
  {
    name: "sfquickstarts",
    url: "https://github.com/Snowflake-Labs/sfquickstarts",
    commit: "b186c148d09b971fcf99e693ffa70312b196e0ec",
    dialect: () => "snowflake",
  },
  {
    name: "jaffle_shop",
    url: "https://github.com/dbt-labs/jaffle_shop",
    commit: "fd7bfacae4f497ff044a6a0275268676bf1b64c3",
    dialect: () => "generic",
  },
  {
    name: "dbt-utils",
    url: "https://github.com/dbt-labs/dbt-utils",
    commit: "2a8f07bcbd02fb7a8c011298a1bdaf3e92759c14",
    dialect: () => "generic",
  },
];

const rootDir = new URL("../", import.meta.url).pathname.replace(/^\/(\w:)/, "$1");
const corpusDir = `${rootDir}.corpus`;
const projectsDir = `${corpusDir}/projects`;

const options = parseArgs(Deno.args);
const selected = options.projects.length === 0
  ? projects
  : options.projects.map((name) => {
    const project = projects.find((p) => p.name === name);
    if (project == null) {
      throw new Error(`Unknown project ${name}. Expected one of: ${projects.map((p) => p.name).join(", ")}`);
    }
    return project;
  });

for (const project of selected) {
  await fetchProject(project);
}
const manifest = `${corpusDir}/manifest.tsv`;
await Deno.writeTextFile(manifest, (await manifestLines(selected)).join("\n") + "\n");

const runnerArgs = ["--manifest", manifest];
if (options.allDialects) {
  runnerArgs.push("--all-dialects");
}
for (const width of options.lineWidths) {
  runnerArgs.push("--line-width", width);
}
if (options.verbose) {
  runnerArgs.push("--verbose");
}

if (options.compare == null) {
  Deno.exit(await runCorpus(rootDir, `${rootDir}target/corpus`, runnerArgs));
}

// compare with the formatter at another ref, running this version of the runner against it
const baseDir = `${corpusDir}/base`;
await removeWorktree(baseDir);
await run("git", ["worktree", "add", "--quiet", "--detach", baseDir, options.compare], rootDir);
let code;
try {
  await Deno.mkdir(`${baseDir}/examples`, { recursive: true });
  await Deno.copyFile(`${rootDir}examples/corpus.rs`, `${baseDir}/examples/corpus.rs`);
  const outDir = `${corpusDir}/out`;
  await Deno.remove(outDir, { recursive: true }).catch(() => {});
  console.log(`\n=== ${options.compare}`);
  await runCorpus(baseDir, `${rootDir}target/corpus-base`, [...runnerArgs, "--out", `${outDir}/base`]);
  console.log("\n=== this version");
  code = await runCorpus(rootDir, `${rootDir}target/corpus`, [...runnerArgs, "--out", `${outDir}/head`]);
  const diff = await output("git", ["diff", "--no-index", "--", "base", "head"], outDir);
  await Deno.writeTextFile(`${corpusDir}/compare.diff`, diff);
  const stat = await output("git", ["diff", "--no-index", "--shortstat", "--", "base", "head"], outDir);
  console.log(`\n${stat.trim() || "no differences"} (see .corpus/compare.diff)`);
} finally {
  await removeWorktree(baseDir);
}
Deno.exit(code);

/** Fetches the project's .sql files at its commit, unless they're there already. */
async function fetchProject(project: Project) {
  const dir = `${projectsDir}/${project.name}`;
  const head = await output("git", ["rev-parse", "HEAD"], dir).catch(() => "");
  if (head.trim() === project.commit) {
    return;
  }
  console.log(`Fetching ${project.name}...`);
  await Deno.mkdir(dir, { recursive: true });
  if (head === "") {
    await run("git", ["init", "--quiet"], dir);
    await run("git", ["remote", "add", "origin", project.url], dir);
  }
  // only the blobs of the .sql files are fetched, which keeps large projects small
  await run("git", ["sparse-checkout", "set", "--no-cone", "*.sql"], dir);
  await run("git", ["fetch", "--quiet", "--depth", "1", "--filter=blob:none", "origin", project.commit], dir);
  await run("git", ["checkout", "--quiet", "--detach", "FETCH_HEAD"], dir);
}

async function manifestLines(projects: Project[]) {
  const lines = [];
  for (const project of projects) {
    const dir = `${projectsDir}/${project.name}`;
    for (const path of project.paths ?? [""]) {
      for await (const file of sqlFiles(`${dir}/${path}`.replace(/\/$/, ""))) {
        const relative = file.slice(dir.length + 1);
        lines.push(`${project.dialect(relative)}\tprojects/${project.name}/${relative}`);
      }
    }
  }
  return lines.sort();
}

async function* sqlFiles(dir: string): AsyncGenerator<string> {
  for await (const entry of Deno.readDir(dir)) {
    const path = `${dir}/${entry.name}`;
    if (entry.isDirectory && entry.name !== ".git") {
      yield* sqlFiles(path);
    } else if (entry.isFile && entry.name.endsWith(".sql")) {
      yield path;
    }
  }
}

/** Builds and runs the runner with the formatter in `crateDir`, returning its exit code. */
async function runCorpus(crateDir: string, targetDir: string, args: string[]) {
  const env = {
    // a panic is only caught when it unwinds
    CARGO_PROFILE_RELEASE_PANIC: "unwind",
    CARGO_TARGET_DIR: targetDir,
  };
  await run("cargo", ["build", "--quiet", "--release", "--example", "corpus"], crateDir, env);
  const exe = `${targetDir}/release/examples/corpus${Deno.build.os === "windows" ? ".exe" : ""}`;
  // the paths in the manifest are relative to the corpus directory
  const { code } = await new Deno.Command(exe, { args, cwd: corpusDir }).spawn().status;
  return code;
}

/** The dialect of a file named after its database (ex. `Chinook_PostgreSql.sql`). */
function dialectFromName(path: string) {
  const lower = path.toLowerCase();
  const names: [string, string][] = [
    ["postgres", "postgresql"],
    ["mysql", "mysql"],
    ["sql-server", "sqlserver"],
    ["sqlserver", "sqlserver"],
    ["sqlite", "sqlite"],
    ["oracle", "oracle"],
  ];
  return names.find(([name]) => lower.includes(name))?.[1] ?? "generic";
}

async function removeWorktree(dir: string) {
  await output("git", ["worktree", "remove", "--force", dir], rootDir).catch(() => {});
}

async function run(command: string, args: string[], cwd: string, env?: Record<string, string>) {
  const { code } = await new Deno.Command(command, { args, cwd, env }).spawn().status;
  if (code !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed with exit code ${code}`);
  }
}

async function output(command: string, args: string[], cwd: string) {
  const result = await new Deno.Command(command, { args, cwd, stderr: "null" }).output();
  // `git diff --no-index` exits with 1 when there are differences
  if (result.code > 1) {
    throw new Error(`${command} ${args.join(" ")} failed with exit code ${result.code}`);
  }
  return new TextDecoder().decode(result.stdout);
}

function parseArgs(args: string[]) {
  const options = {
    allDialects: false,
    lineWidths: [] as string[],
    compare: undefined as string | undefined,
    verbose: false,
    projects: [] as string[],
  };
  for (let i = 0; i < args.length; i++) {
    const arg = args[i];
    if (arg === "--all-dialects") {
      options.allDialects = true;
    } else if (arg === "--line-width") {
      options.lineWidths.push(...args[++i].split(","));
    } else if (arg === "--compare") {
      options.compare = args[++i];
    } else if (arg === "--verbose") {
      options.verbose = true;
    } else if (arg.startsWith("--")) {
      throw new Error(`Unknown option ${arg}`);
    } else {
      options.projects.push(arg);
    }
  }
  return options;
}
