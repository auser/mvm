//! In-memory cpio newc archive builder.
//!
//! Pure-Rust, zero-dep. Emits the "new ASCII" (`070701`) cpio format
//! the Linux kernel decompresses into the initramfs at boot. Used by
//! the bootstrap path that builds a Stage 0 initramfs from embedded
//! static binaries + a shell init script.
//!
//! Format reference: <https://www.kernel.org/doc/html/latest/driver-api/early-userspace/buffer-format.html>
//!
//! Each entry is a 110-byte ASCII header, then the NUL-terminated
//! filename, then the file data; both padded to 4-byte boundaries.
//! Archive ends with a `TRAILER!!!` entry.
//!
//! Only the file types we actually need are supported: regular
//! files, directories, and symlinks. Special files (block/char
//! devices, FIFOs, sockets) are out of scope.
//!
//! All entries get `nlink=1`, `mtime=0`, `uid=0`, `gid=0`, no devnum,
//! no check. Reproducible — the same input list produces the same
//! bytes.

use std::io::Write;

const MAGIC: &str = "070701";
const TRAILER_NAME: &str = "TRAILER!!!";

const MODE_FILE: u32 = 0o100000;
const MODE_DIR: u32 = 0o040000;
const MODE_SYMLINK: u32 = 0o120000;

/// Permission bits for a regular file or directory in the archive.
/// Defaults: `0o755` for executables/directories, `0o644` for data.
#[derive(Debug, Clone, Copy)]
pub struct Perm(pub u32);

impl Perm {
    pub const FILE_644: Perm = Perm(0o644);
    pub const FILE_755: Perm = Perm(0o755);
    pub const DIR_755: Perm = Perm(0o755);
    pub const SYMLINK: Perm = Perm(0o777);
}

/// A single archive entry. `path` is the absolute in-archive path
/// (e.g. `/init`, `/bin/busybox`). Leading slash is mandatory and
/// stripped by [`CpioArchive::write_to`] before emission (cpio paths
/// are always relative to the archive root).
#[derive(Debug, Clone)]
pub enum CpioEntry {
    Dir {
        path: String,
        perm: Perm,
    },
    File {
        path: String,
        perm: Perm,
        data: Vec<u8>,
    },
    Symlink {
        path: String,
        target: String,
    },
}

impl CpioEntry {
    fn path(&self) -> &str {
        match self {
            Self::Dir { path, .. } | Self::File { path, .. } | Self::Symlink { path, .. } => path,
        }
    }
}

/// Builder for a cpio newc archive.
#[derive(Debug, Default)]
pub struct CpioArchive {
    entries: Vec<CpioEntry>,
}

impl CpioArchive {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dir(&mut self, path: impl Into<String>, perm: Perm) -> &mut Self {
        self.entries.push(CpioEntry::Dir {
            path: path.into(),
            perm,
        });
        self
    }

    pub fn file(
        &mut self,
        path: impl Into<String>,
        perm: Perm,
        data: impl Into<Vec<u8>>,
    ) -> &mut Self {
        self.entries.push(CpioEntry::File {
            path: path.into(),
            perm,
            data: data.into(),
        });
        self
    }

    pub fn symlink(&mut self, path: impl Into<String>, target: impl Into<String>) -> &mut Self {
        self.entries.push(CpioEntry::Symlink {
            path: path.into(),
            target: target.into(),
        });
        self
    }

    /// Number of entries currently queued (excluding the trailer).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialize the archive into `out`. Returns the total byte count
    /// written.
    pub fn write_to<W: Write>(&self, out: &mut W) -> std::io::Result<usize> {
        let mut written = 0;
        for (idx, entry) in self.entries.iter().enumerate() {
            // `ino` is opaque but conventionally monotonic from 1.
            // 0 is reserved.
            let ino = (idx + 1) as u32;
            written += write_entry(out, ino, entry)?;
        }
        // Trailer: empty data, mode 0, nlink 1.
        let trailer = CpioEntry::File {
            path: format!("/{TRAILER_NAME}"),
            perm: Perm(0),
            data: Vec::new(),
        };
        written += write_entry(out, 0, &trailer)?;
        Ok(written)
    }

    /// Convenience: serialize to a `Vec<u8>`.
    pub fn into_bytes(self) -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.write_to(&mut buf)?;
        Ok(buf)
    }
}

fn write_entry<W: Write>(out: &mut W, ino: u32, entry: &CpioEntry) -> std::io::Result<usize> {
    let raw_path = entry.path();
    // cpio paths are stored without the leading `/`. The trailer is
    // emitted as `/TRAILER!!!` so we get the same stripping logic.
    let stored_name = raw_path.strip_prefix('/').unwrap_or(raw_path);
    let (mode_bits, file_data): (u32, &[u8]) = match entry {
        CpioEntry::Dir { perm, .. } => (MODE_DIR | perm.0, &[]),
        CpioEntry::File { perm, data, .. } => (MODE_FILE | perm.0, data.as_slice()),
        CpioEntry::Symlink { target, .. } => (MODE_SYMLINK | Perm::SYMLINK.0, target.as_bytes()),
    };
    let namesize = stored_name.len() + 1; // include trailing NUL
    let filesize = file_data.len();

    // 6-byte magic + 13 fields × 8 ASCII hex digits = 6 + 104 = 110 bytes.
    let header = format!(
        "{magic}{ino:08x}{mode:08x}{uid:08x}{gid:08x}{nlink:08x}\
         {mtime:08x}{filesize:08x}{devmajor:08x}{devminor:08x}{rdevmajor:08x}\
         {rdevminor:08x}{namesize:08x}{check:08x}",
        magic = MAGIC,
        ino = ino,
        mode = mode_bits,
        uid = 0u32,
        gid = 0u32,
        nlink = 1u32,
        mtime = 0u32,
        filesize = filesize as u32,
        devmajor = 0u32,
        devminor = 0u32,
        rdevmajor = 0u32,
        rdevminor = 0u32,
        namesize = namesize as u32,
        check = 0u32,
    );
    debug_assert_eq!(header.len(), 110);

    out.write_all(header.as_bytes())?;
    let mut written = header.len();

    out.write_all(stored_name.as_bytes())?;
    out.write_all(&[0u8])?; // NUL terminator
    written += namesize;
    written += pad_to_4(out, written)?;

    out.write_all(file_data)?;
    written += filesize;
    written += pad_to_4(out, written)?;

    Ok(written)
}

