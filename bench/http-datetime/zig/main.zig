// http-datetime: raw-socket HTTP/1.1 server in Zig (std only).
//
// Answers every request with the current UTC time in ISO 8601
// (YYYY-MM-DDTHH:MM:SSZ), computed per request. Keep-alive: one detached
// std.Thread per accepted connection, looping until the peer hangs up or
// asks to close.
//
// Zig's std has no date formatting, so the civil date is derived from the
// Unix timestamp with Howard Hinnant's days-from-civil algorithm.

const std = @import("std");

/// Howard Hinnant's algorithm: days since the Unix epoch -> (year, month, day).
fn civilFromDays(z_in: i64) struct { y: i64, m: u32, d: u32 } {
    const z = z_in + 719468;
    const era = @divFloor(if (z >= 0) z else z - 146096, 146097);
    const doe = z - era * 146097; // [0, 146096]
    const yoe = @divTrunc(doe - @divTrunc(doe, 1460) + @divTrunc(doe, 36524) - @divTrunc(doe, 146096), 365); // [0, 399]
    const y = yoe + era * 400;
    const doy = doe - (365 * yoe + @divTrunc(yoe, 4) - @divTrunc(yoe, 100)); // [0, 365]
    const mp = @divTrunc(5 * doy + 2, 153); // [0, 11]
    const d = doy - @divTrunc(153 * mp + 2, 5) + 1; // [1, 31]
    const m = if (mp < 10) mp + 3 else mp - 9; // [1, 12]
    const year = if (m <= 2) y + 1 else y;
    return .{ .y = year, .m = @intCast(m), .d = @intCast(d) };
}

/// Write the current UTC time as YYYY-MM-DDTHH:MM:SSZ into buf.
fn nowIso8601(buf: []u8) []const u8 {
    const secs: i64 = std.time.timestamp();
    const days = @divFloor(secs, 86400);
    const rem = @mod(secs, 86400);
    const hour = @divTrunc(rem, 3600);
    const min = @divTrunc(@mod(rem, 3600), 60);
    const sec = @mod(rem, 60);
    const c = civilFromDays(days);
    // Cast the (known non-negative) components to unsigned so std.fmt zero-pads
    // them without a sign prefix.
    const year: u32 = @intCast(c.y);
    const hh: u32 = @intCast(hour);
    const mm: u32 = @intCast(min);
    const ss: u32 = @intCast(sec);
    return std.fmt.bufPrint(buf, "{d:0>4}-{d:0>2}-{d:0>2}T{d:0>2}:{d:0>2}:{d:0>2}Z", .{
        year, c.m, c.d, hh, mm, ss,
    }) catch unreachable;
}

/// Read from fd until a full "\r\n\r\n" header terminator is seen.
/// Returns true on a complete header, false on EOF/close/error.
fn readRequest(fd: std.posix.socket_t) bool {
    var buf: [4096]u8 = undefined;
    const term = [4]u8{ '\r', '\n', '\r', '\n' };
    var matched: usize = 0;
    while (true) {
        const n = std.posix.recv(fd, &buf, 0) catch return false;
        if (n == 0) return false;
        for (buf[0..n]) |b| {
            if (b == term[matched]) {
                matched += 1;
                if (matched == 4) return true;
            } else {
                matched = if (b == '\r') 1 else 0;
            }
        }
    }
}

fn writeAll(fd: std.posix.socket_t, bytes: []const u8) bool {
    var off: usize = 0;
    while (off < bytes.len) {
        const n = std.posix.send(fd, bytes[off..], 0) catch return false;
        if (n == 0) return false;
        off += n;
    }
    return true;
}

fn handleConn(fd: std.posix.socket_t) void {
    defer std.posix.close(fd);
    var tsbuf: [32]u8 = undefined;
    var respbuf: [256]u8 = undefined;
    while (true) {
        if (!readRequest(fd)) break;
        const ts = nowIso8601(&tsbuf);
        const resp = std.fmt.bufPrint(&respbuf, "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {d}\r\n\r\n{s}", .{ ts.len, ts }) catch break;
        if (!writeAll(fd, resp)) break;
    }
}

pub fn main() !void {
    const port: u16 = blk: {
        if (std.posix.getenv("PORT")) |v| {
            break :blk std.fmt.parseInt(u16, v, 10) catch 18080;
        }
        break :blk 18080;
    };

    const addr = try std.net.Address.parseIp4("127.0.0.1", port);

    const sock = try std.posix.socket(
        std.posix.AF.INET,
        std.posix.SOCK.STREAM,
        std.posix.IPPROTO.TCP,
    );
    defer std.posix.close(sock);

    try std.posix.setsockopt(
        sock,
        std.posix.SOL.SOCKET,
        std.posix.SO.REUSEADDR,
        &std.mem.toBytes(@as(c_int, 1)),
    );

    try std.posix.bind(sock, &addr.any, addr.getOsSockLen());
    try std.posix.listen(sock, 1024);

    while (true) {
        const fd = std.posix.accept(sock, null, null, 0) catch continue;
        const th = std.Thread.spawn(.{}, handleConn, .{fd}) catch {
            std.posix.close(fd);
            continue;
        };
        th.detach();
    }
}
