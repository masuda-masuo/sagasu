// See ../build-common.rs for what this actually does and why it's structured this way.
include!("../build-common.rs");

fn main() {
    embed_windows_resource(
        "sagasu proto-crawl",
        "sagasu proto-crawl - parallel filesystem crawl throughput prototype",
        "asInvoker",
    );
}
