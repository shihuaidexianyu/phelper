fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // PERMANENTLY asInvoker (decision settled at M2, contrary to an
        // earlier draft of this comment): elevation is the operator's
        // choice — run from an admin terminal — and the engine detects the
        // token at runtime, degrading gracefully (unelevated: pawnio +
        // hp-wmi unavailable, telemetry continues, writes →
        // PermissionDenied). Manifest elevation ALSO applies to cargo's
        // test binary — highestAvailable made `cargo test` fail with
        // os error 740/741 from a normal shell. The desktop app (which
        // must always be elevated) self-elevates via runas instead of a
        // manifest, for the same reason.
        embed_manifest::embed_manifest(
            embed_manifest::new_manifest("phelper-cli")
                .requested_execution_level(embed_manifest::manifest::ExecutionLevel::AsInvoker),
        )
        .expect("embed manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
