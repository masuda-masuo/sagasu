//! NTFS USN Journal delta source (Windows only) — the fast half of design.md §5.
//!
//! Where the mtime fallback pays a full `stat` walk of the corpus to answer
//! "what changed since the marker", this asks NTFS directly: the journal is an
//! append-only log of every change to the volume, and a marker is simply a
//! position in it. The cost scales with the number of changes rather than with
//! the size of the tree, which is what makes the freshness merge affordable on a
//! multi-million-file volume.
//!
//! ## What it costs to use
//!
//! - **Administrator rights.** `FSCTL_QUERY_USN_JOURNAL` and
//!   `FSCTL_READ_USN_JOURNAL` need a handle to the raw volume. Without them
//!   [`UsnDeltaSource::for_config`] returns `None` and the caller silently falls
//!   back to the mtime walk — slower, not wrong.
//! - **Path resolution.** A journal record carries a *file name* and two file
//!   reference numbers (its own and its parent's), not a path. The full path is
//!   recovered by opening the **parent** FRN with `OpenFileById` and asking
//!   `GetFinalPathNameByHandleW`, then appending the record's name. Going
//!   through the parent rather than the record's own FRN is what makes a
//!   *deleted* file resolvable: the parent directory is normally still there.
//!   Parent lookups are memoised — a burst of changes almost always shares a
//!   handful of directories.
//! - **Noise.** The journal is volume-wide, so it reports the whole machine:
//!   telemetry (`.etl`), PowerShell temporaries, browser caches. The crawl's
//!   [`ExcludeSet`] and the crawl root are applied to every record, and what
//!   they drop is counted in [`DeltaSet::excluded`] (issue #16 measured 94% on a
//!   real machine).
//!
//! ## Marker invalidation (issue #16)
//!
//! The journal is a ring buffer sized by `MaximumSize`, and it is consumed fast
//! enough that an ordinary lunch break can outlive a marker. Three checks run
//! before any record is read, and each maps to [`RescanReason`]:
//!
//! 1. `UsnJournalID` differs from the marker's → the journal was recreated and
//!    the USN number space restarted, so number comparison would be nonsense.
//! 2. `marker.next_usn < journal.FirstUsn` → the marker's records rolled off.
//! 3. the read itself fails with `ERROR_JOURNAL_ENTRY_DELETED` (0x8007049D) →
//!    same conclusion, reported by the OS instead of derived.
//!
//! All three are [`DeltaStatus::RescanRequired`], which is a different branch
//! from hitting the delta cap: this one cannot be fixed by raising a limit.
//!
//! ## Verification status — this source is the Windows default
//!
//! Compiled for `x86_64-pc-windows-*`.  Verified on real NTFS hardware on
//! 2026-08-08 (issue #37): normal add/change/delete/rename deltas correct,
//! ~17× faster than the stat walk (19–25 ms vs 332–360 ms), silent fallback
//! confirmed for non-administrator and journal-absent cases.  The opt-in gate
//! has been removed — [`crate::delta::source_for`] now tries the USN source
//! first on Windows with no environment variable required.
//!
//! ## History — why verification was demanded
//!
//! The USN source was initially gated behind `SAGASU_DELTA_SOURCE=usn` because
//! it had never been exercised on real hardware: the development environment is
//! Linux, and CI only compiled it.  That gate existed because of what happened
//! without it. `sagasu index` canonicalizes its root, and
//! `std::fs::canonicalize` on Windows returns a `\\?\C:\…` verbatim path —
//! which [`volume_of`] rejected, so production never reached this source at all.
//! The single test that happened to pass a raw `C:\…` root did reach it, and
//! got an empty delta set back.  Both bugs are fixed, but "compiles and is
//! therefore the default on a whole platform" is exactly the reasoning that
//! produced a search silently returning nothing.  The source was verified on
//! real hardware on 2026-08-08 (issue #37) and the gate was removed.

use std::collections::HashMap;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_JOURNAL_ENTRY_DELETED, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFinalPathNameByHandleW, OpenFileById, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_ID_TYPE,
    FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
    USN_RECORD_V2,
};
use windows::Win32::System::IO::DeviceIoControl;

use crate::delta::{
    now_ns, DeltaConfig, DeltaEntry, DeltaSet, DeltaSource, DeltaSourceKind, DeltaStatus,
    RescanReason, ScanMarker,
};
use crate::walk::ExcludeSet;

/// Journal read buffer, in 8-byte words. 64 KiB per `DeviceIoControl` round.
const READ_BUF_WORDS: usize = 64 * 1024 / 8;

/// `USN_REASON_FILE_DELETE`.
const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;

