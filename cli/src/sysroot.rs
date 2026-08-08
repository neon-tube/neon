use crate::buildcfg::{CcFlavor, RuntimeVariant};
use color_eyre::eyre::{bail, eyre, Result};
use std::path::{Path, PathBuf};

/// Locates `include/`, the `lib/<flavor>/libneon_rt*.a` archives and `stdlib/`.
///
/// Resolved at runtime, never baked in: a compile-time path describes the
/// machine that built the compiler, not the one running it.
pub struct Sysroot(PathBuf);

/// The archive flavors a sysroot may carry, in the order they are probed and reported.
const FLAVOR_DIRS: &[&str] = &["gcc", "clang"];

/// A runtime archive picked for a link, and the warning to show when the pick settled
/// for a fallback flavor (see `Sysroot::runtime_lib`).
pub struct ResolvedRuntime {
    pub path: PathBuf,
    pub note: Option<String>,
}

impl Sysroot {
    fn probe(dir: PathBuf) -> Option<Self> {
        // Any one flavor's release archive marks a sysroot: which flavors exist depends
        // on the compilers present when the toolchain was built, and needing a specific
        // one is a *link-time* question (`runtime_lib`), not an existence question.
        FLAVOR_DIRS
            .iter()
            .any(|f| dir.join("lib").join(f).join("libneon_rt.a").is_file())
            .then_some(Sysroot(dir))
    }

    pub fn find() -> Result<Self> {
        if let Some(dir) = std::env::var_os("NEON_SYSROOT") {
            let dir = PathBuf::from(dir);
            return Self::probe(dir.clone()).ok_or_else(|| {
                eyre!(
                    "NEON_SYSROOT is set to '{}' but there is no lib/<flavor>/libneon_rt.a there \
                     (flavors: gcc, clang)",
                    dir.display()
                )
            });
        }

        let exe =
            std::env::current_exe().map_err(|e| eyre!("cannot locate the neon binary: {e}"))?;
        let exe_dir = exe
            .parent()
            .ok_or_else(|| eyre!("the neon binary has no parent directory"))?;

        // exe_dir: dev (target/<profile>). exe_dir/..: installed (prefix/bin).
        let candidates = [exe_dir.to_path_buf(), exe_dir.join("..")];
        for dir in &candidates {
            if let Some(found) = Self::probe(dir.clone()) {
                return Ok(found);
            }
        }

        bail!(
            "cannot find the Neon sysroot: no lib/<flavor>/libneon_rt.a under {}.\n\
             Set NEON_SYSROOT to override.",
            candidates
                .iter()
                .map(|p| format!("'{}'", p.display()))
                .collect::<Vec<_>>()
                .join(" or ")
        )
    }

    /// The stdlib directory alone, for front-end runs that need no runtime.
    ///
    /// Probed independently of the runtime archives: type-checking needs only the
    /// stdlib source, and the runtime archive does not exist until the backend does,
    /// so requiring it here would make `neon check` unusable before codegen lands.
    ///
    /// Must accept the same two layouts as `find`: beside the binary
    /// (`exe_dir/stdlib`, e.g. `target/release`) and installed (`exe_dir/../stdlib`,
    /// `prefix/bin/neon` → `prefix/stdlib`). Probing rather than assuming one — the old
    /// code hard-coded the installed layout, so a beside-the-binary install that `find`
    /// happily located still failed every compile at `stdlib_dir`.
    pub fn stdlib_dir() -> Result<PathBuf> {
        if let Some(dir) = std::env::var_os("NEON_SYSROOT") {
            // Verified here rather than left to the caller: a path that does not exist
            // reaches the front end as "every stdlib name is unknown", which reads as a
            // broken program instead of a misconfigured override.
            let dir = PathBuf::from(dir).join("stdlib");
            if !dir.is_dir() {
                bail!(
                    "NEON_SYSROOT is set, but there is no stdlib/ at '{}'",
                    dir.display()
                );
            }
            return Ok(dir);
        }
        let exe =
            std::env::current_exe().map_err(|e| eyre!("cannot locate the neon binary: {e}"))?;
        let exe_dir = exe
            .parent()
            .ok_or_else(|| eyre!("the neon binary has no parent directory"))?;
        if let Some(found) = stdlib_beside(exe_dir) {
            return Ok(found);
        }
        bail!(
            "cannot find the Neon stdlib: no stdlib/ under {}.\n\
             Set NEON_SYSROOT to override.",
            stdlib_candidates(exe_dir)
                .iter()
                .map(|p| format!("'{}'", p.display()))
                .collect::<Vec<_>>()
                .join(" or ")
        )
    }

