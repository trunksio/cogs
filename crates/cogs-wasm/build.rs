fn main() {
    // This crate's native TEST binaries link lbug (via the cogs-graph
    // dev-dependency, for native-vs-wasm parity tests) and exercise FTS —
    // they need exported core symbols like every binary embedding lbug (see
    // repo CLAUDE.md). The flag must NOT reach the wasm32 linker (rust-lld
    // rejects -rdynamic), and Windows never needs it.
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.starts_with("wasm32") && !target.contains("windows") {
        println!("cargo:rustc-link-arg=-rdynamic");
    }
}
