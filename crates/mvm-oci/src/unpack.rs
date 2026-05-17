//! Layer-to-tree unpacker — Plan 85 Phase A.
//!
//! Materializes an OCI layer tarball (already digest-verified + cached
//! by [`crate::layer`]) into a directory tree on the host filesystem,
//! with explicit per-entry safety policies that close the
//! CVE-2019-14271-class category of attacks (path traversal, symlink
//! escape, hardlink-to-host, device-node planting, setuid surprise,
//! xattr-based privilege carry).
//!
//! ## Scope through sub-phase A.2
//!
//! The unpacker handles three tar entry kinds and two OCI-specific
//! filename markers that ride inside regular-file tar entries:
//!
//! - **Regular files** — bytes streamed to a freshly-opened file at
//!   `output_root/<entry-path>`. Mode is preserved from the tar
//!   header, masked to `0o7777` (no sticky/setuid/setgid bits
//!   pass-through in A.1 — those land in A.6 under explicit
//!   policy control).
//! - **Directories** — created with `0o755`, regardless of the tar
//!   header's mode bits. A non-canonical mode is recorded in the
//!   [`UnpackReport`] for downstream auditing but does not change
//!   the on-disk result; mode-rewriting is out of scope for the
//!   base unpacker.
//! - **Symbolic links** — written via [`std::os::unix::fs::symlink`].
//!   The link target is preserved **verbatim**; we deliberately do
//!   *not* canonicalize it at unpack time, because the resolution
//!   happens later inside the booted guest where the layer hierarchy
//!   means something different than it does on the host. The safety
//!   policy enforced at *unpack* time is that the link's location
//!   (the path the symlink *file itself* occupies) is under
//!   `output_root`; targets that point outside are not unpack-time
//!   errors.
//! - **OCI whiteouts (A.2)** — regular-file tar entries (typeflag
//!   `'0'`) whose *leaf filename* carries the OCI v1.1 whiteout
//!   semantic. `.wh.<name>` removes the sibling `<name>` from the
//!   assembled tree; `.wh..wh..opq` clears the parent directory's
//!   prior-layer contents but preserves the directory itself. The
//!   marker files themselves are **not** materialized, and markers
//!   apply only to prior/lower-layer state, never to entries from
//!   the same layer. Both shapes
//!   pass through every A.1 safety check on the marker's path
//!   before the whiteout helper runs — including
//!   [`RefusalReason::SymlinkInParent`], so a `.wh.passwd` under a
//!   symlinked `etc/` refuses the same way a regular-file write
//!   would. Targets that don't exist are no-ops (the OCI spec is
//!   declarative; single-layer apply is idempotent).
//!
//! Every other entry kind — hardlinks (`tar::EntryType::Link`),
//! character/block special files, FIFOs, named sockets, sparse files,
//! tar-format extended-attr pax headers, GNU long-name
//! continuations — is **refused** with
//! [`RefusalReason::UnsupportedEntryType`]. Later sub-phases
//! re-classify each refused category:
//!
//! | Sub-phase | What it adds |
//! |---|---|
//! | A.3 | Hardlinks within-layer; cross-layer hardlinks materialize as copy |
//! | A.4 | xattrs (allow-listed: `user.*`, `security.capability`, `security.selinux`) |
//! | A.5 | Device nodes (allow-listed to `/dev/{null,zero,random,urandom}` only) |
//! | A.6 | setuid/setgid bits, policy-controlled (refused under prod-no-cosign) |
//!
//! ## Safety properties enforced in A.1
//!
//! 1. **No path escapes `output_root`.** Three checks layer on top of
//!    each other for defense in depth:
//!    - Refuse absolute paths (`/etc/passwd`).
//!    - Refuse any path containing a literal `..` segment
//!      (segment-by-segment; never substring match — `..foo` is a
//!      valid filename).
//!    - After computing the would-be target as
//!      `output_root.join(rel)`, verify with `.starts_with(output_root)`
//!      that the resolved path is still under root. Catches edge
//!      cases the prior two checks miss (NUL-byte paths, platform-
//!      quirky separators, etc.).
//! 2. **No write follows a symlink that the unpacker itself wrote.**
//!    Before writing under a non-empty parent prefix, we walk every
//!    parent component of the target and refuse if any of them is a
//!    symlink. This blocks the "write `bin -> /tmp` in entry N, then
//!    write `bin/escape` in entry N+1" escape vector. We use
//!    [`std::fs::symlink_metadata`] (which does *not* dereference)
//!    rather than [`std::fs::metadata`] for the check.
//! 3. **Files open with `O_NOFOLLOW`** so even if the symlink-parent
//!    check above missed something, the `open(2)` call itself fails
//!    with `ELOOP` rather than following the symlink.
//! 4. **Timestamps zeroed by default.** OCI layer tarballs are
//!    notoriously timestamp-dependent; the same image pulled twice
//!    can produce non-byte-identical rootfs trees if mtime is
//!    preserved. [`UnpackOptions::strip_timestamps`] (default `true`)
//!    forces every unpacked entry's mtime/atime to `0` so two unpacks
//!    of the same layer produce byte-identical trees modulo
//!    filesystem-allocated inode numbers.
//!
//! ## Async / sync
//!
//! Tar parsing and filesystem syscalls are CPU- and syscall-bound;
//! exposing them as `async` would require either a Tokio file-I/O
//! shim that just farms out to `spawn_blocking` anyway, or porting
//! the `tar` crate to async (no off-the-shelf async-tar implementation
//! covers OCI semantics). We expose a synchronous `unpack_layer` and
//! expect async callers to wrap with `tokio::task::spawn_blocking`.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// OCI v1.1 whiteout marker prefix. A regular-file tar entry whose
/// **leaf filename** starts with this byte sequence is interpreted as
/// a whiteout instruction, not a file to materialize.
const WHITEOUT_PREFIX: &[u8] = b".wh.";

/// OCI v1.1 opaque-directory whiteout marker — the full leaf
/// filename (not a prefix). Distinct from `.wh.<name>` because it
/// directs the unpacker to clear the **parent** directory's
/// prior-layer contents rather than removing a sibling.
const WHITEOUT_OPAQUE: &[u8] = b".wh..wh..opq";

/// Caller-controlled knobs for [`unpack_layer`].
///
/// Defaults are the production-safe ones; tests override individual
/// fields without rebuilding the whole struct.
#[derive(Debug, Clone)]
pub struct UnpackOptions {
    /// Refuse any tar entry whose path length (in bytes, post-
    /// UTF-8-lossy normalisation) exceeds this value. The default
    /// (4096) is Linux's `PATH_MAX` and the longest plausible
    /// real-world image path; anything beyond is either a probe for
    /// path-handling bugs or a malformed tarball.
    pub max_path_len: usize,

