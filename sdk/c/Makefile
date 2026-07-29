CC ?= cc
CFLAGS ?= -O2 -Wall -Wextra -std=gnu11

JSONC_CFLAGS := $(shell pkg-config --cflags json-c)
JSONC_LIBS   := $(shell pkg-config --libs json-c)
CURL_CFLAGS  := $(shell pkg-config --cflags libcurl)
CURL_LIBS    := $(shell pkg-config --libs libcurl)

INC        := -Iinclude
ALL_CFLAGS := $(CFLAGS) $(INC) $(JSONC_CFLAGS) $(CURL_CFLAGS)
ALL_LIBS   := $(JSONC_LIBS) $(CURL_LIBS)

SCHEMA := ../../crates/dr-strange-web/openrpc.json
GEN_H  := include/drsg_generated.h
GEN_C  := src/drsg_generated.c

SRC := src/drsg.c src/drsg_generated.c
OBJ := $(SRC:.c=.o)

libdrsg.a: $(OBJ)
	ar rcs $@ $(OBJ)

src/%.o: src/%.c include/drsg.h $(GEN_H)
	$(CC) $(ALL_CFLAGS) -c $< -o $@

drsg-codegen: codegen/codegen.c
	$(CC) $(CFLAGS) $(JSONC_CFLAGS) $< -o $@ $(JSONC_LIBS)

# Regenerate the committed client from the schema.
generate: drsg-codegen
	./drsg-codegen $(SCHEMA) $(GEN_H) $(GEN_C)

# Fail if the committed client has drifted from the schema.
check-drift: drsg-codegen
	@./drsg-codegen $(SCHEMA) /tmp/drsg_gen.h /tmp/drsg_gen.c
	@diff -u $(GEN_H) /tmp/drsg_gen.h && diff -u $(GEN_C) /tmp/drsg_gen.c \
		&& echo "drift check: OK" \
		|| { echo "drift check: STALE — run 'make generate'"; exit 1; }

test/e2e: test/e2e.c libdrsg.a
	$(CC) $(ALL_CFLAGS) $< -o $@ libdrsg.a $(ALL_LIBS)

# Build the quickstart example (run against a drsg serve on :7700).
example: examples/quickstart.c libdrsg.a
	$(CC) $(ALL_CFLAGS) $< -o $@ libdrsg.a $(ALL_LIBS)

# Drift check + end-to-end suite against a real drsg serve.
test: check-drift test/e2e
	./test/run.sh

clean:
	rm -f $(OBJ) libdrsg.a drsg-codegen test/e2e example

.PHONY: generate check-drift test clean