    pub fn root(&self) -> &PathBuf {
        &self.0
    }

    pub fn include(&self) -> PathBuf {
        self.0.join("include")
    }

    /// Where the prebuilt runtime archives live: one subdirectory per compiler flavor,
    /// each holding the three variants. `lib/<flavor>/libneon_rt.a` doubles as the
    /// marker `probe` looks for: it is the variant that always exists.
    pub fn lib_dir(&self) -> PathBuf {
        self.0.join("lib")
    }

    /// The flavor subdirectories actually present, for diagnostics.
    pub fn flavors_present(&self) -> Vec<&'static str> {
        FLAVOR_DIRS
            .iter()
            .copied()
            .filter(|f| self.lib_dir().join(f).join("libneon_rt.a").is_file())
            .collect()
    }

    /// The prebuilt archive for `variant`, preferring `flavor`'s compiler family, plus a
    /// warning when the build had to settle.
    ///
    /// A missing *variant* is never swapped for another variant — a sanitizer reports
    /// nothing about code compiled without it, so a silently downgraded runtime is a lie
    /// about what was built. A missing *flavor* splits by what the substitution would
    /// actually do:
    ///
    ///   - `Release`/`Debug`: the other flavor's archive links correctly — fat objects
    ///     carry real machine code — but the cross-family link cannot read the LTO
    ///     bitcode, so every runtime call stays un-inlinable (measured at 4× on a hot
    ///     loop). Allowed, with a warning that says exactly that, because a slower
    ///     correct build beats a refusal when the right compiler simply was not there
    ///     when the toolchain was built.
    ///   - `Sanitized`: refused. gcc's libasan and clang's compiler-rt are different
    ///     runtimes; one family's instrumented archive does not link under the other's
    ///     driver, so the fallback would not be a slower build — it would be a broken
    ///     link or worse.
    pub fn runtime_lib(
        &self,
        variant: RuntimeVariant,
        flavor: CcFlavor,
    ) -> Result<ResolvedRuntime> {
        let dir = self.lib_dir().join(flavor.dir());
        let path = dir.join(variant.archive());
        if path.is_file() {
            return Ok(ResolvedRuntime { path, note: None });
        }

        // The flavor was staged but this variant is missing: an incomplete toolchain,
        // and no substitution of any kind.
        if dir.join("libneon_rt.a").is_file() {
            let present: Vec<String> = [
                RuntimeVariant::Release,
                RuntimeVariant::Debug,
                RuntimeVariant::Sanitized,
            ]
            .iter()
            .map(|v| v.archive())
            .filter(|a| dir.join(a).is_file())
            .map(str::to_string)
            .collect();
            bail!(
                "this build needs the runtime archive `{}/{}`, which is not in the sysroot at {}.\n\
                 Present there: {}.\n\
                 The toolchain's runtime is incomplete; rebuild or reinstall it. Another \
                 variant will not be substituted — it would change what the build actually \
                 links without saying so.",
                flavor.dir(),
                variant.archive(),
                self.0.display(),
                if present.is_empty() { "nothing".into() } else { present.join(", ") },
            )
        }

        // The whole flavor is missing: the toolchain was built on a machine without that
        // compiler family.
        let fallback = self
            .flavors_present()
            .into_iter()
            .find(|f| self.lib_dir().join(f).join(variant.archive()).is_file());
        let Some(other) = fallback else {
            bail!(
                "the sysroot at {} has no `{}` runtime archives and no other flavor \
                 carrying `{}` to fall back on. Rebuild or reinstall the toolchain.",
                self.0.display(),
                flavor.dir(),
                variant.archive(),
            )
        };

        if variant == RuntimeVariant::Sanitized {
            bail!(
                "this build's C compiler is {} but the sysroot at {} carries no {} runtime \
                 archives (present: {other}).\n\
                 The {other} sanitized archive cannot stand in: gcc's libasan and clang's \
                 compiler-rt are different sanitizer runtimes, and one family's \
                 instrumented archive does not link under the other's driver. Use a {} \
                 compiler via `--cc`/`$CC`, or rebuild the toolchain with {} installed.",
                flavor.dir(),
                self.0.display(),
                flavor.dir(),
                other,
                flavor.dir(),
            )
        }

        // A cross-family link reads the fallback archive's machine code, not its LTO
        // bitcode — so there has to *be* machine code. An archive built with a plain
        // `-flto` and no fat objects carries bitcode and nothing else, and the other
        // family's linker does not recognise a bitcode member as an object at all. That
        // shape ships: Apple Clang rejects `-ffat-lto-objects`, so the macOS archives are
        // exactly it (`runtime/CMakeLists.txt`), and macOS stages no gcc flavor at all
        // (`runtime/build.rs`) — a mac user building with real gcc lands here. Say so,
        // rather than letting `file format not recognized` come out of ld.
        let path = self.lib_dir().join(other).join(variant.archive());
        if let Some(contents) = inspect_archive(&path) {
            if !contents.native_code {
                bail!(
                    "this build's C compiler is {} but the sysroot at {} carries only {other} \
                     runtime archives, and `{other}/{}` holds LTO bitcode with no machine code \
                     in it — a {} link cannot read it at all, so it cannot stand in.\n\
                     Build with a {other} `cc` (`--cc`/`$CC`), or rebuild the toolchain on a \
                     machine with {} installed so its own archives are staged.",
                    flavor.dir(),
                    self.0.display(),
                    variant.archive(),
                    flavor.dir(),
                    flavor.dir(),
                )
            }
        }

        Ok(ResolvedRuntime {
            path,
            note: Some(format!(
                "warning: this build's C compiler is {} but the toolchain carries no {} \
                 runtime archives; linking the {other} one instead. It links correctly — \
                 the archive's machine code is checked for above — but a cross-family link \
                 cannot read the archive's LTO bitcode, so runtime primitives stay \
                 un-inlinable in hot code. For full speed, rebuild the toolchain with {} \
                 installed or switch `--cc`/`$CC` to {other}.",
                flavor.dir(),
                flavor.dir(),
                flavor.dir(),
            )),
        })
    }

    pub fn stdlib(&self) -> PathBuf {
        self.0.join("stdlib")
    }
}

