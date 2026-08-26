//! HP WMI transport (M0.2 spike gate).
//!
//! Shape: `BiosInvoker` is the seam — one method `invoke(method, args)`.
//! The wmi-crate implementation builds the `hpqBDataIn` instance manually
//! (embedded object + byte arrays, which serde-wmi's typed path cannot
//! pass). If this ever proves unworkable on real firmware, a raw-COM
//! (IWbemServices::ExecMethod) invoker drops in behind the same seam —
//! nothing above `raw_execute` can tell the difference.
//!
//! Threading: the transport is created and used on ONE thread (the HpActor
//! in M1; the probe's main thread in M0). COM apartment affinity is not
//! something we gamble on.
//!
//! Provenance (architecture.md §54 — source reliability tiers): the
//! authoritative protocol reference for everything in this module is the
//! Linux kernel driver `drivers/platform/x86/hp/hp-wmi.c` (Tier A —
//! upstream-reviewed, tested on real OMEN/Victus hardware). Key commits
//! this module's behavior traces to:
//!   13fa3aaf02  8BAB (16-wf0xxx) fan+thermal enablement, omen_v1 params
//!   08ecf6d131  board feature data tables
//!   c203c59fb5  manual fan write 0x2E + keep-alive semantics (0x10)
//!   46be1453e6  0x26 max-fan readback unreliable → diagnostics-only
//!   59f586eb93  MUX 0x52
//! Where community sources (OmenMon/OmenSuperHub, Tier B) conflict with
//! the kernel — e.g. 0x29 power-limit byte order — we trust NEITHER and
//! gate the write behind on-device verification (§25, §57). GPL hygiene
//! (§55): behavior is re-implemented from the protocol facts; no kernel
//! code is copied.

pub(crate) mod actor;
pub(crate) mod commands;

#[cfg(windows)]
mod imp {
    use super::commands::{self, BiosArgs, HpCommandGroup, OutputSize, cmd};
    use phelper_domain::error::HpWmiError;
    use phelper_domain::hp::{FanTable, SystemDesignData};
    use phelper_domain::policy::{FanLevels, GpuPlatformPolicy, MuxMode};
    use phelper_domain::ports::HpPlatform;
    use serde::Deserialize;
    use tracing::{debug, info, warn};
    use wmi::{IWbemClassWrapper, WMIConnection};

    /// Result of one BIOS method invocation.
    #[derive(Debug)]
    pub(crate) struct BiosResponse {
        /// BIOS-level return code (hp-wmi.c bios_return): 0 ok, 2 bad
        /// signature, 3 unknown command, 4 unknown command type,
        /// 5 invalid parameters.
        pub(crate) return_code: u32,
        pub(crate) data: Vec<u8>,
    }

    /// The transport seam (see module docs).
    pub(crate) trait BiosInvoker: Send {
        fn invoke(&self, method: &str, args: &BiosArgs) -> Result<BiosResponse, HpWmiError>;
    }

    /// wmi-crate invoker: manual IWbemClassWrapper property construction.
    pub(crate) struct WmiCrateInvoker {
        conn: WMIConnection,
        instance_path: String,
    }

    // SAFETY: the wmi crate initializes COM as MTA (COINIT_MULTITHREADED),
    // so interface pointers are usable from any MTA-initialized thread.
    // Design invariant: each WmiCrateInvoker is created and used on ONE
    // thread (the HpActor, or the probe's main thread); Send exists only to
    // satisfy the port bounds, never for concurrent use.
    unsafe impl Send for WmiCrateInvoker {}

    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct HpqBIntMInstance {
        instance_name: String,
    }

    impl WmiCrateInvoker {
        pub(crate) fn connect() -> Result<Self, HpWmiError> {
            let conn = WMIConnection::with_namespace_path("root\\wmi")
                .map_err(|e| HpWmiError::Transport(format!("connect root\\wmi: {e}")))?;
            let instance = {
                let mut iter = conn
                    .exec_query("SELECT InstanceName FROM hpqBIntM")
                    .map_err(|e| HpWmiError::Transport(format!("query hpqBIntM: {e}")))?;
                let first = iter
                    .next()
                    .ok_or(HpWmiError::ProbeFailed("hpqBIntM instance not found"))?;
                let wrapper =
                    first.map_err(|e| HpWmiError::Transport(format!("hpqBIntM item: {e}")))?;
                wrapper
                    .into_desr::<HpqBIntMInstance>()
                    .map_err(|e| HpWmiError::Transport(format!("hpqBIntM deser: {e}")))?
            };
            // Relative object path resolves against the connected namespace;
            // backslashes inside the key value must be doubled.
            let escaped = instance.instance_name.replace('\\', "\\\\");
            let path = format!("hpqBIntM.InstanceName=\"{escaped}\"");
            // Verify the path resolves (fail fast at connect, not first call).
            conn.get_object(&path)
                .map_err(|_| HpWmiError::ProbeFailed("hpqBIntM instance path did not resolve"))?;
            info!(%path, "hpqBIntM instance found");
            Ok(Self {
                conn,
                instance_path: path,
            })
        }