/// Pad `out` with NULs so total bytes written so far is 4-aligned.
fn pad_to_4<W: Write>(out: &mut W, written: usize) -> std::io::Result<usize> {
    let rem = written % 4;
    if rem == 0 {
        return Ok(0);
    }
    let pad = 4 - rem;
    out.write_all(&[0u8; 3][..pad])?;
    Ok(pad)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_field(bytes: &[u8], offset: usize) -> u32 {
        let s = std::str::from_utf8(&bytes[offset..offset + 8]).unwrap();
        u32::from_str_radix(s, 16).unwrap()
    }

    #[test]
    fn header_is_110_bytes() {
        let mut arc = CpioArchive::new();
        arc.file("/x", Perm::FILE_644, b"");
        let bytes = arc.into_bytes().unwrap();
        // 6-byte magic + 13 × 8 hex digits.
        assert_eq!(&bytes[..6], b"070701");
        // Header ends at byte 110; payload starts there.
        // We can't assert the exact total because of padding, but
        // we can verify the first entry's header reads cleanly.
        let mode = parse_field(&bytes, 6 + 8);
        assert_eq!(mode & MODE_FILE, MODE_FILE);
    }

    #[test]
    fn entries_and_trailer_present() {
        let mut arc = CpioArchive::new();
        arc.dir("/dir", Perm::DIR_755);
        arc.file("/file", Perm::FILE_755, b"hello");
        arc.symlink("/link", "/file");
        let bytes = arc.into_bytes().unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("dir"), "dir entry present");
        assert!(s.contains("file"), "file entry present");
        assert!(s.contains("link"), "symlink entry present");
        assert!(s.contains("TRAILER!!!"), "trailer entry present");
    }

    #[test]
    fn file_data_is_preserved() {
        let payload = b"hello world\n";
        let mut arc = CpioArchive::new();
        arc.file("/greeting", Perm::FILE_644, payload.to_vec());
        let bytes = arc.into_bytes().unwrap();
        // Find the payload bytes verbatim in the archive.
        assert!(
            bytes.windows(payload.len()).any(|w| w == payload),
            "payload bytes recoverable from serialized archive"
        );
    }

    #[test]
    fn symlink_target_stored_as_payload() {
        let mut arc = CpioArchive::new();
        arc.symlink("/sbin/sh", "/bin/busybox");
        let bytes = arc.into_bytes().unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains("/bin/busybox"),
            "symlink target stored verbatim in file-data slot"
        );
    }

    #[test]
    fn entries_are_4byte_aligned() {
        let mut arc = CpioArchive::new();
        // Names of various lengths to exercise both header and data padding.
        arc.file("/a", Perm::FILE_644, b"x");
        arc.file("/abc", Perm::FILE_644, b"xy");
        arc.file("/abcd", Perm::FILE_644, b"xyzw");
        arc.file("/abcde", Perm::FILE_644, b"xyzw1");
        let bytes = arc.into_bytes().unwrap();
        // Each header starts on a 4-byte boundary. We can scan for
        // the magic and assert positions.
        let mut pos = 0;
        let mut found = 0;
        while pos + 6 <= bytes.len() {
            if &bytes[pos..pos + 6] == b"070701" {
                assert_eq!(pos % 4, 0, "header at {pos} not 4-aligned");
                found += 1;
                pos += 1;
            } else {
                pos += 1;
            }
        }
        // 4 file entries + 1 trailer = 5 headers.
        assert_eq!(found, 5, "all entry headers + trailer found");
    }

    #[test]
    fn empty_archive_has_trailer_only() {
        let arc = CpioArchive::new();
        let bytes = arc.into_bytes().unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("TRAILER!!!"));
        // Exactly one header (the trailer) + name + padding.
        assert_eq!(&bytes[..6], b"070701");
    }

    #[test]
    fn dir_entry_sets_dir_mode_bits() {
        let mut arc = CpioArchive::new();
        arc.dir("/tmp", Perm::DIR_755);
        let bytes = arc.into_bytes().unwrap();
        let mode = parse_field(&bytes, 6 + 8);
        assert_eq!(mode & 0o170000, MODE_DIR, "S_IFDIR set");
        assert_eq!(mode & 0o7777, 0o755);
    }

    #[test]
    fn symlink_entry_sets_lnk_mode_bits() {
        let mut arc = CpioArchive::new();
        arc.symlink("/sh", "/bin/busybox");
        let bytes = arc.into_bytes().unwrap();
        let mode = parse_field(&bytes, 6 + 8);
        assert_eq!(mode & 0o170000, MODE_SYMLINK, "S_IFLNK set");
    }

    #[test]
    fn entry_count_tracks_pushes() {
        let mut arc = CpioArchive::new();
        assert!(arc.is_empty());
        arc.dir("/d", Perm::DIR_755);
        assert_eq!(arc.len(), 1);
        arc.file("/f", Perm::FILE_644, b"");
        assert_eq!(arc.len(), 2);
    }
}