/// The layouts `stdlib_dir` accepts, in probe order: beside the binary (`target/release`,
/// a dev tree) and installed (`prefix/bin/neon` → `prefix/stdlib`). The same two `find`
/// probes, and they must stay the same two — a layout `find` locates but `stdlib_dir`
/// rejects is an install that fails every compile.
fn stdlib_candidates(exe_dir: &Path) -> [PathBuf; 2] {
    [exe_dir.join("stdlib"), exe_dir.join("../stdlib")]
}

fn stdlib_beside(exe_dir: &Path) -> Option<PathBuf> {
    stdlib_candidates(exe_dir).into_iter().find(|d| d.is_dir())
}

/// What a runtime archive's members carry, as far as a link cares: machine code, LTO
/// material one family can read, LTO material the other can.
///
/// Two questions ride on this, and they are not the same question:
///   * *Can this `cc` link the archive at all?* Only machine code answers yes for a
///     cross-family link — a bitcode-only archive is not an object file to the other
///     family's linker (`runtime_lib`).
///   * *Will this `cc` inline through it?* Only that family's own LTO material answers
///     yes, and a build that passes `-flto` without it is a ~2.5x regression on tight
///     code with no other symptom (`emit::link` warns).
///
/// Bitcode is checked per archive *member* rather than by scanning the whole file: a
/// four-byte magic looked for anywhere in a multi-megabyte archive hits by chance, and
/// "the members are bitcode" is a different claim from "these bytes appear somewhere".
#[derive(Debug, PartialEq, Eq)]
pub struct ArchiveContents {
    /// Real objects a linker of any family can consume: ELF or Mach-O members.
    pub native_code: bool,
    /// LLVM bitcode a clang `cc` can inline through: bitcode members, or ELF fat objects
    /// carrying clang's `.llvm.lto`/`.llvmbc` section.
    pub llvm_bitcode: bool,
    /// GCC LTO a gcc `cc` can inline through: fat objects whose section names carry
    /// `.gnu.lto_`.
    pub gcc_lto: bool,
}

impl ArchiveContents {
    /// Whether a `cc` of this flavor can inline through the archive. LTO material does not
    /// cross families: gcc cannot read LLVM bitcode and clang cannot read `.gnu.lto_`.
    pub fn lto_for(&self, flavor: CcFlavor) -> bool {
        match flavor {
            CcFlavor::Clang => self.llvm_bitcode,
            CcFlavor::Gcc => self.gcc_lto,
        }
    }
}

