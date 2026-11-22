//! Per-user install: copy the exe to `%LOCALAPPDATA%\Planor\planor.exe`, start that copy, and let it autostart
//! at sign-in through the `HKCU\...\Run` key. Autostart only ever points at the installed copy, never at a
//! development build under `target\`.
#![allow(dead_code)] // consumed by the settings window (later tasks); removed in the final cleanup
use crate::{Command, Result, client, pipe_name, wide};
use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use windows_sys::Win32::System::Registry::*;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "Planor";

/// `%LOCALAPPDATA%\Planor` (tests point it elsewhere with `PLANOR_INSTALL_DIR`).
pub fn install_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("PLANOR_INSTALL_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("missing LOCALAPPDATA")?).join("Planor"))
}
pub fn installed_exe() -> Result<PathBuf> {
    Ok(install_dir()?.join("planor.exe"))
}

fn normal(p: &Path) -> String {
    let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = p.to_string_lossy().to_lowercase().replace('/', "\\");
    s.strip_prefix(r"\\?\").unwrap_or(&s).trim_end_matches('\\').to_string()
}
/// Whether `running` is the installed copy (case, slashes and `\\?\` prefixes do not matter).
pub fn is_installed_exe(running: &Path, installed: &Path) -> bool {
    normal(running) == normal(installed)
}
pub fn running_is_installed() -> bool {
    match (std::env::current_exe(), installed_exe()) {
        (Ok(running), Ok(installed)) => is_installed_exe(&running, &installed),
        _ => false,
    }
}
/// The command stored in the Run key: the quoted exe path then `start`.
pub fn run_value(exe: &Path) -> String {
    format!("\"{}\" start", exe.to_string_lossy())
}

fn open_run_key(access: u32) -> io::Result<HKEY> {
    unsafe {
        let mut key = std::ptr::null_mut();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            access,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        );
        if status != 0 { Err(io::Error::from_raw_os_error(status as i32)) } else { Ok(key) }
    }
}
fn write_run_value(value: Option<&str>) -> Result<()> {
    unsafe {
        let key = open_run_key(KEY_SET_VALUE)?;
        let name = wide(RUN_NAME);
        let status = match value {
            Some(v) => {
                let data = wide(v);
                RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32)
            }
            None => RegDeleteValueW(key, name.as_ptr()),
        };
        RegCloseKey(key);
        if status != 0 && status != 2 {
            return Err(io::Error::from_raw_os_error(status as i32).into());
        }
        Ok(())
    }
}
fn read_run_value() -> Option<String> {
    unsafe {
        let key = open_run_key(KEY_QUERY_VALUE).ok()?;
        let name = wide(RUN_NAME);
        let (mut ty, mut len) = (0u32, 0u32);
        let mut status = RegQueryValueExW(key, name.as_ptr(), std::ptr::null(), &mut ty, std::ptr::null_mut(), &mut len);
        let mut data = vec![0u16; len as usize / 2 + 1];
        if status == 0 {
            len = (data.len() * 2) as u32;
            status = RegQueryValueExW(key, name.as_ptr(), std::ptr::null(), &mut ty, data.as_mut_ptr().cast(), &mut len);
        }
        RegCloseKey(key);
        (status == 0 && ty == REG_SZ)
            .then(|| String::from_utf16_lossy(&data[..(len as usize / 2)]).trim_end_matches('\0').to_string())
    }
}

/// Turn autostart on (pointing at the installed exe, which must exist) or off (always allowed).
pub fn set_autostart(on: bool) -> Result<()> {
    if !on {
        return write_run_value(None);
    }
    let exe = installed_exe()?;
    if !exe.exists() {
        return Err("not installed".into());
    }
    write_run_value(Some(&run_value(&exe)))
}
pub fn autostart_enabled() -> bool {
    installed_exe().ok().is_some_and(|exe| read_run_value().as_deref() == Some(run_value(&exe).as_str()))
}

