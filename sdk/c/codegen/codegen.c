/*
 * Generate the typed dr-strange C client from the OpenRPC schema.
 *
 * Schema-first: crates/dr-strange-web/openrpc.json is the single source of
 * truth. Emits drsg_generated.h / drsg_generated.c — one function per RPC
 * method named drsg_<method> (dots -> underscores), taking the required params
 * as C arguments, the optional params in a nullable `..._opts` struct (NULL
 * field = omitted), and returning the result json_object.
 *
 * Usage: codegen <schema.json> <out_header> <out_source>
 * `make check-drift` fails if the committed output has drifted from the schema.
 */
#include <json-c/json.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef enum { T_STR, T_INT, T_NUM, T_BOOL, T_JSON } ctype;

static ctype classify(struct json_object *schema) {
    struct json_object *node;
    if (json_object_object_get_ex(schema, "$ref", &node)) {
        return T_JSON;
    }
    if (!json_object_object_get_ex(schema, "type", &node)) {
        return T_JSON;
    }
    const char *t = json_object_get_string(node);
    if (!strcmp(t, "string")) return T_STR;
    if (!strcmp(t, "integer")) return T_INT;
    if (!strcmp(t, "number")) return T_NUM;
    if (!strcmp(t, "boolean")) return T_BOOL;
    return T_JSON; /* array, object */
}

/* drsg function suffix: dots -> underscores (node.create -> node_create). */
static char *ident(const char *rpc) {
    char *s = strdup(rpc);
    for (char *p = s; *p; p++) {
        if (*p == '.') {
            *p = '_';
        }
    }
    return s;
}

static void print_arg_decl(FILE *f, ctype t, const char *name) {
    switch (t) {
        case T_STR:  fprintf(f, "const char *%s", name); break;
        case T_INT:  fprintf(f, "int64_t %s", name); break;
        case T_NUM:  fprintf(f, "double %s", name); break;
        case T_BOOL: fprintf(f, "int %s", name); break;
        case T_JSON: fprintf(f, "struct json_object *%s", name); break;
    }
}

static void print_opt_field(FILE *f, ctype t, const char *name) {
    switch (t) {
        case T_STR:  fprintf(f, "    const char *%s;\n", name); break;
        case T_INT:  fprintf(f, "    const int64_t *%s;\n", name); break;
        case T_NUM:  fprintf(f, "    const double *%s;\n", name); break;
        case T_BOOL: fprintf(f, "    const int *%s;\n", name); break;
        case T_JSON: fprintf(f, "    struct json_object *%s;\n", name); break;
    }
}

static void print_required_add(FILE *f, ctype t, const char *name) {
    fprintf(f, "    json_object_object_add(p, \"%s\", ", name);
    switch (t) {
        case T_STR:  fprintf(f, "json_object_new_string(%s));\n", name); break;
        case T_INT:  fprintf(f, "json_object_new_int64(%s));\n", name); break;
        case T_NUM:  fprintf(f, "json_object_new_double(%s));\n", name); break;
        case T_BOOL: fprintf(f, "json_object_new_boolean(%s));\n", name); break;
        case T_JSON: fprintf(f, "json_object_get(%s));\n", name); break;
    }
}

static void print_optional_add(FILE *f, ctype t, const char *name) {
    fprintf(f, "        if (opts->%s) json_object_object_add(p, \"%s\", ", name, name);
    switch (t) {
        case T_STR:  fprintf(f, "json_object_new_string(opts->%s));\n", name); break;
        case T_INT:  fprintf(f, "json_object_new_int64(*opts->%s));\n", name); break;
        case T_NUM:  fprintf(f, "json_object_new_double(*opts->%s));\n", name); break;
        case T_BOOL: fprintf(f, "json_object_new_boolean(*opts->%s));\n", name); break;
        case T_JSON: fprintf(f, "json_object_get(opts->%s));\n", name); break;
    }
}

