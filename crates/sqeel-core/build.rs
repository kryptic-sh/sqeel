fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_FEATURE_DUCKDB").is_ok()
    {
        println!("cargo:rustc-link-lib=rstrtmgr");
    }
}