/// Reasons that cannot change a file's content or name, and so cannot change a
/// search result. Filtering them here keeps the delta set (and therefore the
/// live grep) down to changes that matter.
const IGNORED_REASONS: u32 = 0x0000_0800 // USN_REASON_SECURITY_CHANGE
    | 0x0000_4000 // USN_REASON_INDEXABLE_CHANGE
    | 0x0002_0000; // USN_REASON_OBJECT_ID_CHANGE

/// A delta source backed by the USN Journal of the volume holding the root.
pub struct UsnDeltaSource {
    volume: String,
    root: PathBuf,
    excludes: ExcludeSet,
    skip_paths: Vec<PathBuf>,
}

impl UsnDeltaSource {
    /// Build a source for `config`, or `None` when the journal is not usable
    /// (no volume letter, not NTFS, journal disabled, or no administrator
    /// rights). The caller falls back to the mtime walk.
    pub fn for_config(config: &DeltaConfig) -> Option<Self> {
        let volume = volume_of(&config.root)?;
        // Compare records against the same shape of path the journal hands
        // back. `GetFinalPathNameByHandleW(FILE_NAME_NORMALIZED)` returns the
        // long form, so a root still holding an 8.3 short component (as
        // `%TEMP%` often does) would match nothing at all.
        let root = config
            .root
            .canonicalize()
            .map(|p| strip_extended_prefix(&p.to_string_lossy()))
            .unwrap_or_else(|_| config.root.clone());
        let source = Self {
            volume,
            root,
            excludes: config.excludes.clone(),
            skip_paths: config.skip_paths.clone(),
        };
        // Probing now, rather than at query time, is what makes the fallback a
        // configuration decision instead of a per-search surprise.
        source.with_volume(query_journal).ok()?;
        Some(source)
    }

    /// Open the volume, run `f`, and close the handle whatever `f` did.
    fn with_volume<T>(&self, f: impl FnOnce(HANDLE) -> Result<T>) -> Result<T> {
        let handle = open_volume(&self.volume)?;
        let out = f(handle);
        unsafe {
            let _ = CloseHandle(handle);
        }
        out
    }

    /// True when a resolved path belongs to the indexed set.
    ///
    /// The full crawl policy, not just the name list: the journal is
    /// volume-wide, so anything the crawl declined to index — a hidden tree, a
    /// gitignored build directory — arrives here too and must be dropped by the
    /// same rule that dropped it there.
    fn accepts(&self, path: &Path) -> bool {
        crate::delta::path_under(&self.root, path)
            && self.excludes.reason_for_path(path, &self.root).is_none()
            && !self
                .skip_paths
                .iter()
                .any(|p| crate::walk::same_path(p, path))
    }
}

impl DeltaSource for UsnDeltaSource {
    fn kind(&self) -> DeltaSourceKind {
        DeltaSourceKind::Usn
    }

    /// The journal carries `RENAME_OLD_NAME` / `RENAME_NEW_NAME` records, so a
    /// rename is an observable event rather than something to infer from
    /// timestamps.
    fn detects_renames(&self) -> bool {
        true
    }

    fn current_marker(&self) -> Result<ScanMarker> {
        let journal = self.with_volume(query_journal)?;
        Ok(ScanMarker::Usn {
            volume: self.volume.clone(),
            journal_id: journal.UsnJournalID,
            next_usn: journal.NextUsn,
            maximum_size: journal.MaximumSize,
            recorded_ns: now_ns(),
        })
    }

    fn changes_since(&self, marker: &ScanMarker, limit: usize) -> Result<DeltaSet> {
        let t0 = Instant::now();

        // A marker taken by a different source (or on a different volume) says
        // nothing about this journal. Reporting it rather than silently reading
        // from USN 0 keeps a wrong answer from looking like a fresh one.
        let ScanMarker::Usn {
            volume,
            journal_id,
            next_usn,
            ..
        } = marker
        else {
            return Ok(failed(marker, RescanReason::MarkerKindMismatch, t0));
        };
        if !volume.eq_ignore_ascii_case(&self.volume) {
            return Ok(failed(marker, RescanReason::MarkerKindMismatch, t0));
        }

        self.with_volume(|handle| {
            let journal = match query_journal(handle) {
                Ok(j) => j,
                Err(_) => return Ok(failed(marker, RescanReason::JournalUnavailable, t0)),
            };

            // (1) Journal identity before journal numbers: a recreated journal
            // restarts the USN space, so `next_usn` would compare "fine" while
            // pointing at unrelated records.
            if journal.UsnJournalID != *journal_id {
                return Ok(failed(marker, RescanReason::JournalIdMismatch, t0));
            }
            // (2) The marker's records rolled off the ring.
            if *next_usn < journal.FirstUsn {
                return Ok(failed(marker, RescanReason::MarkerExpired, t0));
            }

            self.read_range(handle, &journal, *next_usn, limit, marker, t0)
        })
    }
}

