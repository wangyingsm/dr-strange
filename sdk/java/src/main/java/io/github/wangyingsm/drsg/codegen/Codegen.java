// Generate the typed dr-strange client from the OpenRPC schema.
//
// Schema-first: crates/dr-strange-web/openrpc.json is the single source of
// truth. render() emits Drsg.java — the component types plus one method per RPC
// method, named camelCase (node.create -> nodeCreate), taking a params record
// (with of()/withX() builders for optional fields) and returning the typed
// result. Component and parameter types are nested records inside the Drsg
// class, so the whole client is one generated file.
//
// Run via `mvn -q compile exec:java`; GeneratedDriftTest fails if the committed
// Drsg.java has drifted from the schema. This class is excluded from the jar.
package io.github.wangyingsm.drsg.codegen;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

public final class Codegen {

    private static final ObjectMapper MAPPER = new ObjectMapper();

    private Codegen() {}

    public static void main(String[] args) throws Exception {
        Path schemaPath = Path.of(args[0]);
        Path outPath = Path.of(args[1]);
        String src = render(Files.readString(schemaPath));
        Files.writeString(outPath, src);
        System.out.println("wrote " + outPath);
    }

    /** Render the full Drsg.java source for an OpenRPC document. */
    public static String render(String schemaJson) throws Exception {
        JsonNode doc = MAPPER.readTree(schemaJson);
        return new Codegen().build(doc);
    }

    // Emitted type definitions, keyed by name in insertion order (components
    // first, then params/inline-result records discovered while walking methods).
    private final Map<String, String> typeDefs = new LinkedHashMap<>();
    private final Set<String> seen = new LinkedHashSet<>();

    private String build(JsonNode doc) {
        JsonNode components = doc.path("components").path("schemas");
        components.fieldNames().forEachRemaining(name -> {
            JsonNode s = components.get(name);
            // Properties and NodeRef map to Map<String,Object> / Object inline; the
            // rest are property objects rendered as records.
            if ("object".equals(s.path("type").asText()) && hasProps(s)) {
                seen.add(name);
                typeDefs.put(name, renderRecord(name, s));
            }
        });

        StringBuilder methods = new StringBuilder();
        for (JsonNode m : doc.path("methods")) {
            methods.append(renderMethod(m)).append("\n");
        }

        StringBuilder b = new StringBuilder();
        b.append("// Code generated from crates/dr-strange-web/openrpc.json by the codegen package; DO NOT EDIT.\n");
        b.append("package io.github.wangyingsm.drsg;\n\n");
        b.append("import com.fasterxml.jackson.core.type.TypeReference;\n");
        b.append("import java.util.List;\n");
        b.append("import java.util.Map;\n\n");
        b.append("/** A dr-strange server client — one method per JSON-RPC method. */\n");
        b.append("public class Drsg extends Client {\n\n");
        b.append("    public Drsg() {\n        super();\n    }\n\n");
        b.append("    public Drsg(String baseUrl) {\n        super(baseUrl);\n    }\n\n");
        b.append("    public Drsg(String baseUrl, String token) {\n        super(baseUrl, token);\n    }\n\n");

        for (String def : typeDefs.values()) {
            b.append(def).append("\n");
        }
        b.append(methods);
        b.append("}\n");
        return b.toString();
    }

    // --- methods ---

    private String renderMethod(JsonNode m) {
        String wire = m.get("name").asText();
        String name = methodName(wire);
        String pascal = pascalName(wire);
        JsonNode params = m.path("params");
        String result = resultType(m.path("result").path("schema"), pascal);

        String doc = oneLine(m.path("summary").asText(""));
        String access = m.path("x-access").asText("");
        if (!access.isEmpty()) {
            doc += " (access: " + access + ")";
        }
        String tref = "new TypeReference<" + result + ">() {}";

        StringBuilder b = new StringBuilder();
        b.append("    /** ").append(doc).append(" */\n");
        if (params.isMissingNode() || params.size() == 0) {
            b.append("    public ").append(result).append(' ').append(name).append("() throws DrsgException {\n");
            b.append("        return call(\"").append(wire).append("\", null, ").append(tref).append(");\n");
            b.append("    }\n");
            return b.toString();
        }
        String paramsType = pascal + "Params";
        addParams(paramsType, params);
        b.append("    public ").append(result).append(' ').append(name)
                .append('(').append(paramsType).append(" params) throws DrsgException {\n");
        b.append("        return call(\"").append(wire).append("\", params, ").append(tref).append(");\n");
        b.append("    }\n");
        return b.toString();
    }

