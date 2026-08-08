//! Tests for the platform-neutral parent-FRN failure classification
//! (`delta::classify_frn_failure`, issue #57).
//!
//! The open itself — `OpenFileById` on a parent FRN — can only run on
//! Windows. What can be tested everywhere is the decision on top of it: given
//! the Win32 error code from the open, is the failure benign (the parent
//! directory is gone, so the path this record describes is gone → `gone`) or
//! an error (the parent is live and could not be read → `errors`)? These tests
//! feed observable inputs and pin the classification, so they run and mean
//! something on Linux. A test that merely asserted the platform `cfg` could
//! never fail and would prove nothing.

use sagasu_core::delta::{
    classify_frn_failure, FrnFailure, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER,
    ERROR_PATH_NOT_FOUND,
};

// Win32 error codes that must classify as errors: the directory is there, we
// just could not open it.
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_SHARING_VIOLATION: u32 = 32;

// ── the parent is gone → benign ─────────────────────────────────────────────

#[test]
fn every_does_not_exist_code_is_benign() {
    // All three "does not exist"-class codes `OpenFileById` can return for a
    // stale FRN mean the same thing: the parent directory is gone, so the path
    // the record describes is gone with it.
    for code in [
        ERROR_FILE_NOT_FOUND,
        ERROR_PATH_NOT_FOUND,
        ERROR_INVALID_PARAMETER,
    ] {
        assert_eq!(
            classify_frn_failure(code),
            FrnFailure::Gone,
            "gone code {code} must be benign"
        );
    }
}

#[test]
fn the_records_own_reason_does_not_change_the_verdict() {
    // This is the correction the first cut of the design needed: requiring a
    // `USN_REASON_FILE_DELETE` bit sent every *create* record of a
    // create+delete churn to `errors` — roughly half of the 90,016 failures
    // measured in issue #37 — which is the "errors is always huge" failure the
    // three-way split exists to avoid. A gone parent means the path is gone,
    // whatever the record says happened at it.
    //
    // The classifier no longer takes the reason mask at all, so this test is
    // the statement that it must not come back: the verdict for a given code
    // is one value, full stop.
    assert_eq!(classify_frn_failure(ERROR_FILE_NOT_FOUND), FrnFailure::Gone);
    assert_eq!(
        classify_frn_failure(ERROR_ACCESS_DENIED),
        FrnFailure::Unresolved
    );
}

// ── the parent is live and unreadable → error ───────────────────────────────

#[test]
fn access_denied_and_sharing_violation_are_errors() {
    // Nobody asked for these to be dropped, and the directory is still there:
    // a real change may be hidden behind the failure, so the answer must be
    // reported as possibly incomplete.
    for code in [ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION] {
        assert_eq!(
            classify_frn_failure(code),
            FrnFailure::Unresolved,
            "live-but-unreadable code {code} must count as an error"
        );
    }
}

#[test]
fn an_unknown_code_is_an_error_not_a_silent_drop() {
    // The safe direction: a code nobody anticipated must overstate the
    // incompleteness rather than hide a change behind the benign counter.
    for code in [1u32, 4, 21, 1450, 0xDEAD] {
        assert_eq!(
            classify_frn_failure(code),
            FrnFailure::Unresolved,
            "unrecognised code {code} must count as an error"
        );
    }
}

#[test]
fn success_is_never_classified_as_a_failure_code() {
    // 0 is `ERROR_SUCCESS`; it can only reach the classifier if the caller
    // mixed up its error handling, and treating it as "gone" would silently
    // drop records. Error is the honest verdict.
    assert_eq!(classify_frn_failure(0), FrnFailure::Unresolved);
}
