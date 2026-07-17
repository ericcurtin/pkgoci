//! Sandboxed execution of Pkgocifile RUN steps.
//!
//! - Linux: Docker (`--network=none`, work tree mounted, host uid/gid).
//! - macOS: seatbelt via `sandbox-exec` (the mechanism Homebrew uses):
//!   writes limited to the work tree and temp dirs, network denied.
//! - Windows: a SAFER "constrained" restricted token, with the work tree
//!   ACLed for the RESTRICTED SID.
//!
//! `PKGOCI_SANDBOX=0` disables sandboxing (documented escape hatch).

use std::path::Path;
use std::process::Command;

#[allow(unused_imports)]
use anyhow::{bail, Context, Result};

pub const DEFAULT_IMAGE: &str = "docker.io/library/buildpack-deps:bookworm";

pub fn enabled() -> bool {
    std::env::var("PKGOCI_SANDBOX")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Human description of the active backend (for logs).
pub fn describe(image: &str) -> String {
    if !enabled() {
        return "disabled (PKGOCI_SANDBOX=0)".into();
    }
    if cfg!(target_os = "linux") {
        format!("docker ({image}, no network)")
    } else if cfg!(target_os = "macos") {
        "seatbelt (writes limited to build dir, no network)".into()
    } else {
        "restricted token (constrained)".into()
    }
}

/// Prepare a command that runs `cmd` inside the sandbox with `work` as its
/// working directory and `env` in its environment.
pub fn command(cmd: &str, work: &Path, image: &str, env: &[(String, String)]) -> Result<Command> {
    if !enabled() {
        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        let mut c = Command::new(shell);
        c.arg(flag)
            .arg(cmd)
            .current_dir(work)
            .envs(env.iter().cloned());
        return Ok(c);
    }
    #[cfg(target_os = "linux")]
    return linux_docker(cmd, work, image, env);
    #[cfg(target_os = "macos")]
    return macos_seatbelt(cmd, work, image, env);
    #[cfg(windows)]
    return windows_restricted(cmd, work, image, env);
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    bail!("no sandbox backend for this platform (set PKGOCI_SANDBOX=0 to build unsandboxed)");
}

#[cfg(target_os = "linux")]
fn linux_docker(cmd: &str, work: &Path, image: &str, env: &[(String, String)]) -> Result<Command> {
    use std::os::unix::fs::MetadataExt;
    if Command::new("docker").arg("--version").output().is_err() {
        bail!(
            "docker is required for sandboxed builds on Linux \
             (install it, or set PKGOCI_SANDBOX=0 to build unsandboxed)"
        );
    }
    let meta = std::fs::metadata(work)?;
    let mut c = Command::new("docker");
    c.args(["run", "--rm", "--network=none"])
        .arg(format!("--user={}:{}", meta.uid(), meta.gid()))
        .arg(format!("--volume={}:/pkgoci-build", work.display()))
        .args(["--workdir=/pkgoci-build", "--env=HOME=/pkgoci-build"]);
    for (k, v) in env {
        c.arg(format!("--env={k}={v}"));
    }
    c.arg(image).args(["sh", "-c", cmd]);
    Ok(c)
}

#[cfg(target_os = "macos")]
fn macos_seatbelt(
    cmd: &str,
    work: &Path,
    _image: &str,
    env: &[(String, String)],
) -> Result<Command> {
    // Deny-by-exception profile in the style of Homebrew's build sandbox:
    // full read, writes only under the work tree and temp dirs, no network.
    let profile = format!(
        r#"(version 1)
(allow default)
(deny network*)
(deny file-write*)
(allow file-write*
    (subpath "{work}")
    (subpath "/private/tmp")
    (subpath "/private/var/tmp")
    (subpath "/private/var/folders")
    (literal "/dev/null")
    (literal "/dev/zero")
    (literal "/dev/dtracehelper")
    (regex #"^/dev/tty"))
"#,
        work = std::fs::canonicalize(work)?.display()
    );
    let mut c = Command::new("sandbox-exec");
    c.arg("-p")
        .arg(profile)
        .args(["sh", "-c", cmd])
        .current_dir(work)
        .envs(env.iter().cloned());
    Ok(c)
}

#[cfg(windows)]
fn windows_restricted(
    cmd: &str,
    work: &Path,
    _image: &str,
    env: &[(String, String)],
) -> Result<Command> {
    // Grant the RESTRICTED SID (S-1-5-12) access to the work tree so a
    // constrained-token process can build there, then run the step through
    // a helper that spawns it with a SAFER constrained token.
    let status = Command::new("icacls")
        .arg(work)
        .args(["/grant", "*S-1-5-12:(OI)(CI)F", "/T", "/Q"])
        .status()
        .context("running icacls to prepare the sandbox work tree")?;
    if !status.success() {
        bail!("icacls failed preparing the sandboxed work tree");
    }
    let mut c = Command::new(std::env::current_exe()?);
    c.arg("__sandbox-exec")
        .arg(work)
        .arg(cmd)
        .current_dir(work)
        .envs(env.iter().cloned());
    // User-profile temp dirs deny the RESTRICTED SID; point temp at the
    // (RESTRICTED-granted) work tree for the build.
    c.env("TMP", work).env("TEMP", work);
    Ok(c)
}

/// Windows-only internal entrypoint: run `cmd /C <cmd>` under a SAFER
/// "constrained" restricted token and exit with its status.
#[cfg(windows)]
pub fn windows_exec_restricted(work: &Path, cmd: &str) -> Result<i32> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, TRUE};
    use windows_sys::Win32::Security::AppLocker::{
        SaferCloseLevel, SaferComputeTokenFromLevel, SaferCreateLevel, SAFER_LEVELID_CONSTRAINED,
        SAFER_LEVEL_OPEN, SAFER_SCOPEID_USER,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, GetExitCodeProcess, WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT,
        INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
    };

    let mut level = std::ptr::null_mut();
    let ok = unsafe {
        SaferCreateLevel(
            SAFER_SCOPEID_USER,
            SAFER_LEVELID_CONSTRAINED,
            SAFER_LEVEL_OPEN,
            &mut level,
            std::ptr::null_mut(),
        )
    };
    if ok == FALSE {
        bail!("SaferCreateLevel failed");
    }
    let mut token = std::ptr::null_mut();
    let ok = unsafe {
        SaferComputeTokenFromLevel(
            level,
            std::ptr::null_mut(),
            &mut token,
            0,
            std::ptr::null_mut(),
        )
    };
    unsafe { SaferCloseLevel(level) };
    if ok == FALSE {
        bail!("SaferComputeTokenFromLevel failed");
    }

    let cmdline = format!("cmd /C {cmd}");
    let mut cmdline_w: Vec<u16> = std::ffi::OsStr::new(&cmdline)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let workdir_w: Vec<u16> = work
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessAsUserW(
            token,
            std::ptr::null(),
            cmdline_w.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            TRUE,
            CREATE_UNICODE_ENVIRONMENT,
            std::ptr::null(),
            workdir_w.as_ptr(),
            &si,
            &mut pi,
        )
    };
    unsafe { CloseHandle(token) };
    if ok == FALSE {
        bail!(
            "CreateProcessAsUserW failed (error {})",
            std::io::Error::last_os_error()
        );
    }
    let code = unsafe {
        WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        code
    };
    Ok(code as i32)
}