        fn put_u32(obj: &IWbemClassWrapper, name: &str, value: u32) -> Result<(), HpWmiError> {
            obj.put_property(name, value)
                .map_err(|e| HpWmiError::Transport(format!("put {name}: {e}")))
        }
    }

    impl BiosInvoker for WmiCrateInvoker {
        fn invoke(&self, method: &str, args: &BiosArgs) -> Result<BiosResponse, HpWmiError> {
            // Build hpqBDataIn embedded instance.
            let data_class = self
                .conn
                .get_object("hpqBDataIn")
                .map_err(|e| HpWmiError::Transport(format!("get hpqBDataIn class: {e}")))?;
            let data = data_class
                .spawn_instance()
                .map_err(|e| HpWmiError::Transport(format!("spawn hpqBDataIn: {e}")))?;
            Self::put_u32(&data, "Command", args.command)?;
            Self::put_u32(&data, "CommandType", args.command_type)?;
            data.put_property("hpqBData", args.data.clone())
                .map_err(|e| HpWmiError::Transport(format!("put hpqBData: {e}")))?;
            Self::put_u32(&data, "Size", args.size)?;
            // MOF declares Sign as byte[4] ("SECU" on the wire = signature
            // 0x55434553 LE in the kernel's raw struct).
            data.put_property("Sign", b"SECU".to_vec())
                .map_err(|e| HpWmiError::Transport(format!("put Sign: {e}")))?;

            // Build the method's __InParameters with Data = the instance.
            let class = self
                .conn
                .get_object("hpqBIntM")
                .map_err(|e| HpWmiError::Transport(format!("get hpqBIntM class: {e}")))?;
            let in_params_class = class
                .get_method(method)
                .map_err(|e| HpWmiError::Transport(format!("get method {method}: {e}")))?
                .ok_or(HpWmiError::ProbeFailed("method not found on hpqBIntM"))?;
            let in_params = in_params_class
                .spawn_instance()
                .map_err(|e| HpWmiError::Transport(format!("spawn in-params: {e}")))?;
            in_params
                .put_property("InData", data)
                .map_err(|e| HpWmiError::Transport(format!("put InData: {e}")))?;

            let out = self
                .conn
                .exec_method(&self.instance_path, method, Some(&in_params))
                .map_err(|e| HpWmiError::Transport(format!("exec {method}: {e}")))?
                .ok_or(HpWmiError::InvalidResponse("method returned no out-params"))?;

            // Method-level ReturnValue. The 8BAB MOF declares Boolean
            // (verified via Get-CimClass on the reference machine).
            if let Ok(rv) = out.get_property("ReturnValue")
                && let Ok(false) = rv.try_into()
            {
                return Err(HpWmiError::MethodReturnCode { code: 1 });
            }

            // Out-param "OutData": embedded hpqBDataOut{N} instance
            // (per-size classes, not schema-registered under root\wmi).
            // Verified on the reference machine: properties are
            // Active / Data / InstanceName / rwReturnCode / Sign="PASS" —
            // note the byte array is "Data" here but "hpqBData" on the
            // input object. "PASS" in Sign confirms a real firmware
            // round-trip.
            //
            // hpqBIOSInt0 (writes, outsize=0) may carry NO OutData at all —
            // ReturnValue=true is then the whole answer, and demanding an
            // out-object would misreport a successful write as a transport
            // failure (design review R2; S2 verified this shape on-device).
            let data_out_v = match out.get_property("OutData") {
                Ok(v) => v,
                Err(e) if method == OutputSize::Zero.method_name() => {
                    debug!(%e, "no OutData on hpqBIOSInt0 — empty success");
                    return Ok(BiosResponse {
                        return_code: 0,
                        data: Vec::new(),
                    });
                }
                Err(e) => {
                    return Err(HpWmiError::Transport(format!("read OutData: {e}")));
                }
            };
            let data_out: IWbemClassWrapper = data_out_v
                .try_into()
                .map_err(|_| HpWmiError::InvalidResponse("out Data is not an object"))?;
            let return_code: u32 = data_out
                .get_property("rwReturnCode")
                .ok()
                .and_then(|v| v.try_into().ok())
                .unwrap_or(0);
            let sign: Vec<u8> = data_out
                .get_property("Sign")
                .ok()
                .and_then(|v| v.try_into().ok())
                .unwrap_or_default();
            if !sign.is_empty() && sign != b"PASS" {
                warn!(
                    ?sign,
                    "hpqBDataOut Sign is not PASS — response may not be from firmware"
                );
            }
            let bytes: Vec<u8> = data_out
                .get_property("Data")
                .ok()
                .and_then(|v| v.try_into().ok())
                .unwrap_or_default();

            Ok(BiosResponse {
                return_code,
                data: bytes,
            })
        }
    }