/// Ask whatever daemon owns the pipe to stop and wait (up to 6 s) until the pipe is gone.
pub fn stop_daemon() -> Result<()> {
    if client(&pipe_name(), &Command::Stop).is_err() {
        return Ok(());
    }
    let until = Instant::now() + Duration::from_secs(6);
    while Instant::now() < until {
        if client(&pipe_name(), &Command::State { json: false }).is_err() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("the running Planor did not stop".into())
}

/// Copy `src` to `dest` through `dest.tmp` and rename over it, retrying while the old file is still in use.
/// A failure leaves the previous `dest` untouched.
pub fn replace_file(src: &Path, dest: &Path, attempts: u32) -> io::Result<()> {
    let tmp = dest.with_extension("exe.tmp");
    std::fs::copy(src, &tmp)?;
    let mut last = None;
    for _ in 0..attempts.max(1) {
        match std::fs::rename(&tmp, dest) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let _ = std::fs::remove_file(&tmp);
    Err(last.unwrap())
}

fn spawn_detached(exe: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(exe).arg("start").creation_flags(0x0800_0008).spawn().map(|_| ())
}

/// Install: stop the running daemon, copy this exe into the install dir, start the installed copy.
pub fn install() -> Result<PathBuf> {
    if running_is_installed() {
        return Err("already running from the install folder".into());
    }
    let dest = installed_exe()?;
    std::fs::create_dir_all(dest.parent().ok_or("bad install path")?)?;
    stop_daemon()?;
    replace_file(&std::env::current_exe()?, &dest, 20)?;
    spawn_detached(&dest)?;
    Ok(dest)
}

/// Uninstall: remove autostart, stop the daemon, delete the installed exe (and the folder when empty),
/// optionally the settings folder too.
pub fn uninstall(delete_config: bool) -> Result<()> {
    set_autostart(false)?;
    stop_daemon()?;
    let exe = installed_exe()?;
    let dir = install_dir()?;
    if running_is_installed() {
        use std::os::windows::process::CommandExt;
        // This process is the file being deleted: let a helper remove it once we have exited.
        std::process::Command::new("cmd")
            .raw_arg(format!(
                "/c ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" & rmdir \"{}\"",
                exe.display(),
                dir.display()
            ))
            .creation_flags(0x0800_0000)
            .spawn()?;
    } else {
        let _ = std::fs::remove_file(&exe);
        let _ = std::fs::remove_file(exe.with_extension("exe.tmp"));
        let _ = std::fs::remove_dir(&dir);
    }
    if delete_config && let Some(appdata) = std::env::var_os("APPDATA") {
        let _ = std::fs::remove_dir_all(PathBuf::from(appdata).join("planor"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("planor-install-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn is_installed_exe_ignores_case_and_slashes() {
        let a = Path::new(r"C:\Users\Me\AppData\Local\Planor\planor.exe");
        assert!(is_installed_exe(a, Path::new("c:/users/me/appdata/local/PLANOR/Planor.exe")));
        assert!(!is_installed_exe(a, Path::new(r"C:\repo\target\release\planor.exe")));
    }

    #[test]
    fn run_value_quotes_paths_with_spaces_and_unicode() {
        let p = PathBuf::from(r"C:\Users\Zoë Ünï\AppData\Local\Planor\planor.exe");
        assert_eq!(run_value(&p), "\"C:\\Users\\Zoë Ünï\\AppData\\Local\\Planor\\planor.exe\" start");
    }

    #[test]
    fn set_autostart_on_requires_install() {
        let d = temp_dir("noinst");
        unsafe { std::env::set_var("PLANOR_INSTALL_DIR", &d) };
        let err = set_autostart(true).unwrap_err().to_string();
        assert_eq!(err, "not installed");
        assert!(!autostart_enabled());
    }

    #[test]
    fn install_copies_via_temp_and_keeps_old_on_failure() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = temp_dir("copy");
        let (src, dest) = (d.join("new.exe"), d.join("planor.exe"));
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dest, b"old").unwrap();
        // Held open without delete sharing: the rename must fail and the old file survive.
        let held = std::fs::OpenOptions::new().read(true).share_mode(1).open(&dest).unwrap();
        assert!(replace_file(&src, &dest, 2).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
        assert!(!dest.with_extension("exe.tmp").exists(), "temp copy cleaned up");
        drop(held);
        replace_file(&src, &dest, 2).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    }
}