impl UsnDeltaSource {
    /// Read journal records from `start_usn` forward, resolving and filtering
    /// each one.
    fn read_range(
        &self,
        handle: HANDLE,
        journal: &USN_JOURNAL_DATA_V0,
        start_usn: i64,
        limit: usize,
        marker: &ScanMarker,
        t0: Instant,
    ) -> Result<DeltaSet> {
        let mut buf = vec![0u64; READ_BUF_WORDS];
        let mut cursor = start_usn;
        let mut parents: HashMap<u64, Option<PathBuf>> = HashMap::new();
        // Records are per-change, not per-file: one save produces several. The
        // delta set is a set.
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut entries: Vec<DeltaEntry> = Vec::new();
        let mut scanned: u64 = 0;
        let mut excluded: u64 = 0;
        let mut truncated = false;

        loop {
            let read_data = READ_USN_JOURNAL_DATA_V0 {
                StartUsn: cursor,
                ReasonMask: u32::MAX,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: journal.UsnJournalID,
            };
            let mut returned = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_READ_USN_JOURNAL,
                    Some(&read_data as *const _ as *const _),
                    size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                    Some(buf.as_mut_ptr() as *mut _),
                    (buf.len() * 8) as u32,
                    Some(&mut returned),
                    None,
                )
            };
            if let Err(e) = ok {
                // (3) The OS telling us the same thing check (2) derives.
                let expired = e.code().0 as u32 == ERROR_JOURNAL_ENTRY_DELETED.0;
                let reason = if expired {
                    RescanReason::JournalEntryDeleted
                } else {
                    RescanReason::JournalUnavailable
                };
                return Ok(failed(marker, reason, t0));
            }

            // The first 8 bytes are the USN to resume from; fewer than that (or
            // exactly that) means the journal has been drained.
            if returned <= 8 {
                break;
            }
            let bytes =
                unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, returned as usize) };
            let next_cursor = i64::from_le_bytes(bytes[0..8].try_into().unwrap());

            let mut offset = 8usize;
            while offset + size_of::<USN_RECORD_V2>() <= returned as usize {
                // The buffer is 8-byte aligned and every record starts on an
                // 8-byte boundary, so this reference is well-aligned.
                let rec = unsafe { &*(bytes.as_ptr().add(offset) as *const USN_RECORD_V2) };
                if rec.RecordLength == 0 {
                    break;
                }
                offset += rec.RecordLength as usize;
                scanned += 1;

                if rec.Reason & !IGNORED_REASONS == 0 {
                    continue;
                }

                let name_off = rec.FileNameOffset as usize;
                let name_len = rec.FileNameLength as usize / 2;
                let rec_start = offset - rec.RecordLength as usize;
                let name = unsafe {
                    let ptr = bytes.as_ptr().add(rec_start + name_off) as *const u16;
                    String::from_utf16_lossy(std::slice::from_raw_parts(ptr, name_len))
                };

                let Some(dir) = resolve_parent(handle, rec.ParentFileReferenceNumber, &mut parents)
                else {
                    excluded += 1;
                    continue;
                };
                let path = dir.join(&name);
                if !self.accepts(&path) {
                    excluded += 1;
                    continue;
                }

                let path_str = path.to_string_lossy().into_owned();
                let deleted = rec.Reason & USN_REASON_FILE_DELETE != 0;
                // A later record wins: a file created and then deleted in the
                // range is deleted, and one deleted then recreated is live.
                let (size, mtime_ns, exists) = match std::fs::metadata(&path) {
                    Ok(m) if !deleted => (
                        m.len() as i64,
                        m.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos() as i64)
                            .unwrap_or(0),
                        true,
                    ),
                    _ => (0, 0, false),
                };

                match seen.get(&path_str) {
                    Some(&idx) => {
                        entries[idx].size = size;
                        entries[idx].mtime_ns = mtime_ns;
                        entries[idx].exists = exists;
                    }
                    None => {
                        if entries.len() >= limit {
                            truncated = true;
                            break;
                        }
                        seen.insert(path_str.clone(), entries.len());
                        entries.push(DeltaEntry {
                            path: path_str,
                            size,
                            mtime_ns,
                            exists,
                        });
                    }
                }
            }

            if truncated || next_cursor == cursor {
                break;
            }
            cursor = next_cursor;
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(DeltaSet {
            entries,
            status: if truncated {
                DeltaStatus::Truncated { limit }
            } else {
                DeltaStatus::Complete
            },
            kind: DeltaSourceKind::Usn,
            detects_renames: true,
            scanned,
            excluded,
            // The journal read either succeeds or fails as a whole; there is no
            // per-entry read to fail the way a directory walk has.
            errors: 0,
            error_samples: Vec::new(),
            elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
            marker: marker.clone(),
        })
    }
}