    fn hex_head(data: &[u8]) -> String {
        data.iter()
            .take(32)
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Firmware read size mode (hp-wmi.c `zero_if_sup`): newer firmware
    /// requires insize=0 for reads; probed once at init.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum InsizeMode {
        Zero,
        Actual,
    }

    /// The typed HP transport. `pub(crate)` — the only public surface is the
    /// domain `HpPlatform` port (no raw payload escapes, §50).
    pub(crate) struct HpWmiTransport {
        invoker: Box<dyn BiosInvoker>,
        insize: InsizeMode,
    }

    impl HpWmiTransport {
        /// Connect and run the insize probe (fan-count read in Zero mode,
        /// fall back to Actual on firmware/method rejection).
        pub(crate) fn connect() -> Result<Self, HpWmiError> {
            let invoker = Box::new(WmiCrateInvoker::connect()?);
            let insize = Self::probe_insize(invoker.as_ref())?;
            info!(?insize, "HP WMI insize mode detected");
            Ok(Self { invoker, insize })
        }

        /// Constructor with an explicit invoker (tests).
        #[cfg(test)]
        #[allow(dead_code)] // used by M1 actor tests
        pub(crate) fn with_invoker(invoker: Box<dyn BiosInvoker>, insize: InsizeMode) -> Self {
            Self { invoker, insize }
        }

        fn probe_insize(invoker: &dyn BiosInvoker) -> Result<InsizeMode, HpWmiError> {
            let args = BiosArgs::read(HpCommandGroup::Gaming, cmd::FAN_COUNT_GET, &[], true);
            match invoker.invoke(OutputSize::Small4.method_name(), &args) {
                Ok(resp) => {
                    // One-time transport bring-up diagnostics: the out-object's
                    // inner property names (rwReturnCode/hpqBData) are not
                    // schema-registered (no hpqBDataOut class), so log what we
                    // actually got back on the very first call.
                    info!(rc = resp.return_code, len = resp.data.len(), hex = %hex_head(&resp.data), "insize probe (zero) response");
                    if resp.return_code == 0 {
                        Ok(InsizeMode::Zero)
                    } else {
                        Self::probe_insize_actual(invoker)
                    }
                }
                Err(e) => {
                    debug!(%e, "zero-insize probe failed, retrying actual");
                    Self::probe_insize_actual(invoker)
                }
            }
        }

        fn probe_insize_actual(invoker: &dyn BiosInvoker) -> Result<InsizeMode, HpWmiError> {
            let args = BiosArgs::read(HpCommandGroup::Gaming, cmd::FAN_COUNT_GET, &[], false);
            let resp = invoker.invoke(OutputSize::Small4.method_name(), &args)?;
            if resp.return_code == 0 {
                Ok(InsizeMode::Actual)
            } else {
                Err(HpWmiError::ProbeFailed(
                    "fan count probe failed in both insize modes",
                ))
            }
        }

        /// The single raw READ entry point. `pub(crate)` ONLY — typed ops
        /// above, no raw payload construction outside this module.
        pub(crate) fn raw_execute(
            &self,
            group: HpCommandGroup,
            cmd_type: u8,
            input: &[u8],
            out: OutputSize,
        ) -> Result<Vec<u8>, HpWmiError> {
            let args = BiosArgs::read(group, cmd_type, input, self.insize == InsizeMode::Zero);
            let resp = self.invoker.invoke(out.method_name(), &args)?;
            if resp.return_code != 0 {
                return Err(HpWmiError::from_firmware_code(resp.return_code));
            }
            Ok(resp.data)
        }

        /// The single raw WRITE entry point (control feature only). Same
        /// closed-ops discipline as raw_execute: typed callers, no payloads
        /// from outside this module. `datasize` always carries the real
        /// input length (BiosArgs::write — insize=0 is a read-side probe).
        ///
        /// Kernel set calls use outsize=0, tried first. If firmware rejects
        /// the method shape (unknown command/cmdtype or a transport-level
        /// method failure), fall back to Small4 then Medium128 — S2 pinned
        /// the working variant on 8BAB for each op and the fallback log
        /// records which one firmware actually accepted.
        #[cfg(feature = "control")]
        pub(crate) fn write_execute(
            &self,
            group: HpCommandGroup,
            cmd_type: u8,
            input: &[u8],
        ) -> Result<(), HpWmiError> {
            let args = BiosArgs::write(group, cmd_type, input);
            let mut last_err = None;
            for out in [OutputSize::Zero, OutputSize::Small4, OutputSize::Medium128] {
                match self.invoker.invoke(out.method_name(), &args) {
                    Ok(resp) if resp.return_code == 0 => {
                        info!(cmd_type, outsize = out.method_name(), "hp-wmi write accepted");
                        return Ok(());
                    }
                    Ok(resp) => {
                        let rc = resp.return_code;
                        debug!(cmd_type, outsize = out.method_name(), rc, "write rejected by firmware");
                        last_err = Some(HpWmiError::from_firmware_code(rc));
                        // rc 3/4 (unknown command/cmdtype) may be a wrong
                        // method variant — try the next outsize. rc 5
                        // (invalid parameters) is a PAYLOAD problem: stop.
                        if rc != 3 && rc != 4 {
                            break;
                        }
                    }
                    Err(e) => {
                        debug!(cmd_type, outsize = out.method_name(), %e, "write transport failure");
                        last_err = Some(e);
                    }
                }
            }
            Err(last_err.unwrap_or(HpWmiError::InvalidResponse("write: no outsize attempted")))
        }

