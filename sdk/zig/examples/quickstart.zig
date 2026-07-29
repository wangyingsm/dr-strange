// Minimal dr-strange quickstart — run against a `drsg serve` on :7700.
//   zig build example   (with $DRSG_TOKEN set)
const std = @import("std");
const drsg = @import("drsg");
const c = drsg.c;

pub fn main() !void {
    var client = try drsg.Client.init(null, null); // :7700; token from $DRSG_TOKEN
    defer client.deinit();
    const h = client.handle;
    const err = &client.err;

    const labels = c.json_object_new_array();
    _ = c.json_object_array_add(labels, c.json_object_new_string("Person"));
    var alice_opts = std.mem.zeroes(c.drsg_node_create_opts);
    alice_opts.key = "alice";
    alice_opts.labels = labels;
    _ = c.json_object_put(c.drsg_node_create(h, "startup", &alice_opts, err));
    var bob_opts = std.mem.zeroes(c.drsg_node_create_opts);
    bob_opts.key = "bob";
    bob_opts.labels = labels;
    _ = c.json_object_put(c.drsg_node_create(h, "startup", &bob_opts, err));
    _ = c.json_object_put(labels);

    const src = c.json_object_new_string("alice");
    const dst = c.json_object_new_string("bob");
    _ = c.json_object_put(c.drsg_edge_create(h, "startup", src, dst, "KNOWS", null, err));
    _ = c.json_object_put(src);
    _ = c.json_object_put(dst);

    const set = c.json_object_new_object();
    _ = c.json_object_object_add(set, "age", c.json_object_new_int(30));
    var update = std.mem.zeroes(c.drsg_node_update_opts);
    update.key = "alice";
    update.set = set;
    _ = c.json_object_put(c.drsg_node_update(h, "startup", &update, err));
    _ = c.json_object_put(set);

    var get = std.mem.zeroes(c.drsg_node_get_opts);
    get.key = "alice";
    const alice = c.drsg_node_get(h, "startup", &get, err).?;
    var props: ?*c.json_object = null;
    var age: ?*c.json_object = null;
    _ = c.json_object_object_get_ex(alice, "properties", &props);
    _ = c.json_object_object_get_ex(props, "age", &age);
    std.debug.print("alice.age = {d}\n", .{c.json_object_get_int(age)});
    _ = c.json_object_put(alice);

    const stats = c.drsg_db_stats(h, err).?;
    var nodes: ?*c.json_object = null;
    var edges: ?*c.json_object = null;
    _ = c.json_object_object_get_ex(stats, "nodes", &nodes);
    _ = c.json_object_object_get_ex(stats, "edges", &edges);
    std.debug.print("{d} nodes, {d} edge(s)\n", .{ c.json_object_get_int(nodes), c.json_object_get_int(edges) });
    _ = c.json_object_put(stats);
}