/// Inspect a runtime archive, or `None` when it cannot be read or is not an `ar` archive.
///
/// `None` means *unknown*, and every caller stays permissive on it: a diagnostic must
/// never be the thing that breaks a build that would otherwise have worked.
///
/// A byte inspection rather than shelling out to `ar`/`nm`/`llvm-bcanalyzer`: it needs no
/// tools present on the user's machine, and the markers are unambiguous.
pub fn inspect_archive(path: &std::path::Path) -> Option<ArchiveContents> {
    inspect_archive_bytes(&std::fs::read(path).ok()?)
}

/// LLVM bitcode wrapper magic `0x0B17C0DE`, little-endian on disk.
const LLVM_WRAPPER: &[u8] = &[0xDE, 0xC0, 0x17, 0x0B];
/// Raw LLVM bitcode magic, `BC\xC0\xDE`.
const LLVM_RAW: &[u8] = &[0x42, 0x43, 0xC0, 0xDE];

fn inspect_archive_bytes(bytes: &[u8]) -> Option<ArchiveContents> {
    let mut out = ArchiveContents {
        native_code: false,
        llvm_bitcode: false,
        gcc_lto: false,
    };
    for member in ar_members(bytes)? {
        if member.starts_with(LLVM_WRAPPER) || member.starts_with(LLVM_RAW) {
            out.llvm_bitcode = true;
            continue;
        }
        if !starts_with_object_magic(member) {
            continue;
        }
        out.native_code = true;
        // A fat object: machine code *and* bitcode, the shape `-ffat-lto-objects` emits.
        // Which family's LTO it carries is in the section names.
        if contains(member, b".gnu.lto_") {
            out.gcc_lto = true;
        }
        if contains(member, b".llvm.lto") || contains(member, b".llvmbc") {
            out.llvm_bitcode = true;
        }
    }
    Some(out)
}

