fn main() {
    // lbug statically links the Ladybug C++ core; loadable extensions (FTS,
    // VECTOR) resolve core symbols from the host executable, which requires
    // exporting them. lbug's build.rs sets this for its own targets only —
    // this crate's TEST binaries embed lbug (via cogs-graph) and exercise FTS
    // retrieval, so they must repeat it. (See the root build.rs and
    // CLAUDE.md.)
    #[cfg(not(windows))]
    println!("cargo:rustc-link-arg=-rdynamic");
}
