fn main() {
    // dust-core takes loggers whenever its `log` feature is on, which another crate in the same
    // build (such as dust-agent) can enable without this crate's `log` feature.
    println!("cargo::rustc-check-cfg=cfg(core_log)");
    if std::env::var_os("DEP_DUST_CORE_LOG").is_some() {
        println!("cargo::rustc-cfg=core_log");
    }
}