    /**
     * Result Java type: object components and inline objects become records,
     * arrays lists, untyped objects Map&lt;String, Object&gt;.
     */
    private String resultType(JsonNode s, String pascal) {
        if (s.isMissingNode()) {
            return "Object";
        }
        if (s.has("$ref")) {
            return refType(refBase(s.get("$ref").asText()));
        }
        if (s.has("oneOf")) {
            for (JsonNode a : s.get("oneOf")) {
                if (a.has("$ref")) {
                    return refType(refBase(a.get("$ref").asText()));
                }
            }
            return "Object";
        }
        String type = s.path("type").asText("");
        if ("array".equals(type)) {
            JsonNode items = s.path("items");
            if (items.has("$ref")) {
                return "List<" + refType(refBase(items.get("$ref").asText())) + ">";
            }
            if ("object".equals(items.path("type").asText()) && hasProps(items)) {
                String n = pascal + "Item";
                addRecord(n, items);
                return "List<" + n + ">";
            }
            return "List<" + typeOf(items, true, pascal + "Item") + ">";
        }
        if ("object".equals(type)) {
            if (hasProps(s)) {
                String n = pascal + "Result";
                addRecord(n, s);
                return n;
            }
            return "Map<String, Object>";
        }
        return typeOf(s, false, pascal + "Result");
    }

    // --- records ---

    private void addRecord(String name, JsonNode s) {
        if (seen.add(name)) {
            typeDefs.put(name, renderRecord(name, s));
        }
    }

    /** A plain record (component or inline result object) from a properties object. */
    private String renderRecord(String name, JsonNode s) {
        JsonNode props = s.path("properties");
        Set<String> required = toSet(s.path("required"));
        List<String> fields = new ArrayList<>();
        props.fieldNames().forEachRemaining(key -> {
            String type = typeOf(props.get(key), !required.contains(key), name + capitalize(camel(key)));
            fields.add("            " + type + " " + camel(key));
        });
        return "    public record " + name + "(\n"
                + String.join(",\n", fields) + ") {\n    }\n";
    }

    private void addParams(String name, JsonNode params) {
        if (!seen.add(name)) {
            return;
        }
        List<String> fieldDecls = new ArrayList<>();
        List<String> fieldNames = new ArrayList<>();
        List<String> requiredDecls = new ArrayList<>();
        List<String> optionalNames = new ArrayList<>();
        List<String> optionalTypes = new ArrayList<>();

        for (JsonNode p : params) {
            String wire = p.get("name").asText();
            boolean req = p.path("required").asBoolean(false);
            String jname = camel(wire);
            String type = typeOf(p.get("schema"), !req, name + capitalize(jname));
            fieldDecls.add("            " + type + " " + jname);
            fieldNames.add(jname);
            if (req) {
                requiredDecls.add(type + " " + jname);
            } else {
                optionalNames.add(jname);
                optionalTypes.add(type);
            }
        }

        StringBuilder b = new StringBuilder();
        b.append("    public record ").append(name).append("(\n");
        b.append(String.join(",\n", fieldDecls)).append(") {\n");

        // of(required…): a factory that leaves optionals null.
        List<String> ofArgs = new ArrayList<>();
        for (String fn : fieldNames) {
            ofArgs.add(requiredContains(requiredDecls, fn) ? fn : "null");
        }
        b.append("\n        public static ").append(name).append(" of(")
                .append(String.join(", ", requiredDecls)).append(") {\n");
        b.append("            return new ").append(name).append('(')
                .append(String.join(", ", ofArgs)).append(");\n");
        b.append("        }\n");

        // withX(v): copy with one optional replaced (its param shadows the field).
        String ctorArgs = String.join(", ", fieldNames);
        for (int i = 0; i < optionalNames.size(); i++) {
            String fn = optionalNames.get(i);
            String ft = optionalTypes.get(i);
            b.append("\n        public ").append(name).append(" with").append(capitalize(fn))
                    .append('(').append(ft).append(' ').append(fn).append(") {\n");
            b.append("            return new ").append(name).append('(').append(ctorArgs).append(");\n");
            b.append("        }\n");
        }
        b.append("    }\n");
        typeDefs.put(name, b.toString());
    }

