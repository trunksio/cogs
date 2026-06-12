# cogs build orchestration. `cargo build` alone works (empty viz placeholder);
# `just build` produces the full self-contained binary.

# Build the web app into web/dist
web:
    cd web && npm ci && npm run build

# Full build: web assets then the binary (release embeds web/dist)
build: web
    cargo build --release

# Run all tests
test:
    cargo test --workspace

# Dev loop for the viz: backend on :7117 + Vite HMR on :5173
dev-viz vault=".":
    ./target/debug/cogs --vault {{vault}} serve &
    cd web && npm run dev

# Check the Zed extension compiles to wasm
check-extension:
    cd zed-extension && cargo check --target wasm32-wasip2
