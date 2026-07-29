// The committed generated client must match the schema (no manual drift).
import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { OUT, SCHEMA, render } from "../codegen.mjs";

test("src/generated.ts is current", () => {
  const doc = JSON.parse(readFileSync(SCHEMA, "utf8"));
  const expected = render(doc);
  const actual = readFileSync(OUT, "utf8");
  expect(actual).toBe(expected);
});
