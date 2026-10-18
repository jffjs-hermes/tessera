//! Report the vendored Luau version this build links.
//!
//! Run `cargo run -p tessera-script --example print_luau_version` (CI does
//! this when tessera-script changes) to record the exact `luau0-src` version
//! so drift between cache misses is visible in build output.

fn main() {
    let lua = mlua::Lua::new();
    println!("{}", tessera_script::sandbox::luau_version(&lua));
}
