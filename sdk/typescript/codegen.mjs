#!/usr/bin/env bun
// Generate the typed dr-strange client from the OpenRPC schema.
//
// Schema-first: `crates/dr-strange-web/openrpc.json` is the single source of
// truth. This emits `src/generated.ts` — the component types plus one method
// per RPC method, named camelCase (`node.create` -> `nodeCreate`), taking a
// single params object with the schema's wire field names and returning the
// method's typed result.
//
// Run `bun run codegen` after editing the schema; `generated.test.ts` fails if
// the committed output has drifted from the schema.

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
export const SCHEMA = resolve(HERE, "..", "..", "crates", "dr-strange-web", "openrpc.json");
export const OUT = resolve(HERE, "src", "generated.ts");

const HEADER = `// This file is GENERATED from crates/dr-strange-web/openrpc.json by codegen.mjs.
// Do not edit by hand — run \`bun run codegen\` to regenerate.
import { Client } from "./client";
`;

const SCALARS = { string: "string", integer: "number", number: "number", boolean: "boolean", null: "null" };

const scalar = (t) => SCALARS[t] ?? "unknown";

/** Map a JSON-Schema fragment to a TypeScript type expression. */
function tsType(schema) {
  if (!schema) return "unknown";
  if (schema.$ref) return schema.$ref.split("/").pop();
  if (schema.oneOf) return schema.oneOf.map(tsType).join(" | ");
  if (Array.isArray(schema.type)) return schema.type.map(scalar).join(" | ");
  if (schema.enum) return schema.enum.map((v) => JSON.stringify(v)).join(" | ");
  switch (schema.type) {
    case "string":
    case "integer":
    case "number":
    case "boolean":
    case "null":
      return scalar(schema.type);
    case "array":
      return `Array<${tsType(schema.items)}>`;
    case "object":
      return schema.properties ? objectType(schema, "; ") : "Record<string, unknown>";
    default:
      return "unknown";
  }
}

/** An inline object type from a schema's `properties`/`required`. */
function objectType(schema, sep) {
  const req = new Set(schema.required ?? []);
  const fields = Object.entries(schema.properties).map(
    ([k, v]) => `${k}${req.has(k) ? "" : "?"}: ${tsType(v)}`,
  );
  return `{ ${fields.join(sep)} }`;
}

const oneLine = (s) => s.replace(/\s+/g, " ").trim();

/** A named component: interface for property objects, type alias otherwise. */
function renderComponent(name, schema) {
  const desc = schema.description ? `/** ${oneLine(schema.description)} */\n` : "";
  if (schema.type === "object" && schema.properties) {
    const req = new Set(schema.required ?? []);
    const fields = Object.entries(schema.properties)
      .map(([k, v]) => `  ${k}${req.has(k) ? "" : "?"}: ${tsType(v)};`)
      .join("\n");
    return `${desc}export interface ${name} {\n${fields}\n}`;
  }
  return `${desc}export type ${name} = ${tsType(schema)};`;
}

/** `plane.set_props` -> `planeSetProps`. */
function methodName(rpc) {
  const [head, ...rest] = rpc.split(/[._]/);
  return head + rest.map((s) => s[0].toUpperCase() + s.slice(1)).join("");
}

function renderMethod(m) {
  const name = methodName(m.name);
  const params = m.params ?? [];
  const result = tsType(m.result?.schema);
  const access = m["x-access"];
  const doc = `  /** ${oneLine(m.summary ?? "")}${access ? ` (access: ${access})` : ""} */`;
  const wire = JSON.stringify(m.name);
  if (params.length === 0) {
    return `${doc}\n  ${name}(): Promise<${result}> {\n    return this._call(${wire}) as Promise<${result}>;\n  }`;
  }
  const optional = params.every((p) => !p.required) ? "?" : "";
  const pt = objectType({ properties: Object.fromEntries(params.map((p) => [p.name, p.schema])), required: params.filter((p) => p.required).map((p) => p.name) }, "; ");
  return `${doc}\n  ${name}(params${optional}: ${pt}): Promise<${result}> {\n    return this._call(${wire}, params) as Promise<${result}>;\n  }`;
}

/** Render the full generated module source for an OpenRPC document. */
export function render(doc) {
  const comps = doc.components?.schemas ?? {};
  const types = Object.entries(comps)
    .map(([n, s]) => renderComponent(n, s))
    .join("\n\n");
  const methods = (doc.methods ?? []).map(renderMethod).join("\n\n");
  return `${HEADER}\n${types}\n\n/** A dr-strange server client — one method per JSON-RPC method. */\nexport class Drsg extends Client {\n${methods}\n}\n`;
}

function main() {
  const doc = JSON.parse(readFileSync(SCHEMA, "utf8"));
  writeFileSync(OUT, render(doc));
  console.log(`wrote src/generated.ts (${doc.methods.length} methods)`);
}

if (import.meta.main) main();