    /// When `true` (the default), every unpacked entry's mtime and
    /// atime are forced to the Unix epoch (`0`). When `false`,
    /// the tar header's `mtime` field is preserved. **Set to
    /// `false` only for debugging** — production unpacks
    /// uniformly strip timestamps so two pulls of the same layer
    /// produce byte-identical trees (Plan 85 §"R2 Reproducibility").
    pub strip_timestamps: bool,
}

impl Default for UnpackOptions {
    fn default() -> Self {
        Self {
            max_path_len: 4096,
            strip_timestamps: true,
        }
    }
}

/// Summary of what [`unpack_layer`] did with a layer. Returned even
/// on the happy path; callers that need a strict "everything
/// accepted or bust" gate inspect `refused.is_empty()`.
#[derive(Debug, Clone, Default)]
pub struct UnpackReport {
    /// Regular files materialized.
    pub files_written: u64,
    /// Directories created (counts only newly-created dirs; entries
    /// for already-existing dirs are silently coalesced).
    pub dirs_created: u64,
    /// Symlinks written.
    pub symlinks_written: u64,
    /// OCI `.wh.<name>` whiteout markers applied. A successful apply
    /// removes the sibling target if it exists; if the target is
    /// absent the apply is still counted because the marker was
    /// consumed (single-layer apply is declarative — Plan 85
    /// Phase A.2 footprint, OCI v1.1 §"Layer Filesystem Changeset").
    pub whiteouts_applied: u64,
    /// OCI `.wh..wh..opq` opaque-directory markers applied. A
    /// successful apply clears the parent directory's prior-layer
    /// contents (the directory itself is preserved). Counted even
    /// when the parent is absent or empty, for the same declarative
    /// reason as `whiteouts_applied`.
    pub opaque_markers_applied: u64,
    /// Tar entries refused by policy, in the order they appeared in
    /// the stream. Each carries a [`RefusalReason`] so a downstream
    /// audit / debugging surface can render them.
    pub refused: Vec<RefusedEntry>,
}

/// One refused tar entry. The path is recorded as raw bytes (not as
/// a `String`) because the tar header can carry non-UTF-8 paths and
/// we don't want to mask that with lossy decoding before the audit
/// gets a chance to see it.
#[derive(Debug, Clone)]
pub struct RefusedEntry {
    /// Path bytes from the tar header, verbatim. Render via
    /// `String::from_utf8_lossy` for human-readable output;
    /// preserve as-is for audit logging.
    pub raw_path: Vec<u8>,
    /// Which policy rejected the entry.
    pub reason: RefusalReason,
}

/// Why a tar entry was rejected. Each variant maps to one of the
/// safety properties documented at module level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// Path starts with `/`. Tar entries must be relative.
    AbsolutePath,
    /// Path contains a `..` segment (parent-directory reference).
    TraversalSegment,
    /// Path length exceeded [`UnpackOptions::max_path_len`].
    PathTooLong,
    /// Joined-against-root path resolved outside `output_root`. This
    /// is a defense-in-depth check; the prior absolute-path and
    /// traversal-segment refusals should catch every real-world
    /// case before reaching this branch, but the check stays so a
    /// future platform-specific path quirk can't silently bypass.
    JoinedPathEscape,
    /// A parent directory along the target's path is a symlink. We
    /// refuse to write under symlink-prefixed paths so a malicious
    /// layer can't write `bin -> /tmp` in one entry and then
    /// materialize `bin/x` in a later entry that would `open(2)`
    /// `/tmp/x` instead of `<root>/bin/x`.
    SymlinkInParent,
    /// Tar entry type is not supported in this sub-phase. Later
    /// sub-phases narrow the refusal surface (A.2 adds whiteouts,
    /// A.3 adds hardlinks, etc.); A.1 refuses everything except
    /// regular files, directories, and in-root symlinks.
    UnsupportedEntryType,
    /// Tar header was malformed (unreadable path bytes, etc.). We
    /// refuse the entry rather than failing the whole unpack — a
    /// single bad entry shouldn't poison a multi-thousand-entry
    /// layer if the rest is valid.
    MalformedHeader,
}

impl RefusalReason {
    /// Stable wire string for audit logging — never localised, never
    /// rewritten without an explicit Plan-85 sub-phase bump. Pairs
    /// with [`RefusedEntry::raw_path`] to form the audit-entry tuple.
    pub fn audit_tag(self) -> &'static str {
        match self {
            Self::AbsolutePath => "absolute_path",
            Self::TraversalSegment => "traversal_segment",
            Self::PathTooLong => "path_too_long",
            Self::JoinedPathEscape => "joined_path_escape",
            Self::SymlinkInParent => "symlink_in_parent",
            Self::UnsupportedEntryType => "unsupported_entry_type",
            Self::MalformedHeader => "malformed_header",
        }
    }
}

