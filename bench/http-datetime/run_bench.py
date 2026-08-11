#!/usr/bin/env python3
# /// script
# dependencies = ["rich"]
# ///
"""http-datetime benchmark: build each language's server, hit it, report requests/second.

Every server reads $PORT (default 18080) and answers any request with the current time in
ISO 8601. The runner picks a free port, starts one server at a time, checks it returns a
parseable timestamp, then load-tests it with a pool of keep-alive connections."""
import argparse, http.client, os, re, shutil, socket, subprocess, sys, threading, time
from rich.console import Console
from rich.table import Table

console = Console()
HERE = os.path.dirname(os.path.abspath(__file__))
os.chdir(HERE)
ISO = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}")

# name -> (build cmd or None, build cwd, run cmd, toolchain probe)
LANGS = [
    ("C",          ["gcc", "-O3", "-pthread", "-o", "c/httpdt", "c/main.c"], None, ["./c/httpdt"], "gcc"),
    ("C++",        ["g++", "-O3", "-std=c++17", "-pthread", "-o", "cpp/httpdt", "cpp/main.cpp"], None, ["./cpp/httpdt"], "g++"),
    ("Rust",       ["cargo", "build", "--release", "--manifest-path", "rust/Cargo.toml"], None, ["./rust/target/release/httpdt"], "cargo"),
    ("Zig",        ["zig", "build-exe", "-O", "ReleaseFast", "zig/main.zig", "-femit-bin=zig/httpdt"], None, ["./zig/httpdt"], "zig"),
    ("Go",         ["go", "build", "-o", "go/httpdt", "go/main.go"], None, ["./go/httpdt"], "go"),
    ("Neon",       ["cargo", "run", "--release", "--manifest-path", "../../../Cargo.toml", "--bin", "neon", "--", "build"], "neon", ["./neon/_neon/httpdt"], "cargo"),
    ("Dart",       ["dart", "compile", "exe", "dart/main.dart", "-o", "dart/httpdt"], None, ["./dart/httpdt"], "dart"),
    ("C# (.NET)",  ["dotnet", "publish", "dotnet/httpdt.csproj", "-c", "Release", "-o", "dotnet/build"], None, ["./dotnet/build/httpdt"], "dotnet"),
    ("Java",       ["javac", "-d", "java", "java/Main.java"], None, ["java", "-cp", "java", "Main"], "javac"),
    ("Haskell",    ["ghc", "-O2", "-outputdir", "haskell/out", "-o", "haskell/httpdt", "haskell/Main.hs"], None, ["./haskell/httpdt"], "ghc"),
    ("OCaml",      ["ocamlfind", "ocamlopt", "-package", "unix", "-linkpkg", "ocaml/main.ml", "-o", "ocaml/httpdt"], None, ["./ocaml/httpdt"], "ocamlfind"),
    ("Python",     None, None, ["python3", "python/main.py"], "python3"),
    ("JS (Node)",  None, None, ["node", "js/main.js"], "node"),
    ("JS (Bun)",   None, None, ["bun", "js/main.js"], "bun"),
    ("TS (Bun)",   None, None, ["bun", "ts/main.ts"], "bun"),
    ("TS (Deno)",  None, None, ["deno", "run", "-A", "ts/main.ts"], "deno"),
    ("Ruby",       None, None, ["ruby", "ruby/main.rb"], "ruby"),
    ("Elixir",     None, None, ["elixir", "elixir/main.exs"], "elixir"),
    ("Clojure",    None, None, ["clojure", "-M", "clojure/main.clj"], "clojure"),
    ("Lua",        None, None, ["lua", "lua/main.lua"], "lua"),
    ("Perl",       None, None, ["perl", "perl/main.pl"], "perl"),
]

def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p

def wait_ready(port, timeout=15.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.05)
    return False

def one_request(port):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    c.request("GET", "/")
    body = c.getresponse().read().decode(errors="replace").strip()
    c.close()
    return body