/// ELF, or Mach-O in any of its four magics (32/64-bit × either endianness).
fn starts_with_object_magic(member: &[u8]) -> bool {
    const MACH_O: [&[u8]; 4] = [
        &[0xFE, 0xED, 0xFA, 0xCE],
        &[0xCE, 0xFA, 0xED, 0xFE],
        &[0xFE, 0xED, 0xFA, 0xCF],
        &[0xCF, 0xFA, 0xED, 0xFE],
    ];
    member.starts_with(b"\x7FELF") || MACH_O.iter().any(|m| member.starts_with(m))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The data of each real member of an `ar` archive, or `None` if this is not one (or is
/// malformed — an archive we cannot parse is one we decline to draw conclusions from).
///
/// Both name conventions are handled, because both ship: GNU keeps long names in a `//`
/// member and refers to them as `/<offset>`, BSD (macOS) prefixes the member data with the
/// name and writes `#1/<len>` in the header. The symbol-table and string-table members
/// carry no object code and are skipped.
fn ar_members(bytes: &[u8]) -> Option<Vec<&[u8]>> {
    const HEADER: usize = 60;
    bytes.strip_prefix(b"!<arch>\n")?;
    let mut pos = 8;
    let mut out = Vec::new();
    while pos + HEADER <= bytes.len() {
        let header = &bytes[pos..pos + HEADER];
        if &header[58..60] != b"`\n" {
            return None;
        }
        let name = std::str::from_utf8(&header[0..16]).ok()?.trim_end();
        let size: usize = std::str::from_utf8(&header[48..58])
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let start = pos + HEADER;
        let data = bytes.get(start..start.checked_add(size)?)?;
        // Members are padded to an even offset; the pad byte is not part of the data.
        pos = start + size + (size & 1);

        // BSD long name: the first `n` bytes of the data are the name, not content.
        let (name, data) = match name.strip_prefix("#1/").map(str::parse::<usize>) {
            Some(Ok(n)) => {
                let embedded = std::str::from_utf8(data.get(..n)?).ok()?;
                (embedded.trim_end_matches('\0'), &data[n..])
            }
            Some(Err(_)) => return None,
            None => (name, data),
        };
        // Symbol table (`/`, `__.SYMDEF`), long-name table (`//`), 64-bit symbol table.
        let is_special = matches!(name, "/" | "//" | "/SYM64/") || name.starts_with("__.SYMDEF");
        if !is_special {
            out.push(data);
        }
    }
    // Trailing bytes too short to be a header: truncated, so what was parsed is not the
    // whole archive and "no LTO material here" would be a conclusion drawn from a fragment.
    (pos == bytes.len()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A member header: name, mtime, uid, gid, mode, size, and the `` `\n `` terminator,
    /// in the fixed widths `ar` uses.
    fn header(name: &str, size: usize) -> String {
        format!(
            "{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n",
            0, 0, 0, "644"
        )
    }

    /// Build an `ar` archive with GNU-style short names around the given member bodies.
    fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = b"!<arch>\n".to_vec();
        for (name, body) in members {
            out.extend_from_slice(header(&format!("{name}/"), body.len()).as_bytes());
            out.extend_from_slice(body);
            if body.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    fn elf(sections: &[u8]) -> Vec<u8> {
        let mut o = b"\x7FELF".to_vec();
        o.extend_from_slice(sections);
        o
    }

    #[test]
    fn bitcode_only_archive_is_llvm_lto_and_carries_no_machine_code() {
        let a = archive(&[("list.o", &[LLVM_RAW, b"body".as_slice()].concat())]);
        let c = inspect_archive_bytes(&a).expect("parses");
        assert_eq!(
            c,
            ArchiveContents {
                native_code: false,
                llvm_bitcode: true,
                gcc_lto: false
            }
        );
        assert!(c.lto_for(CcFlavor::Clang) && !c.lto_for(CcFlavor::Gcc));
    }

    #[test]
    fn gcc_fat_objects_carry_both_machine_code_and_gcc_lto() {
        let a = archive(&[("list.o", &elf(b"....gnu.lto_.symtab...."))]);
        let c = inspect_archive_bytes(&a).expect("parses");
        assert_eq!(
            c,
            ArchiveContents {
                native_code: true,
                llvm_bitcode: false,
                gcc_lto: true
            }
        );
        // The pair that used to be conflated: a gcc archive is linkable by clang, but
        // clang cannot inline through gcc's LTO.
        assert!(c.lto_for(CcFlavor::Gcc) && !c.lto_for(CcFlavor::Clang));
    }

    #[test]
    fn plain_objects_carry_no_lto_for_either_family() {
        let a = archive(&[
            ("list.o", &elf(b"....text....")),
            ("gc.o", &elf(b"....data....")),
        ]);
        let c = inspect_archive_bytes(&a).expect("parses");
        assert_eq!(
            c,
            ArchiveContents {
                native_code: true,
                llvm_bitcode: false,
                gcc_lto: false
            }
        );
        assert!(!c.lto_for(CcFlavor::Gcc) && !c.lto_for(CcFlavor::Clang));
    }

    /// The symbol table names every symbol in the archive, so a whole-file scan reads its
    /// contents as if they were object code. Members-only does not.
    #[test]
    fn the_symbol_table_is_not_mistaken_for_a_member() {
        let a = archive(&[
            ("", b"\x7FELF fake symtab .gnu.lto_"),
            ("list.o", &elf(b"..text..")),
        ]);
        let c = inspect_archive_bytes(&a).expect("parses");
        assert!(!c.gcc_lto, "the `/` symbol table is skipped");
    }

    #[test]
    fn bsd_long_names_are_stripped_from_the_member_body() {
        let mut a = b"!<arch>\n".to_vec();
        let name = "neon_rt_list.c.o\0\0\0\0"; // 20 bytes, `#1/20`
        let body = [LLVM_RAW, b"body".as_slice()].concat();
        a.extend_from_slice(header("#1/20", name.len() + body.len()).as_bytes());
        a.extend_from_slice(name.as_bytes());
        a.extend_from_slice(&body);
        let c = inspect_archive_bytes(&a).expect("parses");
        assert!(c.llvm_bitcode, "the magic is found after the embedded name");
    }

    #[test]
    fn a_file_that_is_not_an_archive_is_unknown_rather_than_empty() {
        assert!(inspect_archive_bytes(b"\x7FELF not an archive").is_none());
        assert!(inspect_archive_bytes(b"!<arch>\ntruncated").is_none());
    }

    #[test]
    fn stdlib_is_found_beside_the_binary_and_one_level_up() {
        let root = std::env::temp_dir().join(format!("neon-stdlib-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let beside = root.join("target/release");
        let installed = root.join("prefix/bin");
        std::fs::create_dir_all(beside.join("stdlib")).expect("mkdir");
        std::fs::create_dir_all(root.join("prefix/stdlib")).expect("mkdir");
        std::fs::create_dir_all(&installed).expect("mkdir");

        assert_eq!(stdlib_beside(&beside), Some(beside.join("stdlib")));
        assert_eq!(stdlib_beside(&installed), Some(installed.join("../stdlib")));
        assert_eq!(stdlib_beside(&root.join("nowhere")), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