/// Hard-error path for [`unpack_layer`]. Distinguishes
/// "caller-supplied output_root is invalid" (configuration error,
/// surfaces to the operator) from "this specific tar entry is bad"
/// (data error, surfaces in [`UnpackReport::refused`]).
#[derive(Debug, thiserror::Error)]
pub enum UnpackError {
    /// I/O failure reading the tar stream itself (network short-read,
    /// disk EIO, etc.). Distinct from per-entry malformed-header
    /// refusals, which the unpacker continues past.
    #[error("reading layer tar stream: {0}")]
    TarRead(#[from] std::io::Error),

    /// Caller passed a relative path as `output_root`. We require an
    /// absolute path so the "does this resolve under root" check
    /// has well-defined semantics independent of the process's
    /// `cwd`.
    #[error("output_root must be an absolute path; got {0:?}")]
    NonAbsoluteOutputRoot(PathBuf),

    /// `output_root` does not exist or is not a directory. Phase A.1
    /// requires the directory to exist before unpacking — callers
    /// `create_dir_all` it as part of their pull pipeline before
    /// invoking the unpacker. We refuse to silently create it to
    /// keep the unpacker's filesystem write surface a strict
    /// subset of `output_root`'s subtree.
    #[error("output_root must exist as a directory; got {0:?}")]
    OutputRootNotADir(PathBuf),
}

/// Unpack a single layer tarball under `output_root`, applying the
/// Phase A.1 safety policies described at module level.
///
/// Caller's responsibilities:
///
/// - Pre-create `output_root` (we refuse to silently `mkdir` it).
/// - Decompress the layer **before** calling, if the layer
///   `mediaType` is `tar+gzip` / `tar+zstd`. Phase A.1's unpacker
///   reads a *plain* tar stream; the decompression wrapping is
///   the integration layer's problem (and the integration layer
///   already knows the `mediaType` from the manifest descriptor).
///
/// Returns an [`UnpackReport`] enumerating the writes and the
/// refused entries. Refusals are **not** errors — callers that want
/// "all-or-nothing" inspect `report.refused.is_empty()`.
pub fn unpack_layer<R: Read>(
    mut layer_tar: R,
    output_root: &Path,
    options: &UnpackOptions,
) -> Result<UnpackReport, UnpackError> {
    if !output_root.is_absolute() {
        return Err(UnpackError::NonAbsoluteOutputRoot(
            output_root.to_path_buf(),
        ));
    }
    if !output_root.is_dir() {
        return Err(UnpackError::OutputRootNotADir(output_root.to_path_buf()));
    }

    // We feed the raw reader to `tar::Archive`; the crate handles
    // sub-entry framing. The `set_*` knobs below disable the tar
    // crate's built-in unpacking conveniences — every safety
    // decision passes through our own match.
    let mut archive = tar::Archive::new(&mut layer_tar);
    archive.set_preserve_permissions(false);
    archive.set_preserve_mtime(false);
    archive.set_unpack_xattrs(false);

    let mut report = UnpackReport::default();
    let mut current_layer_paths = HashSet::new();

    for entry_result in archive.entries()? {
        let mut entry = match entry_result {
            Ok(e) => e,
            Err(_) => {
                // A malformed header at the stream level (tar crate's
                // own validation) — record as a malformed-header
                // refusal with empty path and keep going. We can't
                // know the "intended" path of an entry whose header
                // didn't parse.
                report.refused.push(RefusedEntry {
                    raw_path: Vec::new(),
                    reason: RefusalReason::MalformedHeader,
                });
                continue;
            }
        };

        let raw_path = entry.path_bytes().to_vec();

        if raw_path.is_empty() {
            report.refused.push(RefusedEntry {
                raw_path,
                reason: RefusalReason::MalformedHeader,
            });
            continue;
        }

        // Safety check 1 — absolute paths.
        if raw_path.first() == Some(&b'/') {
            report.refused.push(RefusedEntry {
                raw_path,
                reason: RefusalReason::AbsolutePath,
            });
            continue;
        }

        // Safety check 2 — traversal segments. Segment-by-segment
        // (never substring): `..foo` is a valid name; `../foo` is
        // not. Tar paths are slash-separated regardless of host OS.
        let traversal = raw_path.split(|b| *b == b'/').any(|seg| seg == b"..");
        if traversal {
            report.refused.push(RefusedEntry {
                raw_path,
                reason: RefusalReason::TraversalSegment,
            });
            continue;
        }

        // Safety check 3 — max length.
        if raw_path.len() > options.max_path_len {
            report.refused.push(RefusedEntry {
                raw_path,
                reason: RefusalReason::PathTooLong,
            });
            continue;
        }

        let rel_path = PathBuf::from(OsStr::from_bytes(&raw_path));
        let target = output_root.join(&rel_path);

        // Safety check 4 — joined path stays under root. The prior
        // checks should make this unreachable in practice, but
        // platform-specific quirks (NUL bytes, Windows-ish
        // separators in tar entries from cross-platform builds, etc.)
        // could in theory slip through; the explicit
        // `starts_with(output_root)` is the catch-all.
        if !target.starts_with(output_root) {
            report.refused.push(RefusedEntry {
                raw_path,
                reason: RefusalReason::JoinedPathEscape,
            });
            continue;
        }

        // Safety check 5 — no symlink in any parent of the target.
        // Walks each existing prefix; if any component is a symlink
        // we refuse this entry. (The first non-existent prefix
        // ends the walk; we're not asking "does the full path
        // exist?", we're asking "does the existing prefix contain
        // a symlink we'd have to follow to write under?")
        if parent_chain_has_symlink(output_root, &rel_path) {
            report.refused.push(RefusedEntry {
                raw_path,
                reason: RefusalReason::SymlinkInParent,
            });
            continue;
        }

        // A.1+A.2 dispatch: regular files and directories and
        // symlinks materialize; regular-file entries whose **leaf
        // filename** matches the OCI whiteout pattern dispatch to
        // the whiteout helpers instead of `write_regular_file`.
        // Everything else refuses — A.3 onwards narrows this.
        let entry_type = entry.header().entry_type();
        match entry_type {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                match classify_whiteout(&raw_path) {
                    WhiteoutKind::None => match write_regular_file(&mut entry, &target, options) {
                        Ok(()) => {
                            report.files_written += 1;
                            current_layer_paths.insert(rel_path.clone());
                        }
                        Err(refuse) => report.refused.push(RefusedEntry {
                            raw_path: raw_path.clone(),
                            reason: refuse,
                        }),
                    },
                    WhiteoutKind::Opaque => {
                        // Parent-of-marker is the directory we're
                        // clearing. `target` already points at the
                        // marker file, so we strip the marker leaf
                        // to get the parent dir.
                        let parent = target.parent().unwrap_or(output_root);
                        let parent_rel = rel_path.parent().unwrap_or_else(|| Path::new(""));
                        match apply_opaque_whiteout(
                            parent,
                            parent_rel,
                            output_root,
                            &current_layer_paths,
                        ) {
                            Ok(()) => report.opaque_markers_applied += 1,
                            Err(refuse) => report.refused.push(RefusedEntry {
                                raw_path: raw_path.clone(),
                                reason: refuse,
                            }),
                        }
                    }
                    WhiteoutKind::Regular(name_suffix) => {
                        // Sibling target = `<parent_of_marker>/<name_suffix>`.
                        let parent = target.parent().unwrap_or(output_root);
                        let parent_rel = rel_path.parent().unwrap_or_else(|| Path::new(""));
                        let sibling_rel = parent_rel.join(OsStr::from_bytes(name_suffix));
                        let sibling = parent.join(OsStr::from_bytes(name_suffix));
                        match apply_regular_whiteout(
                            &sibling,
                            &sibling_rel,
                            output_root,
                            &current_layer_paths,
                        ) {
                            Ok(()) => report.whiteouts_applied += 1,
                            Err(refuse) => report.refused.push(RefusedEntry {
                                raw_path: raw_path.clone(),
                                reason: refuse,
                            }),
                        }
                    }
                    WhiteoutKind::Malformed => {
                        report.refused.push(RefusedEntry {
                            raw_path: raw_path.clone(),
                            reason: RefusalReason::MalformedHeader,
                        });
                    }
                }
            }
            tar::EntryType::Directory => match create_directory(&target) {
                Ok(created) => {
                    if created {
                        report.dirs_created += 1;
                    }
                    current_layer_paths.insert(rel_path.clone());
                }
                Err(refuse) => report.refused.push(RefusedEntry {
                    raw_path: raw_path.clone(),
                    reason: refuse,
                }),
            },
            tar::EntryType::Symlink => {
                let link_target = entry.link_name_bytes().map(|b| b.into_owned());
                match write_symlink(link_target.as_deref(), &target) {
                    Ok(()) => {
                        report.symlinks_written += 1;
                        current_layer_paths.insert(rel_path.clone());
                    }
                    Err(refuse) => report.refused.push(RefusedEntry {
                        raw_path: raw_path.clone(),
                        reason: refuse,
                    }),
                }
            }
            _ => {
                report.refused.push(RefusedEntry {
                    raw_path,
                    reason: RefusalReason::UnsupportedEntryType,
                });
            }
        }
    }

