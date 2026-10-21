# @dprint/sql

npm distribution of [dprint-plugin-sql](https://github.com/dprint/dprint-plugin-sql), a SQL formatter.

Use this with [@dprint/formatter](https://github.com/dprint/js-formatter) or just use @dprint/formatter and download the [dprint-plugin-sql WASM file](https://github.com/dprint/dprint-plugin-sql/releases).

## Example

```ts
import { createFromBuffer } from "@dprint/formatter";
import { getBuffer } from "@dprint/sql";

const formatter = createFromBuffer(getBuffer());

console.log(formatter.formatText({
  filePath: "test.sql",
  fileText: "SELECT    *  FROM  dbo.Something",
}));
```
