# dprint-plugin-sql

[![npm version](https://img.shields.io/npm/v/@dprint/sql.svg)](https://www.npmjs.com/package/@dprint/sql) [![CI](https://github.com/dprint/dprint-plugin-sql/workflows/CI/badge.svg)](https://github.com/dprint/dprint-plugin-sql/actions?query=workflow%3ACI)

SQL formatting plugin for [dprint](https://github.com/dprint/dprint).

It formats the SQL of PostgreSQL, MySQL and MariaDB, SQL Server (T-SQL), SQLite, Oracle (PL/SQL), BigQuery and Snowflake, including stored procedures, functions, triggers and the other procedural code in them.

## Install

[Install](https://dprint.dev/install/) and [setup](https://dprint.dev/setup/) dprint.

Then in your project's directory with a dprint.json file, run:

```shellsession
dprint add sql
# or install from npm
dprint add npm:@dprint/sql
```

## Configuration

To add configuration, specify a `"sql"` key in your dprint.json:

```jsonc
{
  "sql": {
    "dialect": "postgresql",
    "casing": "upper",
    "lineWidth": 100,
  },
  "plugins": [
    // ...etc...
  ],
}
```

| Option                | Default    | Description                                                                                                                         |
| --------------------- | ---------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `dialect`             | `generic`  | The dialect the files are written in: `generic`, `postgresql`, `mysql`, `sqlserver`, `sqlite`, `oracle`, `bigquery` or `snowflake`. |
| `casing`              | `preserve` | The casing of keywords: `preserve`, `upper` or `lower`. Names and functions are left as written.                                    |
| `lineWidth`           | `120`      | The width of a line the printer tries to stay under.                                                                                |
| `indentWidth`         | `2`        | The number of spaces of an indent.                                                                                                  |
| `useTabs`             | `false`    | Whether to indent with tabs.                                                                                                        |
| `newLineKind`         | `lf`       | The kind of newline: `auto`, `crlf`, `lf` or `system`.                                                                              |
| `linesBetweenQueries` | `1`        | The minimum number of line breaks between statements at the top level of the file.                                                  |
| `joinsAsTopLevel`     | `false`    | Whether to write joins at the indentation of `FROM` rather than indented beneath it.                                                |

### Dialects

The dialect decides how the text is split into tokens, which is where the dialects disagree: whether `#` starts a comment (MySQL, BigQuery), names a temporary table (SQL Server) or is an operator (PostgreSQL), whether a backslash escapes a quote in a string (MySQL, BigQuery, Snowflake), whether `[name]` is a quoted identifier (SQL Server, SQLite), and so on.

The `generic` dialect accepts the syntax of all of them where they don't conflict, and works out the rest from what's around it. Set the dialect when a file doesn't format as expected, or formatting reports a string that isn't terminated.

Client commands are kept as they are: `GO` and `sqlcmd` commands for SQL Server, `DELIMITER` for MySQL, `/` and SQL\*Plus commands for Oracle, and psql's meta-commands and the data of a `COPY .. FROM STDIN` for PostgreSQL.

### Procedural code

Stored procedures, functions, triggers and anonymous blocks are formatted along with the statements in them, for the procedural syntax of each dialect: PL/pgSQL, MySQL's compound statements, T-SQL (including statements without semicolons), PL/SQL packages and procedures, and the scripting of BigQuery and Snowflake.

The body of a PostgreSQL or Snowflake routine in a dollar-quoted string (`$$ .. $$`) is formatted when its language is SQL or PL/pgSQL, as is the body of a `DO` block. Bodies in other languages are left as they are.

### Ignoring statements

A statement after a `-- dprint-ignore` comment is left as it is, and a file starting with a `-- dprint-ignore-file` comment isn't formatted.

```sql
-- dprint-ignore
SELECT   a,   b
FROM     t;
```

## JS Formatting API

- [JS Formatter](https://github.com/dprint/js-formatter) - Browser/Deno and Node
- [npm package](https://www.npmjs.com/package/@dprint/sql)