def rewrk_load(port, conns, threads, duration):
    """The real load: rewrk, keep-alive, N connections over T threads. Returns req/s."""
    res = subprocess.run(
        ["rewrk", "-c", str(conns), "-t", str(threads), "-d", f"{int(duration)}s",
         "-h", f"http://127.0.0.1:{port}"],
        capture_output=True, text=True)
    m = re.search(r"Req/Sec:\s*([\d.]+)", res.stdout)
    return float(m.group(1)) if m else None

def python_load(port, duration, workers):
    """Fallback when rewrk is absent: a rough keep-alive number, marked as such."""
    stop = time.time() + duration
    counts = [0] * workers
    def worker(i):
        conn = None
        while time.time() < stop:
            try:
                if conn is None:
                    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                conn.request("GET", "/")
                r = conn.getresponse(); r.read()
                if r.status == 200:
                    counts[i] += 1
                if r.will_close:
                    conn.close(); conn = None
            except Exception:
                if conn: conn.close()
                conn = None
        if conn: conn.close()
    ts = [threading.Thread(target=worker, args=(i,)) for i in range(workers)]
    t0 = time.time()
    for t in ts: t.start()
    for t in ts: t.join()
    return sum(counts) / (time.time() - t0)

def build(name, cmd, cwd, hide):
    if cmd is None:
        return True
    if not hide:
        console.print(f"Compiling [cyan]{name}[/cyan]...")
    res = subprocess.run(cmd, capture_output=True, text=True, cwd=cwd)
    ok = res.returncode == 0
    if not ok:
        console.print(f"Compiling [cyan]{name}[/cyan]... [red]failed[/red]")
        console.print((res.stdout + res.stderr).strip()[:1500])
    elif hide:
        console.print(f"Compiling [cyan]{name}[/cyan]... [green]ok[/green]")
    return ok

def main():
    ap = argparse.ArgumentParser(description="Benchmark the http-datetime servers.")
    ap.add_argument("--only", type=str, help="comma-separated language substrings")
    ap.add_argument("-c", "--connections", type=int, default=64)
    ap.add_argument("-t", "--threads", type=int, default=8)
    ap.add_argument("-d", "--duration", type=float, default=10.0)
    ap.add_argument("--hide-compilation", action="store_true")
    args = ap.parse_args()

    have_rewrk = shutil.which("rewrk") is not None
    if not have_rewrk:
        console.print("[yellow]rewrk not found; falling back to a rough built-in load (install rewrk for real numbers).[/yellow]")

    want = [s.strip().lower() for s in args.only.split(",")] if args.only else None
    def selected(name):
        return want is None or any(w in name.lower() for w in want)

    port = free_port()
    env = dict(os.environ, PORT=str(port))
    results = {}

    for name, bcmd, bcwd, rcmd, probe in LANGS:
        if not selected(name):
            continue
        if shutil.which(probe) is None:
            console.print(f"[yellow]{probe} not found; skipping {name}.[/yellow]")
            results[name] = None
            continue
        if not build(name, bcmd, bcwd, args.hide_compilation):
            results[name] = None
            continue

        console.print(f"Benchmarking [cyan]{name}[/cyan]...")
        proc = subprocess.Popen(rcmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            if not wait_ready(port):
                console.print(f"[red]{name} never started listening.[/red]")
                results[name] = None
                continue
            body = one_request(port)
            if not ISO.match(body):
                console.print(f"[red]{name} did not return an ISO timestamp: {body!r}[/red]")
                results[name] = None
                continue
            if have_rewrk:
                results[name] = rewrk_load(port, args.connections, args.threads, args.duration)
            else:
                results[name] = python_load(port, args.duration, args.threads)
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
            time.sleep(0.2)

    table = Table(title="http-datetime — requests/second (higher is better)")
    table.add_column("Language", style="cyan")
    table.add_column("req/s", justify="right", style="green")
    table.add_column("vs fastest", justify="right", style="magenta")
    ok = {k: v for k, v in results.items() if v}
    best = max(ok.values()) if ok else 0
    for name in sorted(results, key=lambda n: results[n] or -1, reverse=True):
        v = results[name]
        if v:
            table.add_row(name, f"{v:,.0f}", f"{best / v:.2f}x")
        else:
            table.add_row(name, "[red]—[/red]", "")
    console.print(table)

if __name__ == "__main__":
    main()
