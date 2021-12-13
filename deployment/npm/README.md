# @dprint/sql

npm distribution of [dprint-plugin-sql](https://github.com/dprint/dprint-plugin-sql) which wraps [sqlformat-rs](https://github.com/shssoichiro/sqlformat-rs).

Use this with [@dprint/formatter](https://github.com/dprint/js-formatter) or just use @dprint/formatter and download the [dprint-plugin-sql WASM file](https://github.com/dprint/dprint-plugin-sql/releases).

## Example

```ts
import { createFromBuffer } from "@dprint/formatter";
import { getBuffer } from "@dprint/sql";

const formatter = createFromBuffer(getBuffer());

console.log(
  formatter.formatText("test.sql", "SELECT    *  FROM  dbo.Something"),
);
```