        #[allow(dead_code)] // M1 actor reports this in ProviderStatus
        pub(crate) fn insize_mode(&self) -> InsizeMode {
            self.insize
        }
    }

    #[cfg(feature = "control")]
    impl phelper_domain::ports::HpControl for HpWmiTransport {
        fn set_thermal_mode(&self, mode: phelper_domain::policy::ThermalMode) -> Result<(), HpWmiError> {
            let payload = commands::encode_thermal_mode_v1(mode);
            self.write_execute(HpCommandGroup::Gaming, cmd::SET_PERFORMANCE_MODE, &payload)
        }

        fn set_fan_levels(&self, levels: FanLevels) -> Result<(), HpWmiError> {
            let payload = commands::encode_fan_levels(levels)?;
            self.write_execute(HpCommandGroup::Gaming, cmd::FAN_LEVELS_SET, &payload)
        }

        fn set_max_fan(&self, on: bool) -> Result<(), HpWmiError> {
            let payload = commands::encode_max_fan(on);
            self.write_execute(HpCommandGroup::Gaming, cmd::MAX_FAN_SET, &payload)
        }
    }

    impl HpPlatform for HpWmiTransport {
        fn fan_count(&self) -> Result<u8, HpWmiError> {
            let buf = self.raw_execute(
                HpCommandGroup::Gaming,
                cmd::FAN_COUNT_GET,
                &[],
                OutputSize::Small4,
            )?;
            commands::decode_fan_count(&buf)
        }

        fn system_design_data(&self) -> Result<SystemDesignData, HpWmiError> {
            let buf = self.raw_execute(
                HpCommandGroup::Gaming,
                cmd::SYSTEM_DESIGN_DATA,
                &[],
                OutputSize::Medium128,
            )?;
            commands::decode_sdd(&buf)
        }

        fn fan_table(&self) -> Result<FanTable, HpWmiError> {
            let input = commands::encode_fan_table_request();
            let buf = self.raw_execute(
                HpCommandGroup::Gaming,
                cmd::FAN_TABLE_GET,
                &input,
                OutputSize::Medium128,
            )?;
            commands::decode_fan_table(&buf)
        }

        fn fan_levels(&self) -> Result<FanLevels, HpWmiError> {
            let buf = self.raw_execute(
                HpCommandGroup::Gaming,
                cmd::FAN_LEVELS_GET,
                &[],
                OutputSize::Medium128,
            )?;
            commands::decode_fan_levels(&buf)
        }

        fn gpu_platform_policy(&self) -> Result<GpuPlatformPolicy, HpWmiError> {
            let buf = self.raw_execute(
                HpCommandGroup::Gaming,
                cmd::GPU_POLICY_GET,
                &[],
                OutputSize::Small4,
            )?;
            commands::decode_gpu_policy(&buf)
        }

        fn mux_mode(&self) -> Result<MuxMode, HpWmiError> {
            let buf = self.raw_execute(
                HpCommandGroup::LegacyRead,
                cmd::MUX,
                &[],
                OutputSize::Small4,
            )?;
            commands::decode_mux(&buf)
        }

        fn max_fan_readback_diagnostic(&self) -> Result<bool, HpWmiError> {
            let buf = self.raw_execute(
                HpCommandGroup::Gaming,
                cmd::MAX_FAN_GET,
                &[],
                OutputSize::Small4,
            )?;
            let v = commands::decode_max_fan_diag(&buf)?;
            warn!("0x26 max-fan readback is diagnostics-only (unreliable on this firmware family)");
            Ok(v)
        }
    }
}

#[cfg(windows)]
pub(crate) use imp::*;

// The transport is Windows-only by definition; non-Windows builds exist so
// domain logic stays testable elsewhere.
#[cfg(not(windows))]
mod imp {
    compile_error!("phelper-core platform adapters are Windows-only");
}
