//! Mid-match save file format (M1 of save/resume — see `docs/analysis/save-format.md`).
//!
//! A save holds a full simulation snapshot (engine ranges + plugin blobs, serialized by
//! `snapshot::Snapshot::serialize`) and, for live matches, the rollback session state
//! (`session::Session::save_state`). Both payloads are opaque byte blobs here so this
//! module stays game-independent and unit-testable without the game running.
//!
//! File layout (little-endian):
//!
//! ```text
//! 0x00  b"CMRB"              magic
//! 0x04  u32                  version (= SAVE_VERSION)
//! 0x08  u32                  frame  (frames completed by the saved state)
//! 0x0c  u32                  flags  (FLAG_LIVE | FLAG_HARNESS)
//! 0x10  u32                  snapshot_len
//! 0x14  snapshot bytes       (snapshot::Snapshot::serialize)
//!        u32                  session_len (0 = no session section)
//!        session bytes        (session::Session::save_state)
//!        u32                  checksum: FNV-1a 32 over every byte before it
//! ```
//!
//! `read_save` verifies the magic, the version and the checksum before returning any
//! payload, so a truncated or corrupted file fails loudly instead of restoring garbage.
//! Cross-build validation (range table id, plugin ABI versions) happens inside
//! `Snapshot::deserialize`, which compares the serialized range description against the
//! running build's table.

#![allow(dead_code)]

use std::path::Path;

pub const MAGIC: &[u8; 4] = b"CMRB";
pub const SAVE_VERSION: u32 = 1;
/// A save of a live rollback session (has a session section).
pub const FLAG_LIVE: u32 = 1;
/// A save from the replay harness / single-player (no peer session).
pub const FLAG_HARNESS: u32 = 2;

// ---- little-endian byte writer / reader -------------------------------------------

/// Growable little-endian byte writer with length-prefixed byte blobs.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn flag(&mut self, b: bool) {
        self.buf.push(b as u8);
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.buf.extend_from_slice(b);
    }

    pub fn opt_u32(&mut self, o: Option<u32>) {
        self.flag(o.is_some());
        if let Some(v) = o {
            self.u32(v);
        }
    }

    pub fn opt_u64(&mut self, o: Option<u64>) {
        self.flag(o.is_some());
        if let Some(v) = o {
            self.u64(v);
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Bounds-checked reader over a byte slice. Every read fails with `"truncated"` when the
/// buffer runs out, so a corrupt save can never be mis-parsed into game memory.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        let end = self.pos.checked_add(n).ok_or("truncated")?;
        if end > self.buf.len() {
            return Err("truncated");
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub fn u32(&mut self) -> Result<u32, &'static str> {
        let b: [u8; 4] = self.take(4)?.try_into().unwrap();
        Ok(u32::from_le_bytes(b))
    }

    pub fn u64(&mut self) -> Result<u64, &'static str> {
        let b: [u8; 8] = self.take(8)?.try_into().unwrap();
        Ok(u64::from_le_bytes(b))
    }

    pub fn flag(&mut self) -> Result<bool, &'static str> {
        let b = self.take(1)?;
        match b[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("bad flag"),
        }
    }

    pub fn bytes(&mut self) -> Result<&'a [u8], &'static str> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    pub fn opt_u32(&mut self) -> Result<Option<u32>, &'static str> {
        Ok(if self.flag()? { Some(self.u32()?) } else { None })
    }

    pub fn opt_u64(&mut self) -> Result<Option<u64>, &'static str> {
        Ok(if self.flag()? { Some(self.u64()?) } else { None })
    }

    /// The reader must have consumed the buffer exactly; trailing bytes mean the file is
    /// from a different format version than the parser.
    pub fn finish(&self) -> Result<(), &'static str> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err("trailing bytes")
        }
    }
}

// ---- checksum ----------------------------------------------------------------------

fn fnv1a(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ---- file format -------------------------------------------------------------------

/// Write a save file. `snapshot` and `session` are the opaque payloads produced by
/// `Snapshot::serialize` / `Session::save_state` (either may be empty). Returns the
/// number of bytes written.
pub fn write_save(
    path: &Path,
    frame: u32,
    flags: u32,
    snapshot: &[u8],
    session: &[u8],
) -> Result<usize, String> {
    let mut w = Writer::new();
    w.buf.extend_from_slice(MAGIC);
    w.u32(SAVE_VERSION);
    w.u32(frame);
    w.u32(flags);
    w.bytes(snapshot);
    w.bytes(session);
    let checksum = fnv1a(&w.buf);
    w.u32(checksum);
    let n = w.buf.len();
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {dir:?}: {e}"))?;
        }
    }
    std::fs::write(path, &w.buf).map_err(|e| format!("write {path:?}: {e}"))?;
    Ok(n)
}

/// Read a save file back, verifying magic, version and checksum. Returns
/// `(frame, flags, snapshot bytes, session bytes)`.
pub fn read_save(path: &Path) -> Result<(u32, u32, Vec<u8>, Vec<u8>), String> {
    let buf = std::fs::read(path).map_err(|e| format!("read {path:?}: {e}"))?;
    if buf.len() < 4 + 16 + 4 {
        return Err("file too small to be a save".into());
    }
    if &buf[..4] != MAGIC {
        return Err("not a cosmo save (bad magic)".into());
    }
    let (body, checksum) = buf.split_at(buf.len() - 4);
    let want = u32::from_le_bytes(checksum.try_into().unwrap());
    if fnv1a(body) != want {
        return Err("checksum mismatch (corrupted save)".into());
    }
    // The reader starts after the magic (verified above), so it parses version..session.
    let mut r = Reader::new(&body[4..]);
    let version = r.u32().map_err(|e| e.to_string())?;
    if version != SAVE_VERSION {
        return Err(format!("save version {version}, expected {SAVE_VERSION}"));
    }
    let frame = r.u32().map_err(|e| e.to_string())?;
    let flags = r.u32().map_err(|e| e.to_string())?;
    let snapshot = r.bytes().map_err(|e| e.to_string())?.to_vec();
    let session = r.bytes().map_err(|e| e.to_string())?.to_vec();
    r.finish().map_err(|e| e.to_string())?;
    Ok((frame, flags, snapshot, session))
}

