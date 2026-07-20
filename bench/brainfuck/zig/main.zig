const std = @import("std");

const OpKind = enum {
    Add,
    Move,
    Out,
    In,
    Loop,
};

const Op = struct {
    kind: OpKind,
    value: i64,
    body: []Op,
};

fn parse_body(allocator: std.mem.Allocator, source: []const u8, pos: *usize) ![]Op {
    var acc: std.ArrayList(Op) = .empty;
    errdefer acc.deinit(allocator);

    while (pos.* < source.len) {
        const c = source[pos.*];
        if (c == '+' or c == '-') {
            var val: i64 = if (c == '+') 1 else -1;
            pos.* += 1;
            while (pos.* < source.len and (source[pos.*] == '+' or source[pos.*] == '-')) {
                val += if (source[pos.*] == '+') @as(i64, 1) else -1;
                pos.* += 1;
            }
            if (val != 0) {
                try acc.append(allocator, Op{ .kind = .Add, .value = val, .body = &.{} });
            }
        } else if (c == '>' or c == '<') {
            var val: i64 = if (c == '>') 1 else -1;
            pos.* += 1;
            while (pos.* < source.len and (source[pos.*] == '>' or source[pos.*] == '<')) {
                val += if (source[pos.*] == '>') @as(i64, 1) else -1;
                pos.* += 1;
            }
            if (val != 0) {
                try acc.append(allocator, Op{ .kind = .Move, .value = val, .body = &.{} });
            }
        } else if (c == '.') {
            try acc.append(allocator, Op{ .kind = .Out, .value = 0, .body = &.{} });
            pos.* += 1;
        } else if (c == ',') {
            try acc.append(allocator, Op{ .kind = .In, .value = 0, .body = &.{} });
            pos.* += 1;
        } else if (c == '[') {
            pos.* += 1;
            const body = try parse_body(allocator, source, pos);
            try acc.append(allocator, Op{ .kind = .Loop, .value = 0, .body = body });
        } else if (c == ']') {
            pos.* += 1;
            return try acc.toOwnedSlice(allocator);
        } else {
            pos.* += 1;
        }
    }
    return try acc.toOwnedSlice(allocator);
}

fn parse(allocator: std.mem.Allocator, source: []const u8) ![]Op {
    var pos: usize = 0;
    return parse_body(allocator, source, &pos);
}

const TAPE_SIZE = 30000;

fn execute(ops: []const Op, tape: *[TAPE_SIZE]i64, ptr: *usize) !void {
    for (ops) |op| {
        switch (op.kind) {
            .Add => {
                tape[ptr.*] += op.value;
            },
            .Move => {
                ptr.* = @intCast(@as(i64, @intCast(ptr.*)) + op.value);
            },
            .Out => {
                const stdout = std.fs.File.stdout();
                var buf: [64]u8 = undefined;
                const str = std.fmt.bufPrint(&buf, "{d}", .{tape[ptr.*]}) catch unreachable;
                stdout.writeAll(str) catch {};
            },
            .In => {},
            .Loop => {
                while (tape[ptr.*] != 0) {
                    try execute(op.body, tape, ptr);
                }
            },
        }
    }
}

fn free_ops(allocator: std.mem.Allocator, ops: []Op) void {
    for (ops) |op| {
        if (op.kind == .Loop) {
            free_ops(allocator, op.body);
        }
    }
    allocator.free(ops);
}

pub fn main() !void {
    const allocator = std.heap.page_allocator;

    const program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]";
    const ops = try parse(allocator, program);
    defer free_ops(allocator, ops);

    var tape = [_]i64{0} ** TAPE_SIZE;
    var ptr: usize = 0;
    try execute(ops, &tape, &ptr);

    const stdout = std.fs.File.stdout();
    var buf: [128]u8 = undefined;
    const str = std.fmt.bufPrint(&buf, "Result: {d}\n", .{tape[8]}) catch unreachable;
    stdout.writeAll(str) catch {};
}