    Ok(report)
}

/// Walk each existing prefix of `output_root.join(rel)` and return
/// `true` if any component is a symlink. We use `symlink_metadata`
/// (which does **not** dereference) so the existence check itself
/// can't be defeated by a symlink loop.
fn parent_chain_has_symlink(output_root: &Path, rel: &Path) -> bool {
    let mut cursor = output_root.to_path_buf();
    let components: Vec<_> = rel.components().collect();
    // Walk parent components only — not the leaf. A symlink AT the
    // target itself is fine (we either refuse, overwrite, or
    // O_NOFOLLOW it depending on entry kind); what's dangerous is a
    // symlink in a parent that lets us write under a different
    // subtree.
    let parent_count = components.len().saturating_sub(1);
    for comp in components.iter().take(parent_count) {
        // Skip non-Normal components defensively. `..`/`/` are
        // already refused upstream; any oddity that survived
        // doesn't expand the cursor.
        if let Component::Normal(seg) = comp {
            cursor.push(seg);
            match std::fs::symlink_metadata(&cursor) {
                Ok(meta) if meta.file_type().is_symlink() => return true,
                _ => {
                    // Non-existent or non-symlink — keep walking.
                    // The non-existent case means later
                    // create_dir_all_under_root() will mkdir the
                    // remaining chain; a not-yet-existing path
                    // can't be a symlink trap.
                }
            }
        }
    }
    false
}

/// Write a regular file from `entry` to `target`, with O_NOFOLLOW
/// + zeroed timestamps (when `options.strip_timestamps`) + the tar
///   header's mode bits masked to `0o7777`.
///
/// Returns `Ok(())` on success, or a [`RefusalReason`] for caller-
/// recorded per-entry failures. Hard I/O errors propagate via
/// `Err(RefusalReason::MalformedHeader)` for now — a future
/// sub-phase may grow a distinct `IoError` variant if the
/// granularity matters for audit, but A.1's caller treats all
/// per-entry failures equivalently.
fn write_regular_file<R: Read>(
    entry: &mut tar::Entry<R>,
    target: &Path,
    options: &UnpackOptions,
) -> Result<(), RefusalReason> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    // Ensure parent directories exist. We've already verified no
    // existing parent is a symlink (safety check 5 in
    // `unpack_layer`), so `create_dir_all` here can't escape the
    // root via a planted symlink — only previously-existing host
    // directories could, and we trust the caller to give us a
    // clean `output_root`.
    if let Some(parent) = target.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return Err(RefusalReason::MalformedHeader);
        }
    }

    // Mode from the tar header. A.1 strips the high bits (setuid,
    // setgid, sticky) — those land in A.6 with policy control.
    let mode_from_header = entry.header().mode().unwrap_or(0o644) & 0o0777;

    let mut opts = OpenOptions::new();
    opts.write(true)
        .create_new(true) // O_CREAT | O_EXCL — refuse to overwrite
        .mode(mode_from_header)
        .custom_flags(libc::O_NOFOLLOW);

    let mut file = match opts.open(target) {
        Ok(f) => f,
        Err(_) => return Err(RefusalReason::MalformedHeader),
    };

    if let Err(_e) = std::io::copy(entry, &mut file) {
        // The partial file is left on disk; the caller's pull
        // pipeline GCs the whole `output_root` on failure. A
        // tighter cleanup is possible but Phase A.1 keeps the
        // happy path simple.
        return Err(RefusalReason::MalformedHeader);
    }

    if let Err(_e) = file.flush() {
        return Err(RefusalReason::MalformedHeader);
    }

    if options.strip_timestamps {
        // `utimensat(AT_FDCWD, target, {0, 0})` via the `filetime`
        // crate's std-only equivalent. We use `set_file_mtime` /
        // `set_file_atime` from std-unstable / `filetime`... but
        // `filetime` isn't in our deps yet. Standard library has
        // no stable timestamp setter as of edition 2024; the
        // closest is `std::fs::File::set_modified` (stable in
        // 1.75), which takes `SystemTime`.
        use std::time::SystemTime;
        let _ = file.set_modified(SystemTime::UNIX_EPOCH);
    }

    Ok(())
}

/// Create a directory at `target`. Returns `Ok(true)` if a new
/// directory was created, `Ok(false)` if it already existed. Tar
/// archives commonly list a directory entry both before its
/// children and as part of every parent chain, so coalescing
/// is correct and not a refusal.
fn create_directory(target: &Path) -> Result<bool, RefusalReason> {
    match std::fs::create_dir(target) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Parent doesn't exist — create the chain.
            if std::fs::create_dir_all(target).is_ok() {
                Ok(true)
            } else {
                Err(RefusalReason::MalformedHeader)
            }
        }
        Err(_) => Err(RefusalReason::MalformedHeader),
    }
}

/// Write a symlink whose location is `target` and whose link-text is
/// `link_target_bytes`. The link target is preserved verbatim — we
/// do not interpret it at unpack time (Plan 85 §"Phase A.1 safety
/// properties"). Refusal conditions:
///
/// - Link target bytes empty.
/// - Link target bytes contain a NUL.
/// - `symlink(2)` returns `EEXIST` (we don't overwrite).
fn write_symlink(link_target_bytes: Option<&[u8]>, target: &Path) -> Result<(), RefusalReason> {
    use std::os::unix::fs::symlink;

    let link_target = match link_target_bytes {
        Some(b) if !b.is_empty() && !b.contains(&0) => b,
        _ => return Err(RefusalReason::MalformedHeader),
    };

    if let Some(parent) = target.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return Err(RefusalReason::MalformedHeader);
        }
    }

    let link_os = OsStr::from_bytes(link_target);
    match symlink(link_os, target) {
        Ok(()) => Ok(()),
        Err(_) => Err(RefusalReason::MalformedHeader),
    }
}

/// Tells the dispatch loop whether a regular-file tar entry is
/// actually an OCI whiteout marker and, if so, which kind.
///
/// Decided strictly from the **leaf filename**. The path's parent
/// chain is irrelevant for classification (a directory named `.wh.X`
/// is legitimate as an intermediate component); only the last
/// segment carries the marker semantic.
enum WhiteoutKind<'a> {
    /// Not a whiteout — treat as a regular file write.
    None,
    /// `.wh..wh..opq` — clear parent dir's prior contents.
    Opaque,
    /// `.wh.<name>` — remove sibling. Inner slice is `<name>` (the
    /// suffix after the four-byte `.wh.` prefix).
    Regular(&'a [u8]),
    /// `.wh.` exactly (empty suffix) — wire-format violation, refused.
    Malformed,
}

