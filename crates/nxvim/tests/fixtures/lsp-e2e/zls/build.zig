// Minimal build.zig: its presence makes this directory a zls project root (one of
// zls's `root_markers`), so the resolved root is this dir rather than an ancestor.
const std = @import("std");

pub fn build(b: *std.Build) void {
    _ = b;
}
