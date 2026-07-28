// See ../build-common.rs for what this actually does and why it's structured this way.
include!("../build-common.rs");

fn main() {
    // proto-usn's only function -- FSCTL_QUERY_USN_JOURNAL / FSCTL_READ_USN_JOURNAL on
    // a volume handle -- requires administrator rights unconditionally. There is no
    // reduced-privilege mode of this tool to preserve by staying asInvoker: every run
    // either has admin or fails at CreateFileW/DeviceIoControl. requireAdministrator
    // lets Windows elevate via UAC at launch instead of the process starting, opening
    // the volume, and failing several lines in with an access-denied error the
    // operator has to interpret and then retry "as administrator" by hand anyway.
    embed_windows_resource(
        "sagasu proto-usn",
        "sagasu proto-usn - NTFS USN journal differential read-back prototype",
        "requireAdministrator",
    );
}
