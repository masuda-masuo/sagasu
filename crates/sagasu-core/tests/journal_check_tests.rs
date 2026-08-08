//! Tests for the platform-neutral USN-marker lifetime check
//! (`delta::classify_journal` / `delta::check_journal`, issue #60).
//!
//! The probe itself — open the volume, `FSCTL_QUERY_USN_JOURNAL` — can only
//! run on Windows. What can be tested everywhere is the decision on top of it:
//! given a marker and a set of live journal values, is the marker fine, about
//! to expire, or already gone, and why do the not-checked cases say they are
//! not checked. These tests feed observable values and pin the classification,
//! so they run and mean something on Linux — a check that merely asserted the
//! platform `cfg` could never fail and would prove nothing.

use sagasu_core::delta::{
    check_journal, classify_journal, estimate_lifetime, JournalCheck, LiveJournal, ScanMarker,
};

fn usn_marker(next_usn: i64, maximum_size: u64, recorded_ns: i64) -> ScanMarker {
    ScanMarker::Usn {
        volume: "C:".into(),
        journal_id: 7,
        next_usn,
        maximum_size,
        recorded_ns,
    }
}

fn live(journal_id: u64, first_usn: i64, next_usn: i64, maximum_size: u64) -> LiveJournal {
    LiveJournal {
        journal_id,
        first_usn,
        next_usn,
        maximum_size,
    }
}

fn checked(check: &JournalCheck) -> &sagasu_core::delta::MarkerLifetime {
    match check {
        JournalCheck::Checked { lifetime, .. } => lifetime,
        other => panic!("expected a checked result, got {other:?}"),
    }
}

// ── classification: fine / short / gone ─────────────────────────────────────

#[test]
fn healthy_marker_classifies_as_fine_with_a_lifetime_estimate() {
    // 8 MiB consumed in 300 s against a 32 MiB journal: 24 MiB of headroom at
    // ~27.9 KiB/s leaves roughly 900 s — the same arithmetic
    // `estimate_lifetime` pins in its own unit tests.
    let marker = usn_marker(1_000_000, 32 * 1024 * 1024, 0);
    let now = 300i64 * 1_000_000_000;
    let check = classify_journal(
        &marker,
        &live(7, 500_000, 1_000_000 + 8 * 1024 * 1024, 32 * 1024 * 1024),
        now,
    );

    let JournalCheck::Checked {
        next_usn_now,
        lifetime,
        journal_matches,
        rolled_off,
        ..
    } = &check
    else {
        panic!("expected a checked result, got {check:?}");
    };
    assert!(journal_matches);
    assert!(!rolled_off);
    assert!(!lifetime.expired);
    assert_eq!(*next_usn_now, 1_000_000 + 8 * 1024 * 1024);
    assert_eq!(lifetime.consumed, 8 * 1024 * 1024);
    let remaining = lifetime.remaining_secs.unwrap();
    assert!((remaining - 900.0).abs() < 1.0, "{remaining}");
}

#[test]
fn low_headroom_marker_still_classifies_fine_but_short() {
    // 31 MiB of a 32 MiB journal consumed: not rolled off yet, but the
    // remaining estimate is measured in minutes — the `--journal-warn-hours`
    // warning exists precisely for this case.
    let marker = usn_marker(1_000_000, 32 * 1024 * 1024, 0);
    let now = 300i64 * 1_000_000_000;
    let check = classify_journal(
        &marker,
        &live(7, 500_000, 1_000_000 + 31 * 1024 * 1024, 32 * 1024 * 1024),
        now,
    );
    let lifetime = checked(&check);
    assert!(!lifetime.expired);
    let remaining = lifetime.remaining_secs.unwrap();
    assert!(remaining < 3600.0, "{remaining} s should be under an hour");
}

#[test]
fn fully_consumed_marker_classifies_as_rolled_off_and_expired() {
    let marker = usn_marker(0, 8 * 1024 * 1024, 0);
    let now = 60i64 * 1_000_000_000;
    let check = classify_journal(
        &marker,
        &live(7, 64 * 1024 * 1024, 64 * 1024 * 1024, 8 * 1024 * 1024),
        now,
    );

    let JournalCheck::Checked {
        lifetime,
        journal_matches,
        rolled_off,
        ..
    } = &check
    else {
        panic!("expected a checked result, got {check:?}");
    };
    assert!(journal_matches, "the journal id still matches");
    assert!(rolled_off, "a fully consumed ring has rolled the marker off");
    assert!(lifetime.expired);
}

#[test]
fn first_usn_past_the_marker_classifies_rolled_off_before_capacity_is_consumed() {
    // The authoritative roll-off check the delta read uses is
    // `marker.next_usn < journal.FirstUsn` — not the consumed-vs-capacity
    // arithmetic, which can lag it (a journal resized smaller, or one whose
    // growth pattern differs from the marker's recorded `MaximumSize`).
    let marker = usn_marker(10_000, 32 * 1024 * 1024, 0);
    let now = 60i64 * 1_000_000_000;
    let check = classify_journal(&marker, &live(7, 12_000, 20_000, 16 * 1024 * 1024), now);

    let JournalCheck::Checked {
        lifetime,
        rolled_off,
        ..
    } = &check
    else {
        panic!("expected a checked result, got {check:?}");
    };
    assert!(rolled_off);
    assert!(
        !lifetime.expired,
        "the estimate alone would call this fine; FirstUsn is the point"
    );
}