/* A parameter, resolved from the schema. */
struct param {
    const char *name;
    ctype type;
    int required;
};

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: %s <schema.json> <out_header> <out_source>\n", argv[0]);
        return 2;
    }
    struct json_object *doc = json_object_from_file(argv[1]);
    if (!doc) {
        fprintf(stderr, "cannot read schema %s\n", argv[1]);
        return 1;
    }
    FILE *fh = fopen(argv[2], "w");
    FILE *fc = fopen(argv[3], "w");
    if (!fh || !fc) {
        fprintf(stderr, "cannot open outputs\n");
        return 1;
    }

    fprintf(fh, "// Code generated from crates/dr-strange-web/openrpc.json by codegen.c; DO NOT EDIT.\n");
    fprintf(fh, "#ifndef DRSG_GENERATED_H\n#define DRSG_GENERATED_H\n\n#include \"drsg.h\"\n\n");
    fprintf(fc, "// Code generated from crates/dr-strange-web/openrpc.json by codegen.c; DO NOT EDIT.\n");
    fprintf(fc, "#include \"drsg.h\"\n\n");

    struct json_object *methods;
    json_object_object_get_ex(doc, "methods", &methods);
    size_t nm = json_object_array_length(methods);

    for (size_t i = 0; i < nm; i++) {
        struct json_object *m = json_object_array_get_idx(methods, i);
        struct json_object *node;

        json_object_object_get_ex(m, "name", &node);
        const char *wire = json_object_get_string(node);
        char *id = ident(wire);

        const char *summary = "";
        if (json_object_object_get_ex(m, "summary", &node)) {
            summary = json_object_get_string(node);
        }
        const char *access = "";
        if (json_object_object_get_ex(m, "x-access", &node)) {
            access = json_object_get_string(node);
        }

        struct param req[16], opt[16];
        int nreq = 0, nopt = 0;
        struct json_object *params;
        if (json_object_object_get_ex(m, "params", &params)) {
            size_t np = json_object_array_length(params);
            for (size_t j = 0; j < np; j++) {
                struct json_object *p = json_object_array_get_idx(params, j);
                struct json_object *pn, *ps, *pr;
                json_object_object_get_ex(p, "name", &pn);
                json_object_object_get_ex(p, "schema", &ps);
                int required = json_object_object_get_ex(p, "required", &pr)
                        && json_object_get_boolean(pr);
                struct param entry = {json_object_get_string(pn), classify(ps), required};
                if (required) {
                    req[nreq++] = entry;
                } else {
                    opt[nopt++] = entry;
                }
            }
        }

        char doc_comment[512];
        if (access[0]) {
            snprintf(doc_comment, sizeof doc_comment, "/* %s (access: %s) */", summary, access);
        } else {
            snprintf(doc_comment, sizeof doc_comment, "/* %s */", summary);
        }

        /* Optional-params struct. */
        if (nopt > 0) {
            fprintf(fh, "typedef struct {\n");
            for (int k = 0; k < nopt; k++) {
                print_opt_field(fh, opt[k].type, opt[k].name);
            }
            fprintf(fh, "} drsg_%s_opts;\n\n", id);
        }

        /* Prototype (header) and definition (source) share a signature. */
        for (int pass = 0; pass < 2; pass++) {
            FILE *f = pass == 0 ? fh : fc;
            fprintf(f, "%s\n", doc_comment);
            fprintf(f, "struct json_object *drsg_%s(drsg_client *c", id);
            for (int k = 0; k < nreq; k++) {
                fprintf(f, ", ");
                print_arg_decl(f, req[k].type, req[k].name);
            }
            if (nopt > 0) {
                fprintf(f, ", const drsg_%s_opts *opts", id);
            }
            fprintf(f, ", drsg_error *err)");
            if (pass == 0) {
                fprintf(f, ";\n\n");
                continue;
            }
            fprintf(f, " {\n");
            if (nreq == 0 && nopt == 0) {
                fprintf(f, "    struct json_object *p = NULL;\n");
            } else {
                fprintf(f, "    struct json_object *p = json_object_new_object();\n");
                for (int k = 0; k < nreq; k++) {
                    print_required_add(f, req[k].type, req[k].name);
                }
                if (nopt > 0) {
                    fprintf(f, "    if (opts) {\n");
                    for (int k = 0; k < nopt; k++) {
                        print_optional_add(f, opt[k].type, opt[k].name);
                    }
                    fprintf(f, "    }\n");
                }
            }
            fprintf(f, "    struct json_object *result = NULL;\n");
            fprintf(f, "    int rc = drsg_call(c, \"%s\", p, &result, err);\n", wire);
            fprintf(f, "    if (p) json_object_put(p);\n");
            fprintf(f, "    return rc == 0 ? result : NULL;\n}\n\n");
        }

        free(id);
    }

    fprintf(fh, "#endif /* DRSG_GENERATED_H */\n");

    fclose(fh);
    fclose(fc);
    json_object_put(doc);
    return 0;
}
