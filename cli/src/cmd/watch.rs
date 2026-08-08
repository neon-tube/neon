//! `--watch`: re-run a verb every time the watched sources change.
//!
//! Each iteration spawns THIS binary again with the same arguments minus `--watch`,
//! rather than calling the verb's function in a loop. That is not laziness, it is the
//! only correct shape: the front end renders diagnostics and calls `exit(1)`, which is
//! right for a build and fatal for a watcher — and a child process also resets every
//! bit of state a failed compile might leave behind. The watcher itself never compiles
//! anything.
//!
//! Change detection is mtime polling at 250ms over the watched set, dependency-free.
//! An inotify crate would wake faster and cost a dependency tree; a quarter-second
//! after save is beneath a human's noticing next to the compile it triggers.

use color_eyre::eyre::Result;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

pub fn rerun_on_change(watched: Vec<PathBuf>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let args: Vec<OsString> = std::env::args_os()
        .skip(1)
        .filter(|a| a != "--watch")
        .collect();

    let mut last = snapshot(&watched);
    loop {
        let started = std::time::Instant::now();
        let status = Command::new(&exe).args(&args).status();
        let took = started.elapsed();
        match status {
            Ok(s) if s.success() => {
                eprintln!(
                    "\n[watch] ok in {:.1}s — waiting for changes",
                    took.as_secs_f32()
                )
            }
            Ok(s) => eprintln!(
                "\n[watch] exit {} in {:.1}s — waiting for changes",
                s.code().unwrap_or(-1),
                took.as_secs_f32()
            ),
            Err(e) => eprintln!("\n[watch] could not run: {e} — waiting for changes"),
        }
        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let now = snapshot(&watched);
            if now != last {
                last = now;
                break;
            }
        }
        eprintln!("[watch] change detected\n");
    }
}

/// Every watched file's mtime: a named file directly, a directory as its `.neon` files
/// and `neon.toml`, recursively. A path that stops existing simply leaves the map,
/// which reads as a change — deleting a file should re-run too.
fn snapshot(watched: &[PathBuf]) -> HashMap<PathBuf, SystemTime> {
    let mut out = HashMap::new();
    for p in watched {
        collect(p, &mut out);
    }
    out
}

fn collect(p: &Path, out: &mut HashMap<PathBuf, SystemTime>) {
    if p.is_dir() {
        let Ok(entries) = std::fs::read_dir(p) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                collect(&path, out);
            } else if path.extension().is_some_and(|x| x == "neon")
                || path.file_name().is_some_and(|n| n == "neon.toml")
            {
                insert_mtime(&path, out);
            }
        }
        return;
    }
    insert_mtime(p, out);
}

fn insert_mtime(p: &Path, out: &mut HashMap<PathBuf, SystemTime>) {
    if let Ok(meta) = std::fs::metadata(p) {
        if let Ok(t) = meta.modified() {
            out.insert(p.to_path_buf(), t);
        }
    }
}

/// What a `run`/`test` invocation should watch: the named `.neon` file, or the
/// project's sources when the target is a directory or absent.
pub fn watch_set(target: Option<&OsString>) -> Vec<PathBuf> {
    match target {
        Some(t) => {
            let p = PathBuf::from(t);
            if p.is_dir() {
                project_set(&p)
            } else {
                vec![p]
            }
        }
        None => project_set(Path::new(".")),
    }
}

fn project_set(root: &Path) -> Vec<PathBuf> {
    let mut v = vec![root.join("neon.toml")];
    let src = root.join("src");
    v.push(if src.is_dir() {
        src
    } else {
        root.to_path_buf()
    });
    v
}
