fn main() {
    // Another crate in the same build (such as dust-agent) can enable dust-core features this
    // crate doesn't: `log` makes the constructors take loggers, `debugger-hooks` adds `RunOutput`
    // variants.
    println!("cargo::rustc-check-cfg=cfg(core_log)");
    println!("cargo::rustc-check-cfg=cfg(core_debugger_hooks)");
    if std::env::var_os("DEP_DUST_CORE_LOG").is_some() {
        println!("cargo::rustc-cfg=core_log");
    }
    if std::env::var_os("DEP_DUST_CORE_DEBUGGER_HOOKS").is_some() {
        println!("cargo::rustc-cfg=core_debugger_hooks");
    }
}