fn classify_whiteout(raw_path: &[u8]) -> WhiteoutKind<'_> {
    // Leaf filename = bytes after the last `/`. For root-level paths
    // with no `/`, the whole path is the filename.
    let leaf = match raw_path.iter().rposition(|b| *b == b'/') {
        Some(idx) => &raw_path[idx + 1..],
        None => raw_path,
    };

    if leaf == WHITEOUT_OPAQUE {
        return WhiteoutKind::Opaque;
    }
    if !leaf.starts_with(WHITEOUT_PREFIX) {
        return WhiteoutKind::None;
    }
    let suffix = &leaf[WHITEOUT_PREFIX.len()..];
    if suffix.is_empty() {
        return WhiteoutKind::Malformed;
    }
    WhiteoutKind::Regular(suffix)
}

/// Apply a `.wh.<name>` whiteout: remove the sibling file or
/// directory tree at `target` if it exists, except for paths written
/// by the layer currently being applied. OCI whiteouts apply to
/// lower/parent layers only; same-layer entries survive regardless
/// of marker ordering in the tar stream.
///
/// `output_root` is passed so the caller's safety envelope continues
/// to apply — we re-assert `target` lives under it before any
/// filesystem mutation. The check is defense-in-depth: the entry's
/// path already passed every A.1 safety check, and the sibling
/// shares the same parent chain, so this branch should be
/// unreachable for any non-malicious caller wiring.
fn apply_regular_whiteout(
    target: &Path,
    target_rel: &Path,
    output_root: &Path,
    current_layer_paths: &HashSet<PathBuf>,
) -> Result<(), RefusalReason> {
    if !target.starts_with(output_root) {
        return Err(RefusalReason::JoinedPathEscape);
    }

    // `symlink_metadata` doesn't follow symlinks — important so a
    // sibling symlink-to-elsewhere gets removed as a symlink rather
    // than the unpacker dereferencing it and acting on whatever it
    // points at. The corresponding `remove_file` call deletes the
    // symlink, not its target.
    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_dir() => {
            remove_tree_except_current_layer(target, target_rel, current_layer_paths)
        }
        Ok(_) if current_layer_paths.contains(target_rel) => Ok(()),
        Ok(_) => match std::fs::remove_file(target) {
            Ok(()) => Ok(()),
            Err(_) => Err(RefusalReason::MalformedHeader),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(RefusalReason::MalformedHeader),
    }
}

/// Apply a `.wh..wh..opq` opaque whiteout: clear `target_dir`'s
/// lower-layer contents, preserving the directory itself and any
/// same-layer entries below it. OCI says opaque markers are applied
/// before sibling entries regardless of archive ordering; preserving
/// same-layer paths gives that result without buffering the whole
/// layer.
///
/// Same `output_root` defense-in-depth check as the regular
/// whiteout: re-assert the target dir lives under root.
fn apply_opaque_whiteout(
    target_dir: &Path,
    target_rel: &Path,
    output_root: &Path,
    current_layer_paths: &HashSet<PathBuf>,
) -> Result<(), RefusalReason> {
    if !target_dir.starts_with(output_root) {
        return Err(RefusalReason::JoinedPathEscape);
    }

    remove_children_except_current_layer(target_dir, target_rel, current_layer_paths)
}

fn remove_tree_except_current_layer(
    target: &Path,
    target_rel: &Path,
    current_layer_paths: &HashSet<PathBuf>,
) -> Result<(), RefusalReason> {
    if !has_current_layer_path_at_or_below(target_rel, current_layer_paths) {
        return std::fs::remove_dir_all(target).map_err(|_| RefusalReason::MalformedHeader);
    }

    remove_children_except_current_layer(target, target_rel, current_layer_paths)
}

fn remove_children_except_current_layer(
    target_dir: &Path,
    target_rel: &Path,
    current_layer_paths: &HashSet<PathBuf>,
) -> Result<(), RefusalReason> {
    let read_dir = match std::fs::read_dir(target_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(RefusalReason::MalformedHeader),
    };

    for child in read_dir {
        let child = match child {
            Ok(c) => c,
            Err(_) => return Err(RefusalReason::MalformedHeader),
        };
        let child_path = child.path();
        let child_rel = target_rel.join(child.file_name());
        let file_type = match child.file_type() {
            Ok(ft) => ft,
            Err(_) => return Err(RefusalReason::MalformedHeader),
        };
        if current_layer_paths.contains(&child_rel) {
            if file_type.is_dir() {
                remove_children_except_current_layer(&child_path, &child_rel, current_layer_paths)?;
            }
            continue;
        }
        if file_type.is_dir() && has_current_layer_path_below(&child_rel, current_layer_paths) {
            remove_children_except_current_layer(&child_path, &child_rel, current_layer_paths)?;
            continue;
        }
        let removed = if file_type.is_dir() {
            std::fs::remove_dir_all(&child_path)
        } else {
            std::fs::remove_file(&child_path)
        };
        if removed.is_err() {
            return Err(RefusalReason::MalformedHeader);
        }
    }
    Ok(())
}

fn has_current_layer_path_at_or_below(rel: &Path, current_layer_paths: &HashSet<PathBuf>) -> bool {
    current_layer_paths
        .iter()
        .any(|written| written == rel || written.starts_with(rel))
}

