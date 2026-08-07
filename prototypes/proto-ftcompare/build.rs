// See ../build-common.rs for what this actually does and why it's structured this way.
include!("../build-common.rs");

fn main() {
    embed_windows_resource(
        "sagasu proto-ftcompare",
        "sagasu proto-ftcompare - tantivy vs SQLite FTS5 comparison prototype",
        "asInvoker",
    );
}