// ── Win32 plumbing ──────────────────────────────────────────────────────────

/// An empty delta set carrying a rescan reason.
fn failed(marker: &ScanMarker, reason: RescanReason, t0: Instant) -> DeltaSet {
    DeltaSet {
        entries: Vec::new(),
        status: DeltaStatus::RescanRequired(reason),
        kind: DeltaSourceKind::Usn,
        detects_renames: false,
        scanned: 0,
        excluded: 0,
        errors: 0,
        error_samples: Vec::new(),
        elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
        marker: marker.clone(),
    }
}

/// The volume specifier (`C:`) a path lives on, if it has a drive letter.
///
/// The `\\?\` extended-length prefix is stripped first. That is not a nicety:
/// `sagasu index` canonicalizes its root before storing it, and
/// `std::fs::canonicalize` on Windows *always* returns a `\\?\C:\…` verbatim
/// path — so a check that only looked at byte 0 rejected every root the product
/// actually passes, and the USN source could never be selected in production.
///
/// `\\?\UNC\server\share` and plain UNC paths keep returning `None`: there is no
/// local volume handle to open, so they take the mtime fallback.
fn volume_of(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        Some(s[..2].to_string())
    } else {
        None
    }
}

/// Open `\\.\C:` for the journal ioctls. Requires administrator rights.
fn open_volume(volume: &str) -> Result<HANDLE> {
    let path = format!(r"\\.\{}", volume.trim_end_matches(['\\', '/']));
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .with_context(|| format!("cannot open volume {path} (administrator rights required)"))
}

/// `FSCTL_QUERY_USN_JOURNAL`.
fn query_journal(handle: HANDLE) -> Result<USN_JOURNAL_DATA_V0> {
    let mut data = USN_JOURNAL_DATA_V0::default();
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            handle,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(&mut data as *mut _ as *mut _),
            size_of::<USN_JOURNAL_DATA_V0>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .context("FSCTL_QUERY_USN_JOURNAL failed (journal disabled, or not an NTFS volume)")?;
    Ok(data)
}

/// Resolve a parent directory's file reference number to a path, memoising both
/// successes and failures (a directory that cannot be opened will not become
/// openable within one delta query, and each attempt is a syscall).
fn resolve_parent(
    volume: HANDLE,
    frn: u64,
    cache: &mut HashMap<u64, Option<PathBuf>>,
) -> Option<PathBuf> {
    if let Some(hit) = cache.get(&frn) {
        return hit.clone();
    }
    let resolved = open_by_id(volume, frn).and_then(|h| {
        let path = final_path(h);
        unsafe {
            let _ = CloseHandle(h);
        }
        path
    });
    cache.insert(frn, resolved.clone());
    resolved
}

/// `OpenFileById` for a 64-bit NTFS file reference number.
///
/// `FILE_FLAG_BACKUP_SEMANTICS` is required to open a *directory* handle, which
/// is what every parent FRN is.
fn open_by_id(volume: HANDLE, frn: u64) -> Option<HANDLE> {
    let descriptor = FILE_ID_DESCRIPTOR {
        dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: FILE_ID_TYPE(0), // FileIdType
        Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: frn as i64 },
    };
    unsafe {
        OpenFileById(
            volume,
            &descriptor,
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    }
    .ok()
}

/// `GetFinalPathNameByHandleW`, stripped of the `\\?\` prefix so the result
/// compares equal to the paths the crawler stored.
fn final_path(handle: HANDLE) -> Option<PathBuf> {
    let mut buf = vec![0u16; 1024];
    let len = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, FILE_NAME_NORMALIZED) };
    if len == 0 {
        return None;
    }
    if len as usize > buf.len() {
        buf = vec![0u16; len as usize + 1];
        let len = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, FILE_NAME_NORMALIZED) };
        if len == 0 || len as usize > buf.len() {
            return None;
        }
        return Some(strip_extended_prefix(&String::from_utf16_lossy(
            &buf[..len as usize],
        )));
    }
    Some(strip_extended_prefix(&String::from_utf16_lossy(
        &buf[..len as usize],
    )))
}

/// `\\?\C:\dir` → `C:\dir`. UNC forms are left alone.
fn strip_extended_prefix(s: &str) -> PathBuf {
    PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(s))
}