#[test]
fn recreated_journal_classifies_as_not_matching() {
    // Same numbers, different journal id: the USN space restarted, so the
    // numbers mean nothing even though nothing looks consumed.
    let marker = usn_marker(1_000_000, 32 * 1024 * 1024, 0);
    let now = 300i64 * 1_000_000_000;
    let check = classify_journal(
        &marker,
        &live(99, 500_000, 1_000_000 + 8 * 1024 * 1024, 32 * 1024 * 1024),
        now,
    );
    let JournalCheck::Checked {
        journal_matches, ..
    } = &check
    else {
        panic!("expected a checked result, got {check:?}");
    };
    assert!(!journal_matches);
}

#[test]
fn rate_not_observable_yet_yields_no_invented_lifetime() {
    // Nothing written since the marker: there is no rate to divide headroom
    // by, and the check must not invent one (`remaining_secs` stays `None`).
    let marker = usn_marker(1_000_000, 32 * 1024 * 1024, 0);
    let now = 300i64 * 1_000_000_000;
    let check = classify_journal(&marker, &live(7, 500_000, 1_000_000, 32 * 1024 * 1024), now);
    let lifetime = checked(&check);
    assert_eq!(lifetime.consumed, 0);
    assert!(lifetime.remaining_secs.is_none());
}

// ── the not-checked cases ───────────────────────────────────────────────────

#[test]
fn an_mtime_marker_is_not_checked_with_the_reason_named() {
    // A wall-clock instant does not expire, so there is nothing to check; the
    // reason must name the situation, not fail.
    let marker = ScanMarker::Mtime { started_ns: 0 };
    let check = classify_journal(&marker, &live(7, 1, 1, 1), 0);
    match &check {
        JournalCheck::NotChecked { reason } => {
            assert!(reason.contains("mtime"), "{reason}");
            assert!(reason.contains("never expires"), "{reason}");
        }
        other => panic!("expected not-checked, got {other:?}"),
    }
}

#[test]
fn a_missing_marker_is_not_checked_with_the_reason_named() {
    let check = check_journal(None, 0);
    match &check {
        JournalCheck::NotChecked { reason } => {
            assert!(reason.contains("no delta marker"), "{reason}");
        }
        other => panic!("expected not-checked, got {other:?}"),
    }
}

#[test]
fn check_journal_routes_an_mtime_marker_to_the_not_checked_reason() {
    // The entry point the CLI calls must produce the same verdict as the pure
    // classifier, without ever reaching the platform fetch.
    let marker = ScanMarker::Mtime { started_ns: 0 };
    let check = check_journal(Some(&marker), 0);
    match &check {
        JournalCheck::NotChecked { reason } => {
            assert!(reason.contains("mtime"), "{reason}");
        }
        other => panic!("expected not-checked, got {other:?}"),
    }
}

// ── the check agrees with the arithmetic it reports ─────────────────────────

#[test]
fn classify_and_estimate_lifetime_agree_on_the_same_inputs() {
    // The Checked verdict carries the same `MarkerLifetime` the standalone
    // function returns — the two cannot drift apart.
    let marker = usn_marker(1_000_000, 32 * 1024 * 1024, 0);
    let live = live(7, 500_000, 1_000_000 + 8 * 1024 * 1024, 32 * 1024 * 1024);
    let now = 300i64 * 1_000_000_000;
    let check = classify_journal(&marker, &live, now);
    let lifetime = checked(&check);
    let direct = estimate_lifetime(&marker, live.next_usn, now).unwrap();
    assert_eq!(lifetime.consumed, direct.consumed);
    assert_eq!(lifetime.headroom, direct.headroom);
    assert_eq!(lifetime.remaining_secs, direct.remaining_secs);
    assert_eq!(lifetime.expired, direct.expired);
}

#[test]
fn consumed_past_the_recorded_capacity_is_not_rolled_off_while_first_usn_lags() {
    // The other direction of the same asymmetry: the marker's *recorded*
    // MaximumSize is used up, but the live ring is bigger and `FirstUsn` is
    // still behind the marker, so the delta read would succeed. Calling this
    // "rolled off" would tell the user their index is dead and push them into
    // a full reindex for nothing.
    //
    // This is not a hypothetical shape: NTFS treats MaximumSize as a target
    // and trims lazily — on real hardware (issue #37) a 512 KB journal held
    // ~90,000 records without wrapping.
    let marker = usn_marker(1_000_000, 8 * 1024 * 1024, 0);
    let now = 300i64 * 1_000_000_000;
    let check = classify_journal(
        &marker,
        &live(
            7,
            500_000,
            1_000_000 + 16 * 1024 * 1024,
            64 * 1024 * 1024,
        ),
        now,
    );

    let JournalCheck::Checked {
        lifetime,
        journal_matches,
        rolled_off,
        live_maximum_size,
        ..
    } = &check
    else {
        panic!("expected a checked result, got {check:?}");
    };
    assert!(journal_matches);
    assert!(
        !rolled_off,
        "FirstUsn is still behind the marker, so the delta read would succeed"
    );
    assert!(
        lifetime.expired,
        "the estimate against the recorded capacity does say expired — it is just not the verdict"
    );
    assert_eq!(
        *live_maximum_size,
        64 * 1024 * 1024,
        "the live capacity is reported so the discrepancy is visible"
    );
}
