// Package gen renders the typed dr-strange client from the OpenRPC schema.
//
// Schema-first: crates/dr-strange-web/openrpc.json is the single source of
// truth. Render emits generated.go — the component types plus one method per
// RPC method, named PascalCase (node.create -> NodeCreate), taking a params
// struct with the schema's wire field names and returning the typed result.
//
// cmd/gen writes the file (`go generate ./...`); generated_test.go fails if the
// committed output has drifted from the schema.
package gen

import (
	"encoding/json"
	"fmt"
	"go/format"
	"sort"
	"strconv"
	"strings"
)

type schema = map[string]any

type param struct {
	Name     string `json:"name"`
	Required bool   `json:"required"`
	Schema   schema `json:"schema"`
}

type method struct {
	Name    string  `json:"name"`
	Summary string  `json:"summary"`
	Access  string  `json:"x-access"`
	Params  []param `json:"params"`
	Result  *struct {
		Schema schema `json:"schema"`
	} `json:"result"`
}

type openrpc struct {
	Methods    []method `json:"methods"`
	Components struct {
		Schemas map[string]schema `json:"schemas"`
	} `json:"components"`
}

type gen struct {
	aux  []string
	seen map[string]bool
}

// Render returns the formatted source of generated.go for an OpenRPC document.
func Render(schemaJSON []byte) ([]byte, error) {
	var doc openrpc
	if err := json.Unmarshal(schemaJSON, &doc); err != nil {
		return nil, fmt.Errorf("parse schema: %w", err)
	}

	g := &gen{seen: map[string]bool{}}

	names := make([]string, 0, len(doc.Components.Schemas))
	for name := range doc.Components.Schemas {
		names = append(names, name)
		g.seen[name] = true // reserve component names so aux types never collide
	}
	sort.Strings(names)

	var comps strings.Builder
	for _, name := range names {
		comps.WriteString(g.renderComponent(name, doc.Components.Schemas[name]))
		comps.WriteString("\n")
	}

	var methods strings.Builder
	for _, m := range doc.Methods {
		methods.WriteString(g.renderMethod(m)) // side effect: appends params/result aux
		methods.WriteString("\n")
	}

	var b strings.Builder
	b.WriteString("// Code generated from crates/dr-strange-web/openrpc.json by internal/gen; DO NOT EDIT.\n\n")
	b.WriteString("package drsg\n\n")
	b.WriteString("import \"context\"\n\n")
	b.WriteString(comps.String())
	for _, a := range g.aux {
		b.WriteString(a)
		b.WriteString("\n")
	}
	b.WriteString(methods.String())

	src, err := format.Source([]byte(b.String()))
	if err != nil {
		return nil, fmt.Errorf("format generated source: %w\n%s", err, b.String())
	}
	return src, nil
}

func (g *gen) renderComponent(name string, s schema) string {
	desc := ""
	if d, ok := s["description"].(string); ok && d != "" {
		desc = "// " + name + " — " + oneLine(d) + "\n"
	}
	if s["type"] == "object" && hasProps(s) {
		return desc + g.renderStruct(name, s)
	}
	base, _ := g.baseType(s, name)
	return desc + fmt.Sprintf("type %s = %s\n", name, base)
}

func (g *gen) renderMethod(m method) string {
	name := pascalMethod(m.Name)
	var resultSchema schema
	if m.Result != nil {
		resultSchema = m.Result.Schema
	}
	result := g.resultType(resultSchema, name)

	doc := oneLine(m.Summary)
	if m.Access != "" {
		doc += " (access: " + m.Access + ")"
	}
	wire := strconv.Quote(m.Name)

	var b strings.Builder
	fmt.Fprintf(&b, "// %s %s\n", name, doc)
	if len(m.Params) == 0 {
		fmt.Fprintf(&b, "func (c *Client) %s(ctx context.Context) (%s, error) {\n", name, result)
		fmt.Fprintf(&b, "\tvar out %s\n", result)
		fmt.Fprintf(&b, "\terr := c.call(ctx, %s, nil, &out)\n", wire)
		b.WriteString("\treturn out, err\n}\n")
		return b.String()
	}
	pname := name + "Params"
	g.addParams(pname, m.Params)
	fmt.Fprintf(&b, "func (c *Client) %s(ctx context.Context, p %s) (%s, error) {\n", name, pname, result)
	fmt.Fprintf(&b, "\tvar out %s\n", result)
	fmt.Fprintf(&b, "\terr := c.call(ctx, %s, p, &out)\n", wire)
	b.WriteString("\treturn out, err\n}\n")
	return b.String()
}

// resultType maps a method's result schema to the Go type the call unmarshals
// into: object components and inline objects become pointers, arrays slices,
// untyped objects map[string]any.
func (g *gen) resultType(s schema, method string) string {
	if s == nil {
		return "any"
	}
	if ref, ok := s["$ref"].(string); ok {
		name := refBase(ref)
		if name == "Properties" || name == "NodeRef" {
			return name
		}
		return "*" + name
	}
	if one, ok := s["oneOf"].([]any); ok {
		for _, a := range one {
			if am, ok := a.(map[string]any); ok {
				if ref, ok := am["$ref"].(string); ok {
					return "*" + refBase(ref)
				}
			}
		}
		return "any"
	}
	switch s["type"] {
	case "array":
		items, _ := s["items"].(map[string]any)
		if ref, ok := items["$ref"].(string); ok {
			return "[]" + refBase(ref)
		}
		if items["type"] == "object" && hasProps(items) {
			name := method + "Item"
			g.addStruct(name, items)
			return "[]" + name
		}
		it, _ := g.baseType(items, method+"Item")
		return "[]" + it
	case "object":
		if hasProps(s) {
			name := method + "Result"
			g.addStruct(name, s)
			return "*" + name
		}
		return "map[string]any"
	}
	t, _ := g.baseType(s, method+"Result")
	return t
}

