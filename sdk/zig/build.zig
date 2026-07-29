const std = @import("std");

// The Zig client binds the C library (sdk/c): it @cImports drsg.h and compiles
// the C sources directly, linking system libcurl + json-c via pkg-config.
fn wireC(mod: *std.Build.Module, b: *std.Build) void {
    mod.link_libc = true;
    mod.addIncludePath(b.path("../c/include"));
    // For <json-c/json.h> pulled in by drsg.h (json-c lives under /usr/include).
    mod.addSystemIncludePath(.{ .cwd_relative = "/usr/include" });
    mod.addCSourceFiles(.{
        .root = b.path("../c"),
        .files = &.{ "src/drsg.c", "src/drsg_generated.c" },
        .flags = &.{"-std=gnu11"},
    });
    mod.linkSystemLibrary("json-c", .{ .use_pkg_config = .force });
    mod.linkSystemLibrary("libcurl", .{ .use_pkg_config = .force });
}

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    const drsg_mod = b.addModule("drsg", .{
        .root_source_file = b.path("src/drsg.zig"),
        .target = target,
        .optimize = optimize,
    });
    wireC(drsg_mod, b);

    // e2e test (drives a real drsg serve; started by test/run.sh).
    const test_mod = b.createModule(.{
        .root_source_file = b.path("test/e2e.zig"),
        .target = target,
        .optimize = optimize,
    });
    test_mod.addImport("drsg", drsg_mod);

    const tests = b.addTest(.{ .root_module = test_mod });
    const run_tests = b.addRunArtifact(tests);
    const test_step = b.step("test", "Run the e2e test (needs $DRSG_BASE_URL; see test/run.sh)");
    test_step.dependOn(&run_tests.step);

    // Quickstart example: `zig build example` (needs a drsg serve on :7700).
    const example_mod = b.createModule(.{
        .root_source_file = b.path("examples/quickstart.zig"),
        .target = target,
        .optimize = optimize,
    });
    example_mod.addImport("drsg", drsg_mod);
    const example_exe = b.addExecutable(.{ .name = "quickstart", .root_module = example_mod });
    const run_example = b.addRunArtifact(example_exe);
    const example_step = b.step("example", "Run the quickstart example");
    example_step.dependOn(&run_example.step);
}
