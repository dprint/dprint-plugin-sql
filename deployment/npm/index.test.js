// @ts-check
const assert = require("assert");
const createFromBuffer = require("@dprint/formatter").createFromBuffer;
const getBuffer = require("./index").getBuffer;

const formatter = createFromBuffer(getBuffer());
const result = formatter.formatText({
  filePath: "file.sql",
  fileText: "SELECT  *  FROM dbo.Test",
});

assert.strictEqual(result, "SELECT *\nFROM dbo.Test\n");
