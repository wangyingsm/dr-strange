// End-to-end test: drive a real `drsg serve` over the C-backed client.
//
// The server is started by test/run.sh, which exports $DRSG_BASE_URL and
// $DRSG_TOKEN. The test skips if $DRSG_BASE_URL is unset.
const std = @import("std");
const drsg = @import("drsg");
const c = drsg.c;

fn intField(obj: *c.json_object, key: [*c]const u8) i64 {
    var v: ?*c.json_object = null;
    if (c.json_object_object_get_ex(obj, key, &v) != 0) {
        return c.json_object_get_int64(v);
    }
    return -1;
}

fn strField(obj: *c.json_object, key: [*c]const u8) [*c]const u8 {
    var v: ?*c.json_object = null;
    if (c.json_object_object_get_ex(obj, key, &v) != 0) {
        return c.json_object_get_string(v);
    }
    return null;
}

fn hasField(obj: *c.json_object, key: [*c]const u8) bool {
    var v: ?*c.json_object = null;
    return c.json_object_object_get_ex(obj, key, &v) != 0;
}

test "e2e: crud, plane admin, discover, auth" {
    const base = c.getenv("DRSG_BASE_URL") orelse return error.SkipZigTest;

    var client = try drsg.Client.init(base, null); // token from $DRSG_TOKEN
    defer client.deinit();
    const h = client.handle;

    // db.stats -> 0 nodes.
    {
        const stats = c.drsg_db_stats(h, &client.err) orelse return error.CallFailed;
        defer _ = c.json_object_put(stats);
        try std.testing.expectEqual(@as(i64, 0), intField(stats, "nodes"));
    }

    // node.create alice + bob.
    const labels = c.json_object_new_array();
    defer _ = c.json_object_put(labels);
    _ = c.json_object_array_add(labels, c.json_object_new_string("Person"));
    {
        var opts = std.mem.zeroes(c.drsg_node_create_opts);
        opts.key = "alice";
        opts.labels = labels;
        const alice = c.drsg_node_create(h, "startup", &opts, &client.err) orelse return error.CallFailed;
        defer _ = c.json_object_put(alice);
        try std.testing.expectEqualStrings("alice", std.mem.span(strField(alice, "external_key")));
    }
    {
        var opts = std.mem.zeroes(c.drsg_node_create_opts);
        opts.key = "bob";
        opts.labels = labels;
        const bob = c.drsg_node_create(h, "startup", &opts, &client.err) orelse return error.CallFailed;
        _ = c.json_object_put(bob);
    }

    // edge.create alice -KNOWS-> bob (endpoints by key).
    {
        const src = c.json_object_new_string("alice");
        defer _ = c.json_object_put(src);
        const dst = c.json_object_new_string("bob");
        defer _ = c.json_object_put(dst);
        const edge = c.drsg_edge_create(h, "startup", src, dst, "KNOWS", null, &client.err) orelse return error.CallFailed;
        defer _ = c.json_object_put(edge);
        try std.testing.expectEqualStrings("KNOWS", std.mem.span(strField(edge, "type")));
    }

    // node.update: set then unset, types preserved.
    {
        const set = c.json_object_new_object();
        defer _ = c.json_object_put(set);
        _ = c.json_object_object_add(set, "age", c.json_object_new_int(41));
        _ = c.json_object_object_add(set, "city", c.json_object_new_string("NYC"));
        var opts = std.mem.zeroes(c.drsg_node_update_opts);
        opts.key = "alice";
        opts.set = set;
        const upd = c.drsg_node_update(h, "startup", &opts, &client.err) orelse return error.CallFailed;
        defer _ = c.json_object_put(upd);
        var props: ?*c.json_object = null;
        _ = c.json_object_object_get_ex(upd, "properties", &props);
        try std.testing.expectEqual(@as(i64, 41), intField(props.?, "age"));
    }
    {
        const unset = c.json_object_new_array();
        defer _ = c.json_object_put(unset);
        _ = c.json_object_array_add(unset, c.json_object_new_string("city"));
        var opts = std.mem.zeroes(c.drsg_node_update_opts);
        opts.key = "alice";
        opts.unset = unset;
        const upd = c.drsg_node_update(h, "startup", &opts, &client.err) orelse return error.CallFailed;
        defer _ = c.json_object_put(upd);
        var props: ?*c.json_object = null;
        _ = c.json_object_object_get_ex(upd, "properties", &props);
        try std.testing.expect(!hasField(props.?, "city"));
    }

    // node.get reads it back.
    {
        var opts = std.mem.zeroes(c.drsg_node_get_opts);
        opts.key = "alice";
        const got = c.drsg_node_get(h, "startup", &opts, &client.err) orelse return error.CallFailed;
        defer _ = c.json_object_put(got);
        var props: ?*c.json_object = null;
        _ = c.json_object_object_get_ex(got, "properties", &props);
        try std.testing.expectEqual(@as(i64, 41), intField(props.?, "age"));
    }

    // node.delete cascades the edge.
    {
        var opts = std.mem.zeroes(c.drsg_node_delete_opts);
        opts.key = "alice";
        const del = c.drsg_node_delete(h, "startup", &opts, &client.err) orelse return error.CallFailed;
        defer _ = c.json_object_put(del);
        try std.testing.expectEqual(@as(i64, 1), intField(del, "deleted"));
    }
    {
        const stats = c.drsg_db_stats(h, &client.err) orelse return error.CallFailed;
        defer _ = c.json_object_put(stats);
        try std.testing.expectEqual(@as(i64, 1), intField(stats, "nodes"));
        try std.testing.expectEqual(@as(i64, 0), intField(stats, "edges"));
    }

    // plane admin.
    {
        const plane = c.drsg_plane_create(h, "notes", null, &client.err) orelse return error.CallFailed;
        defer _ = c.json_object_put(plane);
        try std.testing.expectEqualStrings("notes", std.mem.span(strField(plane, "name")));
    }
    {
        const renamed = c.drsg_plane_rename(h, "notes", "archive", &client.err) orelse return error.CallFailed;
        defer _ = c.json_object_put(renamed);
        try std.testing.expectEqualStrings("archive", std.mem.span(strField(renamed, "name")));
    }
    {
        const del = c.drsg_plane_delete(h, "archive", &client.err) orelse return error.CallFailed;
        defer _ = c.json_object_put(del);
        try std.testing.expectEqual(@as(i64, 1), intField(del, "deleted"));
    }

    // rpc.discover.
    {
        const doc = c.drsg_rpc_discover(h, &client.err) orelse return error.CallFailed;
        defer _ = c.json_object_put(doc);
        try std.testing.expectEqualStrings("1.2.6", std.mem.span(strField(doc, "openrpc")));
    }

    // Bad token -> Unauthorized (-32001).
    {
        var bad = try drsg.Client.init(base, "wrong");
        defer bad.deinit();
        const denied = c.drsg_db_stats(bad.handle, &bad.err);
        try std.testing.expect(denied == null);
        try std.testing.expect(c.drsg_is_auth_error(&bad.err) != 0);
        try std.testing.expectEqual(@as(c_int, drsg.AUTH_ERROR_CODE), bad.err.code);
    }
}
