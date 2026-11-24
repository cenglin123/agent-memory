//! Cheap change signals for the JSONL / DSH index watermarks.
//!
//! Three independent facts about one carrier, each bounded by a constant, never
//! proportional to the file size:
//!
//! * **head probe** — sha256 of the first [`PROBE_WINDOW`] bytes of the indexed
//!   prefix;
//! * **anchor probe** — sha256 of the [`PROBE_WINDOW`] bytes ending at the
//!   committed offset;
//! * **file identity** — Windows volume serial number + file id
//!   (`GetFileInformationByHandle`), empty on another platform or when the call
//!   fails.
//!
//! They answer "is the recorded prefix still the file's prefix?" and "is this
//! still the same file at the same path?" without reading a body. Size and
//! timestamp stay the first signals; the probes are only consulted when those
//! two disagree, so an unchanged file costs two `stat` fields and no read.
//!
//! # Blind spot (declared, bounded)
//!
//! Detection is bounded verification, the same discipline `capture_jsonl`
//! documents: **the middle of a carrier is never probed — whatever the length**
//! (probing it would cost the file size on every round). A rewrite that keeps
//! the first `PROBE_WINDOW` bytes, the `PROBE_WINDOW` bytes ending at the
//! committed offset and the file identity is indistinguishable from an unchanged
//! file when it lands in the middle; at an equal length that is the whole blind
//! spot, and with growth the middle below the committed offset is equally
//! unprotected. The bytes **after** the committed offset are not covered by any
//! window either: an uncommitted trailing fragment is re-read only once its
//! mtime moves (see `fulltext::build::decide_jsonl`). The recovery path stays
//! `agent-memory index --rebuild`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::recall::hash::sha256_hex;

/// Probe window: the head of the prefix and the window ending at the committed
/// offset (8192 bytes, the same window `capture_jsonl` uses).
pub const PROBE_WINDOW: u64 = 8192;

/// Read `[start, start + len)` without going past EOF.
fn read_window(file: &mut File, start: u64, len: u64) -> std::io::Result<Vec<u8>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = vec![0u8; len as usize];
    let mut filled = 0usize;
    while filled < buffer.len() {
        let read = file.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buffer.truncate(filled);
    Ok(buffer)
}

/// `(head, anchor)` sha256 digests of the committed prefix `[0, offset)`.
///
/// `None` when the file cannot be read; the caller then treats the prefix as
/// unverified (it never resumes on bytes it could not check).
pub fn probes(path: &Path, offset: u64) -> Option<(String, String)> {
    let window = offset.min(PROBE_WINDOW);
    let mut file = File::open(path).ok()?;
    let head = sha256_hex(&read_window(&mut file, 0, window).ok()?);
    let anchor = sha256_hex(&read_window(&mut file, offset - window, window).ok()?);
    crate::cost::counter("index.probe_reads", 1);
    crate::cost::counter("index.probe_bytes", window.saturating_mul(2));
    Some((head, anchor))
}

/// Windows file identity: `"<volume serial>:<file index high><file index low>"`,
/// or `None` on another platform and when the handle cannot be queried.
///
/// A replacement (rotation, copy-then-rename, a tool that writes a new file at
/// the same path) gets a new file index, so "same size, same mtime" stops
/// meaning "same file".
pub fn file_identity(path: &Path) -> Option<String> {
    if identity_is_unavailable() {
        return None;
    }
    identity_of(path)
}

thread_local! {
    /// Test seam: pretend the handle query failed on this thread. Production
    /// never sets it.
    static IDENTITY_UNAVAILABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn identity_is_unavailable() -> bool {
    IDENTITY_UNAVAILABLE.with(|flag| flag.get())
}

/// Install (or clear) the test seam that makes [`file_identity`] answer `None` on
/// the current thread, as a transient handle failure or an unsupported
/// filesystem would. A single failing round must never erase an identity that
/// was already recorded.
pub fn set_identity_unavailable(unavailable: bool) {
    IDENTITY_UNAVAILABLE.with(|flag| flag.set(unavailable));
}

#[cfg(windows)]
fn identity_of(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;

    // Hand-written declarations: the crate takes no extra dependency for three
    // `kernel32` calls (std already links `kernel32`).
    type Handle = isize;

    #[repr(C)]
    #[derive(Default)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time_low: u32,
        creation_time_high: u32,
        last_access_time_low: u32,
        last_access_time_high: u32,
        last_write_time_low: u32,
        last_write_time_high: u32,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut core::ffi::c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: Handle,
        ) -> Handle;
        fn GetFileInformationByHandle(
            file: Handle,
            information: *mut ByHandleFileInformation,
        ) -> i32;
        fn CloseHandle(object: Handle) -> i32;
    }

    const GENERIC_READ: u32 = 0x8000_0000;
    const SHARE_READ_WRITE_DELETE: u32 = 0x0000_0007;
    const OPEN_EXISTING: u32 = 3;
    // Required for directories, harmless for files; keeps the helper usable for
    // any path the caller has.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const INVALID_HANDLE_VALUE: Handle = -1;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call; the
    // security attributes are absent (null) and the template handle is unused
    // for OPEN_EXISTING, so the only contract is the documented argument types.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            SHARE_READ_WRITE_DELETE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle == 0 {
        return None;
    }
    let mut information = ByHandleFileInformation::default();
    // SAFETY: `handle` is a handle this function opened and `information` is a
    // valid, writable `BY_HANDLE_FILE_INFORMATION` buffer.
    let ok = unsafe { GetFileInformationByHandle(handle, &mut information) };
    // SAFETY: the handle is owned by this call and closed exactly once.
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        return None;
    }
    if information.volume_serial_number == 0
        && information.file_index_high == 0
        && information.file_index_low == 0
    {
        // The API answers "not supported" this way (some filesystems and network
        // shares): an all-zero identity is not a claim, so the caller falls back
        // to size + timestamp and never rescan on it.
        return None;
    }
    Some(format!(
        "{:08x}:{:08x}{:08x}",
        information.volume_serial_number, information.file_index_high, information.file_index_low
    ))
}

#[cfg(not(windows))]
fn identity_of(_path: &Path) -> Option<String> {
    // Documented fallback: size + timestamp only (the pre-004a behaviour).
    None
}