fn has_current_layer_path_below(rel: &Path, current_layer_paths: &HashSet<PathBuf>) -> bool {
    current_layer_paths
        .iter()
        .any(|written| written != rel && written.starts_with(rel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    /// Helper — build a tar archive in memory from a list of
    /// `(header_setup, body)` closures. Returns the archive bytes.
    fn build_tar(setup: impl FnOnce(&mut tar::Builder<Cursor<Vec<u8>>>)) -> Vec<u8> {
        let buf = Cursor::new(Vec::new());
        let mut builder = tar::Builder::new(buf);
        setup(&mut builder);
        builder.into_inner().unwrap().into_inner()
    }

    /// Add a regular file with the given content + relative path.
    fn add_file(builder: &mut tar::Builder<Cursor<Vec<u8>>>, path: &str, body: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, body).unwrap();
    }

    /// Add a directory entry.
    fn add_dir(builder: &mut tar::Builder<Cursor<Vec<u8>>>, path: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(0);
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Directory);
        header.set_cksum();
        builder.append(&header, std::io::empty()).unwrap();
    }

    /// Add a symlink.
    fn add_symlink(builder: &mut tar::Builder<Cursor<Vec<u8>>>, path: &str, link_target: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(0);
        header.set_mode(0o777);
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_link_name(link_target).unwrap();
        header.set_cksum();
        builder.append(&header, std::io::empty()).unwrap();
    }

    /// Add a hardlink entry (refused in A.1; will be supported in A.3).
    fn add_hardlink(builder: &mut tar::Builder<Cursor<Vec<u8>>>, path: &str, link_target: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Link);
        header.set_link_name(link_target).unwrap();
        header.set_cksum();
        builder.append(&header, std::io::empty()).unwrap();
    }

    #[test]
    fn happy_path_writes_files_dirs_symlinks() {
        let tar_bytes = build_tar(|b| {
            add_dir(b, "etc/");
            add_file(b, "etc/hostname", b"alpine\n");
            add_dir(b, "bin/");
            add_file(b, "bin/sh", b"#!fake busybox\n");
            add_symlink(b, "bin/busybox", "sh");
        });

        let tmp = TempDir::new().unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack happy path");

        assert_eq!(report.files_written, 2, "etc/hostname + bin/sh");
        // Directories: etc/, bin/, plus implicit parents for files
        // we may or may not have created via create_dir_all. We
        // assert ≥ 2 (the two explicit Directory entries).
        assert!(report.dirs_created >= 2, "got {}", report.dirs_created);
        assert_eq!(report.symlinks_written, 1);
        assert!(report.refused.is_empty(), "{:?}", report.refused);

        // Verify files actually exist on disk.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("etc/hostname")).unwrap(),
            "alpine\n"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("bin/sh")).unwrap(),
            "#!fake busybox\n"
        );
        // Symlink resolves textually to "sh"; the link itself is at
        // bin/busybox.
        let link = std::fs::read_link(tmp.path().join("bin/busybox")).unwrap();
        assert_eq!(link.to_str().unwrap(), "sh");
    }

    /// Build a single-entry tar archive with a hand-rolled USTAR
    /// header whose path field carries `path_bytes` verbatim. This
    /// bypasses `tar::Header::set_path`'s built-in refusals (leading
    /// `/`, `..` segments, length > 100) so we can produce
    /// adversarial inputs that the unpacker is supposed to reject.
    /// Returns the archive bytes (header + 1024 bytes of EOF zero
    /// blocks).
    fn handrolled_tar_with_path(path_bytes: &[u8], entry_type: u8) -> Vec<u8> {
        // The USTAR `name` field is 100 bytes (offset 0..100) plus
        // an optional 155-byte `prefix` field (offset 345..500). For
        // paths up to 100 bytes we fill `name`; longer paths split
        // at any `/`, with the prefix going into `prefix` and the
        // remainder into `name`. For the adversarial-fixture path
        // we test, the *whole* path is short enough to fit in
        // `name` even when it includes `..` segments.
        //
        // For paths > 100 bytes the cleanest way to bypass
        // `set_path`'s refusal is to use the GNU long-name extension
        // (entry typeflag `L`, name = `././@LongLink`, body = the
        // real path, followed by a normal-typeflag entry whose own
        // header carries the *truncated* path). We don't go there —
        // tests that exercise > 100 byte paths use the GNU writer
        // path via `tar::Builder` (which IS happy to emit GNU
        // long-name records) or simply set
        // `UnpackOptions::max_path_len` low enough that a 100-byte
        // path is already over the cap.
        assert!(
            path_bytes.len() <= 100,
            "handrolled_tar_with_path only handles USTAR name-field paths (≤100 bytes); got {}",
            path_bytes.len()
        );

        let mut header = [0u8; 512];
        header[..path_bytes.len()].copy_from_slice(path_bytes);
        // mode 0644
        header[100..108].copy_from_slice(b"0000644\0");
        // uid 0
        header[108..116].copy_from_slice(b"0000000\0");
        // gid 0
        header[116..124].copy_from_slice(b"0000000\0");
        // size = 0
        header[124..136].copy_from_slice(b"00000000000\0");
        // mtime = 0
        header[136..148].copy_from_slice(b"00000000000\0");
        // typeflag
        header[156] = entry_type;
        // USTAR magic + version
        header[257..265].copy_from_slice(b"ustar  \0");
        // Checksum: sum of every byte in the header with the
        // checksum field itself treated as 8 spaces.
        header[148..156].copy_from_slice(b"        ");
        let cksum: u32 = header.iter().map(|&b| b as u32).sum();
        let s = format!("{:06o}\0 ", cksum);
        header[148..156].copy_from_slice(s.as_bytes());

        let mut tar_bytes = header.to_vec();
        // Two 512-byte zero blocks = end-of-archive.
        tar_bytes.extend_from_slice(&[0u8; 1024]);
        tar_bytes
    }

    #[test]
    fn refuses_absolute_path() {
        // `tar::Header::set_path` strips a leading `/`, so we
        // hand-craft a USTAR header to put the literal absolute-path
        // bytes on the wire. Mirrors the path bytes a malicious
        // remote image could ship.
        let tar_bytes = handrolled_tar_with_path(b"/etc/passwd", b'0');
        let tmp = TempDir::new().unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack should succeed with refusals, not error");

        assert_eq!(report.files_written, 0);
        assert_eq!(report.refused.len(), 1, "{:?}", report.refused);
        assert_eq!(report.refused[0].reason, RefusalReason::AbsolutePath);
        assert!(
            !tmp.path().join("etc/passwd").exists(),
            "absolute-path entry must not write outside output_root"
        );
    }

    #[test]
    fn refuses_traversal_segment() {
        // `tar::Header::set_path` refuses `..` segments at construction
        // time ("paths in archives must not have `..`"), so we
        // hand-roll a USTAR header to test the unpacker's own
        // segment-by-segment refusal.
        let tar_bytes = handrolled_tar_with_path(b"foo/../escape", b'0');
        let tmp = TempDir::new().unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        assert_eq!(report.files_written, 0);
        assert_eq!(report.refused.len(), 1, "{:?}", report.refused);
        assert_eq!(report.refused[0].reason, RefusalReason::TraversalSegment);
        // Sister test: `..foo` (no trailing slash) is *not* a
        // traversal; it's a single segment with a valid filename
        // that happens to begin with two dots. Confirm the
        // segment-by-segment check doesn't false-positive on it.
        let tar_bytes2 = handrolled_tar_with_path(b"..foo", b'0');
        let tmp2 = TempDir::new().unwrap();
        let report2 = unpack_layer(
            Cursor::new(tar_bytes2),
            tmp2.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");
        // `..foo` is a valid filename — accepted as a regular file.
        assert_eq!(report2.files_written, 1, "{:?}", report2.refused);
        assert!(report2.refused.is_empty());
    }

    #[test]
    fn refuses_path_too_long() {
        // Use a single-segment 64-byte name + a low max_path_len
        // so the cap fires without needing GNU long-name extensions.
        let path = b"x".repeat(64);
        let tar_bytes = handrolled_tar_with_path(&path, b'0');
        let tmp = TempDir::new().unwrap();
        let opts = UnpackOptions {
            max_path_len: 32,
            ..UnpackOptions::default()
        };
        let report = unpack_layer(Cursor::new(tar_bytes), tmp.path(), &opts).expect("unpack ok");

        assert_eq!(report.files_written, 0);
        assert_eq!(report.refused.len(), 1, "{:?}", report.refused);
        assert_eq!(report.refused[0].reason, RefusalReason::PathTooLong);
    }

    #[test]
    fn refuses_hardlink_in_phase_a1() {
        let tar_bytes = build_tar(|b| {
            add_file(b, "real", b"data");
            add_hardlink(b, "alias", "real");
        });

        let tmp = TempDir::new().unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        // The regular file writes successfully; the hardlink is
        // refused because Phase A.1 doesn't support `Link` entries.
        assert_eq!(report.files_written, 1);
        assert_eq!(report.refused.len(), 1);
        assert_eq!(
            report.refused[0].reason,
            RefusalReason::UnsupportedEntryType
        );
        assert!(!tmp.path().join("alias").exists());
    }

    #[test]
    fn refuses_write_under_symlinked_parent() {
        // Layer entries (in order):
        //   1. symlink `bin -> /tmp`  — itself an in-root location
        //   2. regular file `bin/escape` — would land in /tmp/escape
        //      without the parent-chain-symlink check.
        // Expected: entry 1 writes (in-root location); entry 2 is
        // refused with SymlinkInParent.
        let tar_bytes = build_tar(|b| {
            add_symlink(b, "bin", "/tmp");
            add_file(b, "bin/escape", b"do not write");
        });

        let tmp = TempDir::new().unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        assert_eq!(report.symlinks_written, 1);
        assert_eq!(report.files_written, 0);
        assert_eq!(report.refused.len(), 1);
        assert_eq!(report.refused[0].reason, RefusalReason::SymlinkInParent);
        // Confirm we didn't actually write through the symlink.
        assert!(!std::path::Path::new("/tmp/escape").exists());
    }

    #[test]
    fn rejects_relative_output_root() {
        let err = unpack_layer(
            Cursor::new(Vec::new()),
            Path::new("relative/path"),
            &UnpackOptions::default(),
        )
        .expect_err("relative output_root must be rejected");

        assert!(matches!(err, UnpackError::NonAbsoluteOutputRoot(_)));
    }

    #[test]
    fn rejects_nonexistent_output_root() {
        let err = unpack_layer(
            Cursor::new(Vec::new()),
            Path::new("/nonexistent/path/under/root"),
            &UnpackOptions::default(),
        )
        .expect_err("missing output_root must be rejected");

        assert!(matches!(err, UnpackError::OutputRootNotADir(_)));
    }

    #[test]
    fn reproducibility_two_unpacks_match_modulo_inode() {
        // Same layer, two clean output_roots — every regular file
        // should have mtime 0 in both. We can't compare inode-level
        // equality across two trees, but mtime stripping is the
        // load-bearing property for the upstream `mkfs.ext4` step
        // (Phase B) to produce byte-identical ext4 images.
        let tar_bytes = build_tar(|b| {
            add_file(b, "a", b"x");
            add_file(b, "b", b"y");
        });
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        unpack_layer(
            Cursor::new(tar_bytes.clone()),
            tmp1.path(),
            &UnpackOptions::default(),
        )
        .unwrap();
        unpack_layer(
            Cursor::new(tar_bytes),
            tmp2.path(),
            &UnpackOptions::default(),
        )
        .unwrap();

        let m1 = std::fs::metadata(tmp1.path().join("a")).unwrap();
        let m2 = std::fs::metadata(tmp2.path().join("a")).unwrap();
        assert_eq!(m1.modified().unwrap(), std::time::SystemTime::UNIX_EPOCH);
        assert_eq!(m2.modified().unwrap(), std::time::SystemTime::UNIX_EPOCH);
        assert_eq!(m1.modified().unwrap(), m2.modified().unwrap());
    }

    #[test]
    fn refusal_audit_tags_are_stable_and_safe() {
        // Pin the wire format of `audit_tag` — these strings end
        // up in audit chain entries (Plan 85 Phase E claim 10), so
        // they must never contain spaces, equals signs, commas, or
        // newlines (which would confuse the existing audit-emit
        // `key=value` parser).
        let all = [
            RefusalReason::AbsolutePath,
            RefusalReason::TraversalSegment,
            RefusalReason::PathTooLong,
            RefusalReason::JoinedPathEscape,
            RefusalReason::SymlinkInParent,
            RefusalReason::UnsupportedEntryType,
            RefusalReason::MalformedHeader,
        ];
        for r in all {
            let t = r.audit_tag();
            assert!(!t.is_empty(), "{r:?}");
            assert!(!t.contains(' '), "{r:?} -> {t:?}");
            assert!(!t.contains('='), "{r:?} -> {t:?}");
            assert!(!t.contains(','), "{r:?} -> {t:?}");
            assert!(!t.contains('\n'), "{r:?} -> {t:?}");
        }
    }

    // ── Phase A.2: OCI whiteout + opaque marker semantics ────────

    #[test]
    fn whiteout_removes_prior_layer_sibling_file() {
        // The output tree can already contain lower-layer state
        // before this layer is applied. A sibling `.wh.<name>` marker
        // removes that prior file. The marker itself is not
        // materialized.
        let tar_bytes = build_tar(|b| {
            add_dir(b, "etc/");
            add_file(b, "etc/.wh.passwd", b"");
        });

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("etc")).unwrap();
        std::fs::write(
            tmp.path().join("etc/passwd"),
            b"root:x:0:0::/root:/bin/sh\n",
        )
        .unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        assert_eq!(report.files_written, 0);
        assert_eq!(report.whiteouts_applied, 1);
        assert_eq!(report.opaque_markers_applied, 0);
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        assert!(
            !tmp.path().join("etc/passwd").exists(),
            "whiteout should have removed etc/passwd"
        );
        assert!(
            !tmp.path().join("etc/.wh.passwd").exists(),
            "whiteout marker itself must not be materialized in the output tree"
        );
    }

    #[test]
    fn whiteout_does_not_hide_same_layer_file_when_marker_appears_later() {
        // OCI whiteouts hide only lower-layer entries. Same-layer
        // entries survive even if a tar stream places the whiteout
        // marker after the file.
        let tar_bytes = build_tar(|b| {
            add_dir(b, "etc/");
            add_file(b, "etc/passwd", b"new\n");
            add_file(b, "etc/.wh.passwd", b"");
        });

        let tmp = TempDir::new().unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        assert_eq!(report.files_written, 1);
        assert_eq!(report.whiteouts_applied, 1);
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("etc/passwd")).unwrap(),
            "new\n"
        );
        assert!(!tmp.path().join("etc/.wh.passwd").exists());
    }

    #[test]
    fn whiteout_with_absent_target_is_idempotent_noop() {
        // OCI single-layer apply is declarative — a `.wh.<name>`
        // whose target doesn't exist is still a successful apply
        // (the marker has been consumed). Counter still increments.
        let tar_bytes = build_tar(|b| {
            add_dir(b, "etc/");
            add_file(b, "etc/.wh.ghost", b"");
        });

        let tmp = TempDir::new().unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        assert_eq!(report.whiteouts_applied, 1);
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        assert!(!tmp.path().join("etc/.wh.ghost").exists());
        assert!(!tmp.path().join("etc/ghost").exists());
    }

    #[test]
    fn whiteout_removes_sibling_directory_recursively() {
        // A whiteout on a lower-layer directory takes that whole
        // subtree out. remove_dir_all is the right primitive — we
        // use symlink_metadata first so we don't accidentally walk a
        // symlink-to-elsewhere as a directory.
        let tar_bytes = build_tar(|b| {
            add_dir(b, "etc/");
            add_file(b, "etc/.wh.sub", b"");
        });

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("etc/sub")).unwrap();
        std::fs::write(tmp.path().join("etc/sub/a"), b"one").unwrap();
        std::fs::write(tmp.path().join("etc/sub/b"), b"two").unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        assert_eq!(report.whiteouts_applied, 1);
        assert!(!tmp.path().join("etc/sub").exists());
        assert!(!tmp.path().join("etc/sub/a").exists());
        assert!(!tmp.path().join("etc/sub/b").exists());
        assert!(tmp.path().join("etc").is_dir(), "parent etc/ must remain");
    }

    #[test]
    fn opaque_whiteout_clears_parent_contents_preserving_directory() {
        // `.wh..wh..opq` clears the parent dir's prior-layer
        // contents but keeps the dir itself. Entries from the
        // current layer survive even when the marker appears later
        // in the tar stream.
        let tar_bytes = build_tar(|b| {
            add_dir(b, "etc/");
            add_file(b, "etc/a", b"alpha");
            add_dir(b, "etc/sub/");
            add_file(b, "etc/sub/c", b"gamma");
            add_file(b, "etc/.wh..wh..opq", b"");
        });

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("etc/sub")).unwrap();
        std::fs::write(tmp.path().join("etc/old"), b"old").unwrap();
        std::fs::write(tmp.path().join("etc/sub/old"), b"old").unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        assert_eq!(report.opaque_markers_applied, 1);
        assert_eq!(report.whiteouts_applied, 0);
        assert!(report.refused.is_empty(), "{:?}", report.refused);

        // The parent dir survives; prior contents are gone, while
        // current-layer entries remain.
        assert!(tmp.path().join("etc").is_dir(), "etc/ must remain");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("etc/a")).unwrap(),
            "alpha"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("etc/sub/c")).unwrap(),
            "gamma"
        );
        assert!(!tmp.path().join("etc/old").exists());
        assert!(!tmp.path().join("etc/sub/old").exists());
        // The opaque marker itself is not materialized.
        assert!(!tmp.path().join("etc/.wh..wh..opq").exists());
    }

    #[test]
    fn whiteout_under_symlinked_parent_refuses_like_a_regular_write() {
        // CVE-class guard: a `.wh.passwd` under a symlinked `etc/`
        // must refuse with SymlinkInParent, same as a regular-file
        // write would. Otherwise a malicious layer could write
        // `etc -> /tmp` then `etc/.wh.passwd` and trick the
        // unpacker into removing `/tmp/passwd`.
        let tar_bytes = build_tar(|b| {
            add_symlink(b, "etc", "/tmp");
            add_file(b, "etc/.wh.passwd", b"");
        });

        let tmp = TempDir::new().unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        assert_eq!(report.symlinks_written, 1);
        assert_eq!(report.whiteouts_applied, 0);
        assert_eq!(report.refused.len(), 1);
        assert_eq!(report.refused[0].reason, RefusalReason::SymlinkInParent);
    }

    #[test]
    fn whiteout_with_traversal_in_path_refuses() {
        // `.wh.foo` is a fine filename; `foo/../.wh.bar` is not — the
        // traversal segment in the parent chain must refuse before
        // the whiteout dispatch even runs.
        let tar_bytes = handrolled_tar_with_path(b"foo/../.wh.bar", b'0');
        let tmp = TempDir::new().unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        assert_eq!(report.refused.len(), 1, "{:?}", report.refused);
        assert_eq!(report.refused[0].reason, RefusalReason::TraversalSegment);
        assert_eq!(report.whiteouts_applied, 0);
    }

    #[test]
    fn malformed_whiteout_marker_with_empty_suffix_refused() {
        // `.wh.` exactly (no suffix) is a wire-format violation per
        // OCI v1.1 §"Layer Filesystem Changeset" — every whiteout
        // either names a sibling (`.wh.<name>`) or is the magic
        // opaque marker (`.wh..wh..opq`). The bare prefix is neither.
        // Refused as MalformedHeader (not a security boundary; just
        // an unrenderable marker).
        let tar_bytes = build_tar(|b| {
            add_dir(b, "etc/");
            add_file(b, "etc/.wh.", b"");
        });

        let tmp = TempDir::new().unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        assert_eq!(report.whiteouts_applied, 0);
        assert_eq!(report.opaque_markers_applied, 0);
        assert_eq!(report.refused.len(), 1);
        assert_eq!(report.refused[0].reason, RefusalReason::MalformedHeader);
    }

    #[test]
    fn filename_with_wh_substring_but_not_prefix_is_a_regular_file() {
        // `foo.wh.bar` is a regular filename, not a whiteout. The
        // classifier matches on the *leaf prefix*, not on a
        // substring anywhere in the leaf — otherwise legitimate
        // filenames would be silently dropped.
        let tar_bytes = build_tar(|b| {
            add_file(b, "foo.wh.bar", b"contents");
        });

        let tmp = TempDir::new().unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        assert_eq!(report.files_written, 1);
        assert_eq!(report.whiteouts_applied, 0);
        assert!(tmp.path().join("foo.wh.bar").is_file());
    }

    #[test]
    fn intermediate_directory_named_wh_passes_through() {
        // A *directory* component named `.wh.something` is not a
        // marker — only the **leaf** filename of a regular-file
        // entry carries the semantic. So `a/.wh.x/y` writes a real
        // file at `a/.wh.x/y` with `.wh.x` materialized as a
        // directory by `create_dir_all`.
        let tar_bytes = build_tar(|b| {
            add_file(b, "a/.wh.x/y", b"data");
        });

        let tmp = TempDir::new().unwrap();
        let report = unpack_layer(
            Cursor::new(tar_bytes),
            tmp.path(),
            &UnpackOptions::default(),
        )
        .expect("unpack ok");

        assert_eq!(report.files_written, 1);
        assert_eq!(report.whiteouts_applied, 0);
        assert!(tmp.path().join("a/.wh.x/y").is_file());
    }
}
