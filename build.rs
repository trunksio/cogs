fn main() {
    // lbug statically links the Ladybug C++ core; loadable extensions (FTS,
    // VECTOR) resolve core symbols from the host executable, which requires
    // exporting them. lbug's build.rs sets this for its own targets only —
    // every binary embedding lbug must repeat it.
    #[cfg(not(windows))]
    println!("cargo:rustc-link-arg=-rdynamic");
}