    // requiredDecls holds "Type name" entries; match by the trailing name.
    private static boolean requiredContains(List<String> requiredDecls, String fieldName) {
        for (String d : requiredDecls) {
            if (d.endsWith(" " + fieldName)) {
                return true;
            }
        }
        return false;
    }

    // --- type mapping ---

    /** Map a JSON-Schema fragment to a Java type; optional forces boxed scalars. */
    private String typeOf(JsonNode s, boolean optional, String hint) {
        if (s == null || s.isMissingNode()) {
            return "Object";
        }
        if (s.has("$ref")) {
            return refType(refBase(s.get("$ref").asText()));
        }
        if (s.has("oneOf")) {
            JsonNode nonNull = null;
            boolean hasNull = false;
            for (JsonNode a : s.get("oneOf")) {
                if ("null".equals(a.path("type").asText())) {
                    hasNull = true;
                } else {
                    nonNull = a;
                }
            }
            if (nonNull != null && hasNull) {
                return typeOf(nonNull, true, hint);
            }
            return "Object";
        }
        JsonNode type = s.get("type");
        if (type != null && type.isArray()) { // e.g. ["string", "null"]
            for (JsonNode x : type) {
                if (!"null".equals(x.asText())) {
                    return boxed(x.asText());
                }
            }
            return "Object";
        }
        String t = type == null ? "" : type.asText();
        switch (t) {
            case "array":
                // Element types are boxed: Java generics can't hold primitives.
                return "List<" + typeOf(s.path("items"), true, hint + "Item") + ">";
            case "object":
                if (hasProps(s)) {
                    addRecord(hint, s);
                    return hint;
                }
                return "Map<String, Object>";
            case "integer":
                return optional ? "Long" : "long";
            case "number":
                return optional ? "Double" : "double";
            case "boolean":
                return optional ? "Boolean" : "boolean";
            case "string":
                return "String";
            default:
                return "Object";
        }
    }

    private static String boxed(String jsonType) {
        switch (jsonType) {
            case "integer":
                return "Long";
            case "number":
                return "Double";
            case "boolean":
                return "Boolean";
            default:
                return "String";
        }
    }

    private static String refType(String name) {
        switch (name) {
            case "Properties":
                return "Map<String, Object>";
            case "NodeRef":
                return "Object";
            default:
                return name;
        }
    }

    // --- helpers ---

    private static boolean hasProps(JsonNode s) {
        JsonNode p = s.path("properties");
        return p.isObject() && p.size() > 0;
    }

    private static Set<String> toSet(JsonNode arr) {
        Set<String> set = new LinkedHashSet<>();
        if (arr.isArray()) {
            arr.forEach(n -> set.add(n.asText()));
        }
        return set;
    }

    private static String refBase(String ref) {
        int i = ref.lastIndexOf('/');
        return i >= 0 ? ref.substring(i + 1) : ref;
    }

    private static String camel(String wire) {
        String[] parts = wire.split("_");
        StringBuilder b = new StringBuilder(parts[0]);
        for (int i = 1; i < parts.length; i++) {
            b.append(capitalize(parts[i]));
        }
        return b.toString();
    }

    private static String methodName(String rpc) {
        String[] parts = rpc.split("[._]");
        StringBuilder b = new StringBuilder(parts[0]);
        for (int i = 1; i < parts.length; i++) {
            b.append(capitalize(parts[i]));
        }
        return b.toString();
    }

    private static String pascalName(String rpc) {
        StringBuilder b = new StringBuilder();
        for (String p : rpc.split("[._]")) {
            b.append(capitalize(p));
        }
        return b.toString();
    }

    private static String capitalize(String s) {
        if (s.isEmpty()) {
            return s;
        }
        return Character.toUpperCase(s.charAt(0)) + s.substring(1);
    }

    private static String oneLine(String s) {
        return s.replaceAll("\\s+", " ").trim();
    }
}
