//! `phelper-cli gpuload` — DEV VERIFICATION load generator, not a product
//! feature. Drives the dGPU awake (a memory-bound CUDA memset loop) so the
//! M1 acceptance run can observe gpu.power_w non-zero and NVAPI clocks
//! ramping. Uses the CUDA Driver API shipped with the NVIDIA driver
//! (nvcuda.dll) — no toolkit install needed. Load, never control: it
//! touches no phelper write paths.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{PCWSTR, s};

#[derive(Args)]
pub struct GpuLoadArgs {
    /// How long to burn, in seconds.
    #[arg(long, default_value_t = 30)]
    seconds: u64,
    /// Device memory to churn through, in MiB.
    #[arg(long, default_value_t = 512)]
    mem_mib: u64,
}

type CuResult = i32;

/// Resolve one nvcuda entry point and transmute it to the expected
/// signature. MUST be used inside an `unsafe` block (raw transmute).
macro_rules! cufn {
    ($lib:expr, $name:literal, $ty:ty) => {{
        let p = GetProcAddress($lib, s!($name))
            .ok_or_else(|| anyhow::anyhow!("nvcuda entry {} missing", $name))?;
        std::mem::transmute::<_, $ty>(p)
    }};
}

pub fn run(args: GpuLoadArgs) -> Result<()> {
    unsafe {
        let lib: HMODULE = LoadLibraryW(PCWSTR(
            "nvcuda.dll\0".encode_utf16().collect::<Vec<u16>>().as_ptr(),
        ))
        .context("LoadLibrary nvcuda.dll")?;

        let cu_init: fn(u32) -> CuResult = cufn!(lib, "cuInit", _);
        let cu_device_get_count: fn(*mut i32) -> CuResult = cufn!(lib, "cuDeviceGetCount", _);
        let cu_device_get: fn(*mut i32, i32) -> CuResult = cufn!(lib, "cuDeviceGet", _);
        let cu_ctx_create: fn(*mut *mut core::ffi::c_void, u32, i32) -> CuResult =
            cufn!(lib, "cuCtxCreate_v2", _);
        let cu_mem_alloc: fn(*mut usize, usize) -> CuResult = cufn!(lib, "cuMemAlloc_v2", _);
        let cu_memset: fn(usize, u32, usize) -> CuResult = cufn!(lib, "cuMemsetD32_v2", _);
        let cu_ctx_sync: fn() -> CuResult = cufn!(lib, "cuCtxSynchronize", _);
        let cu_mem_free: fn(usize) -> CuResult = cufn!(lib, "cuMemFree_v2", _);
        let cu_ctx_destroy: fn(*mut core::ffi::c_void) -> CuResult =
            cufn!(lib, "cuCtxDestroy_v2", _);

        check(cu_init(0), "cuInit")?;
        let mut count = 0;
        check(cu_device_get_count(&mut count), "cuDeviceGetCount")?;
        if count == 0 {
            bail!("no CUDA device");
        }
        let mut dev = 0;
        check(cu_device_get(&mut dev, 0), "cuDeviceGet")?;
        let mut ctx = std::ptr::null_mut();
        check(cu_ctx_create(&mut ctx, 0, dev), "cuCtxCreate")?;

        let bytes = (args.mem_mib as usize) << 20;
        let words = bytes / 4;
        let mut buf = 0usize;
        check(cu_mem_alloc(&mut buf, bytes), "cuMemAlloc")?;

        println!(
            "burning {} MiB on CUDA device 0 for {} s ...",
            args.mem_mib, args.seconds
        );
        let deadline = Instant::now() + Duration::from_secs(args.seconds);
        let mut iters = 0u64;
        while Instant::now() < deadline {
            // Queue a batch of memsets, sync occasionally to stay queued-deep
            // without piling up unbounded work.
            for _ in 0..64 {
                check(cu_memset(buf, 0xA5, words), "cuMemsetD32")?;
            }
            check(cu_ctx_sync(), "cuCtxSynchronize")?;
            iters += 64;
        }

        cu_mem_free(buf);
        cu_ctx_destroy(ctx);
        println!("done: {iters} memset iterations");
    }
    Ok(())
}

fn check(rc: CuResult, what: &str) -> Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        bail!("{what} failed: CUresult={rc}")
    }
}