/// Keep only the `keep` newest `autosave_<frame>.cosmo` files in `dir` (newest = highest
/// frame number). No-op when `keep` is 0 or the directory does not exist.
pub fn prune_autosaves(dir: &str, keep: usize) {
    if keep == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut saves: Vec<(u32, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name.strip_prefix("autosave_")?.strip_suffix(".cosmo")?;
            Some((stem.parse().ok()?, e.path()))
        })
        .collect();
    if saves.len() <= keep {
        return;
    }
    saves.sort_by_key(|(f, _)| std::cmp::Reverse(*f));
    for (_, path) in saves.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cosmo_save_test_{}_{}", std::process::id(), name));
        p
    }

    #[test]
    fn writer_reader_round_trips() {
        let mut w = Writer::new();
        w.u32(0x1234_5678);
        w.u64(0x0102_0304_0506_0708);
        w.flag(true);
        w.bytes(&[1, 2, 3, 4]);
        w.opt_u32(Some(9));
        w.opt_u32(None);
        w.opt_u64(Some(0xf00d));
        let bytes = w.into_bytes();

        let mut r = Reader::new(&bytes);
        assert_eq!(r.u32(), Ok(0x1234_5678));
        assert_eq!(r.u64(), Ok(0x0102_0304_0506_0708));
        assert_eq!(r.flag(), Ok(true));
        assert_eq!(r.bytes(), Ok(&[1, 2, 3, 4][..]));
        assert_eq!(r.opt_u32(), Ok(Some(9)));
        assert_eq!(r.opt_u32(), Ok(None));
        assert_eq!(r.opt_u64(), Ok(Some(0xf00d)));
        assert_eq!(r.finish(), Ok(()));
    }

    #[test]
    fn truncated_reader_fails_loudly() {
        let mut w = Writer::new();
        w.u32(7);
        w.bytes(&[0xaa; 10]);
        let bytes = w.into_bytes();
        // Cut the blob short: the reader must refuse, not read garbage.
        let mut r = Reader::new(&bytes[..bytes.len() - 3]);
        assert_eq!(r.u32(), Ok(7));
        assert_eq!(r.bytes(), Err("truncated"));
    }

    #[test]
    fn save_file_round_trips_to_disk() {
        let path = temp_path("roundtrip.cosmo");
        let snap = vec![0x11u8; 64];
        let sess = vec![0x22u8; 32];
        let n = write_save(&path, 1234, FLAG_LIVE, &snap, &sess).unwrap();
        assert!(n > 64 + 32);
        let (frame, flags, snap2, sess2) = read_save(&path).unwrap();
        assert_eq!(frame, 1234);
        assert_eq!(flags, FLAG_LIVE);
        assert_eq!(snap2, snap);
        assert_eq!(sess2, sess);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_file_accepts_an_empty_session() {
        let path = temp_path("nosession.cosmo");
        write_save(&path, 7, FLAG_HARNESS, b"snap", b"").unwrap();
        let (frame, flags, snap, sess) = read_save(&path).unwrap();
        assert_eq!((frame, flags), (7, FLAG_HARNESS));
        assert_eq!(snap, b"snap");
        assert!(sess.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupted_save_is_rejected() {
        let path = temp_path("corrupt.cosmo");
        write_save(&path, 1, FLAG_LIVE, b"payload", b"").unwrap();
        let mut buf = std::fs::read(&path).unwrap();
        let mid = buf.len() / 2;
        buf[mid] ^= 0xff;
        std::fs::write(&path, &buf).unwrap();
        assert!(read_save(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wrong_magic_is_rejected() {
        let path = temp_path("magic.cosmo");
        write_save(&path, 1, FLAG_LIVE, b"payload", b"").unwrap();
        let mut buf = std::fs::read(&path).unwrap();
        buf[0] = b'X';
        std::fs::write(&path, &buf).unwrap();
        assert!(read_save(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pruning_keeps_only_the_newest() {
        let path = temp_path("prune");
        std::fs::create_dir_all(&path).unwrap();
        for f in [10u32, 20, 30, 40] {
            let p = path.join(format!("autosave_{f}.cosmo"));
            std::fs::write(&p, b"x").unwrap();
        }
        // A stray non-autosave file must survive.
        std::fs::write(path.join("notes.txt"), b"keep me").unwrap();
        prune_autosaves(path.to_str().unwrap(), 2);
        let names: Vec<String> = std::fs::read_dir(&path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"autosave_30.cosmo".to_string()));
        assert!(names.contains(&"autosave_40.cosmo".to_string()));
        assert!(!names.contains(&"autosave_10.cosmo".to_string()));
        assert!(!names.contains(&"autosave_20.cosmo".to_string()));
        assert!(names.contains(&"notes.txt".to_string()));
        let _ = std::fs::remove_dir_all(&path);
    }
}
