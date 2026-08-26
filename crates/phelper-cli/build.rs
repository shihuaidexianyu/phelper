fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // asInvoker for M0/M1: everything so far is read-only and works
        // unelevated (PawnIO driver load will need an elevated shell, but
        // that's the operator's choice, not a hard requirement). Manifest
        // elevation ALSO applies to cargo's test binary — highestAvailable
        // made `cargo test` fail with os error 740 from a normal shell.
        // Flip to HighestAvailable at M2 when control writes land; the
        // engine already detects token elevation at runtime (R9).
        embed_manifest::embed_manifest(
            embed_manifest::new_manifest("phelper-cli")
                .requested_execution_level(embed_manifest::manifest::ExecutionLevel::AsInvoker),
        )
        .expect("embed manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
