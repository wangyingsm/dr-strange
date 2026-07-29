/* Minimal dr-strange quickstart — run against a `drsg serve` on :7700.
 *   make example && DRSG_TOKEN=… ./example
 */
#include "drsg.h"
#include <stdio.h>

int main(void) {
    drsg_client *c = drsg_client_new(NULL, NULL); /* :7700; token from $DRSG_TOKEN */
    drsg_error err;

    struct json_object *labels = json_object_new_array();
    json_object_array_add(labels, json_object_new_string("Person"));

    drsg_node_create_opts alice_opts = {.key = "alice", .labels = labels};
    json_object_put(drsg_node_create(c, "startup", &alice_opts, &err));
    drsg_node_create_opts bob_opts = {.key = "bob", .labels = labels};
    json_object_put(drsg_node_create(c, "startup", &bob_opts, &err));
    json_object_put(labels);

    struct json_object *src = json_object_new_string("alice");
    struct json_object *dst = json_object_new_string("bob");
    json_object_put(drsg_edge_create(c, "startup", src, dst, "KNOWS", NULL, &err));
    json_object_put(src);
    json_object_put(dst);

    struct json_object *set = json_object_new_object();
    json_object_object_add(set, "age", json_object_new_int(30));
    drsg_node_update_opts update = {.key = "alice", .set = set};
    json_object_put(drsg_node_update(c, "startup", &update, &err));
    json_object_put(set);

    drsg_node_get_opts get = {.key = "alice"};
    struct json_object *alice = drsg_node_get(c, "startup", &get, &err);
    struct json_object *props = NULL, *age = NULL;
    json_object_object_get_ex(alice, "properties", &props);
    json_object_object_get_ex(props, "age", &age);
    printf("alice.age = %d\n", json_object_get_int(age));
    json_object_put(alice);

    struct json_object *stats = drsg_db_stats(c, &err);
    struct json_object *nodes = NULL, *edges = NULL;
    json_object_object_get_ex(stats, "nodes", &nodes);
    json_object_object_get_ex(stats, "edges", &edges);
    printf("%d nodes, %d edge(s)\n", json_object_get_int(nodes), json_object_get_int(edges));
    json_object_put(stats);

    drsg_client_free(c);
    return 0;
}
