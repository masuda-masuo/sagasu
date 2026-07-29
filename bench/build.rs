// See prototypes/build-common.rs for what this actually does and why it's
// structured this way.  Included from the prototypes/ subtree by reference;
// the file itself is not modified.
include!("../prototypes/build-common.rs");

fn main() {
    embed_windows_resource(
        "sagasu bench",
        "sagasu bench - benchmark harness for sagasu project",
        "asInvoker",
    );
}