func (g *gen) renderStruct(name string, s schema) string {
	props, _ := s["properties"].(map[string]any)
	req := toSet(s["required"])
	var b strings.Builder
	fmt.Fprintf(&b, "type %s struct {\n", name)
	for _, k := range sortedKeys(props) {
		fs, _ := props[k].(map[string]any)
		ft := g.fieldType(fs, req[k], name+exportName(k))
		tag := k
		if !req[k] {
			tag += ",omitempty"
		}
		fmt.Fprintf(&b, "\t%s %s `json:%q`\n", exportName(k), ft, tag)
	}
	b.WriteString("}\n")
	return b.String()
}

func (g *gen) addStruct(name string, s schema) {
	if g.seen[name] {
		return
	}
	g.seen[name] = true
	g.aux = append(g.aux, g.renderStruct(name, s))
}

func (g *gen) addParams(name string, params []param) {
	if g.seen[name] {
		return
	}
	g.seen[name] = true
	var b strings.Builder
	fmt.Fprintf(&b, "type %s struct {\n", name)
	for _, p := range params {
		ft := g.fieldType(p.Schema, p.Required, name+exportName(p.Name))
		tag := p.Name
		if !p.Required {
			tag += ",omitempty"
		}
		fmt.Fprintf(&b, "\t%s %s `json:%q`\n", exportName(p.Name), ft, tag)
	}
	b.WriteString("}\n")
	g.aux = append(g.aux, b.String())
}

// fieldType is baseType plus optional/nullable semantics: composites carry
// their own nil, scalars and struct refs become pointers when optional.
func (g *gen) fieldType(s schema, required bool, hint string) string {
	base, nullable := g.baseType(s, hint)
	if isComposite(base) {
		return base
	}
	if !required || nullable {
		return "*" + base
	}
	return base
}

// baseType maps a JSON-Schema fragment to a Go value type, synthesising a named
// struct (via addStruct) for inline objects. The bool is whether the value is
// nullable (a `[X, null]` union), which fieldType turns into a pointer.
func (g *gen) baseType(s schema, hint string) (string, bool) {
	if s == nil {
		return "any", false
	}
	if ref, ok := s["$ref"].(string); ok {
		return refBase(ref), false
	}
	if one, ok := s["oneOf"].([]any); ok {
		var nonNull schema
		hasNull := false
		for _, a := range one {
			am, _ := a.(map[string]any)
			if am["type"] == "null" {
				hasNull = true
				continue
			}
			nonNull = am
		}
		if nonNull != nil && hasNull {
			t, _ := g.baseType(nonNull, hint)
			return t, true
		}
		return "any", false
	}
	if arr, ok := s["type"].([]any); ok { // e.g. ["string", "null"]
		for _, x := range arr {
			if x != "null" {
				return scalar(x.(string)), true
			}
		}
		return "any", true
	}
	switch s["type"] {
	case "array":
		items, _ := s["items"].(map[string]any)
		it, _ := g.baseType(items, hint+"Item")
		return "[]" + it, false
	case "object":
		if hasProps(s) {
			g.addStruct(hint, s)
			return hint, false
		}
		return "map[string]any", false
	case "string", "integer", "number", "boolean", "null":
		return scalar(s["type"].(string)), false
	}
	return "any", false
}

func isComposite(base string) bool {
	return strings.HasPrefix(base, "[]") ||
		base == "map[string]any" || base == "Properties" ||
		base == "any" || base == "NodeRef"
}

// --- small helpers ---

var initialisms = map[string]bool{"id": true, "url": true, "api": true, "json": true, "http": true}

func exportName(s string) string {
	parts := strings.Split(s, "_")
	for i, p := range parts {
		parts[i] = capitalize(p)
	}
	return strings.Join(parts, "")
}

func pascalMethod(name string) string {
	parts := strings.FieldsFunc(name, func(r rune) bool { return r == '.' || r == '_' })
	for i, p := range parts {
		parts[i] = capitalize(p)
	}
	return strings.Join(parts, "")
}

func capitalize(p string) string {
	if p == "" {
		return p
	}
	if initialisms[p] {
		return strings.ToUpper(p)
	}
	return strings.ToUpper(p[:1]) + p[1:]
}

func scalar(t string) string {
	switch t {
	case "string":
		return "string"
	case "integer":
		return "int64"
	case "number":
		return "float64"
	case "boolean":
		return "bool"
	default:
		return "any"
	}
}

func refBase(ref string) string {
	if i := strings.LastIndex(ref, "/"); i >= 0 {
		return ref[i+1:]
	}
	return ref
}

func hasProps(s schema) bool {
	p, ok := s["properties"].(map[string]any)
	return ok && len(p) > 0
}

func toSet(v any) map[string]bool {
	m := map[string]bool{}
	if arr, ok := v.([]any); ok {
		for _, x := range arr {
			if s, ok := x.(string); ok {
				m[s] = true
			}
		}
	}
	return m
}

func sortedKeys(m map[string]any) []string {
	ks := make([]string, 0, len(m))
	for k := range m {
		ks = append(ks, k)
	}
	sort.Strings(ks)
	return ks
}

func oneLine(s string) string {
	return strings.Join(strings.Fields(s), " ")
}
