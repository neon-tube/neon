//! http-datetime: raw-socket HTTP/1.1 server in Rust, std only.
//!
//! Answers every request with the current UTC time in ISO 8601
//! (YYYY-MM-DDTHH:MM:SSZ), computed per request. Keep-alive: one thread per
//! accepted connection, looping until the peer hangs up or asks to close.
//!
//! Rust's std has no date formatting, so the civil date is derived from the
//! Unix timestamp with Howard Hinnant's days-from-civil algorithm (integer
//! math, no dependencies).

use std::env;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// Format the current UTC time as `YYYY-MM-DDTHH:MM:SSZ`.
fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;

    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;

    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hour, min, sec
    )
}

/// Howard Hinnant's algorithm: convert a count of days since the Unix epoch
/// (1970-01-01) into a (year, month, day) civil date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

/// Read from the stream until a full `\r\n\r\n` header terminator is seen.
/// Returns Ok(true) on a complete header, Ok(false) on EOF/close.
fn read_request(stream: &mut TcpStream) -> std::io::Result<bool> {
    let mut buf = [0u8; 4096];
    let term = [b'\r', b'\n', b'\r', b'\n'];
    let mut matched = 0usize;
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(false);
        }
        for &b in &buf[..n] {
            if b == term[matched] {
                matched += 1;
                if matched == 4 {
                    return Ok(true);
                }
            } else {
                matched = if b == b'\r' { 1 } else { 0 };
            }
        }
    }
}

fn handle_conn(mut stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    loop {
        match read_request(&mut stream) {
            Ok(true) => {}
            _ => break, // EOF, close, or read error: drop this connection
        }

        let ts = now_iso8601();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
            ts.len(),
            ts
        );
        if stream.write_all(resp.as_bytes()).is_err() {
            break;
        }
    }
}

fn main() {
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(18080);

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .unwrap_or_else(|e| panic!("bind 127.0.0.1:{port}: {e}"));

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                thread::spawn(move || handle_conn(stream));
            }
            Err(_) => continue, // transient accept error: keep serving
        }
    }
}
